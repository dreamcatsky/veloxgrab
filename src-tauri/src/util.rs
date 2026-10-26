//! 通用工具：文件名清洗。
//!
//! 之前 `engine.rs` 与 `direct.rs` 各有一份实现，长度上限（120 / 150）和行为
//! 都不一致，而且都没处理 Windows 保留设备名——`CON`、`NUL` 这类名字会让
//! `File::create` 直接失败。这里收敛成唯一实现，两边都调它。

use std::path::Path;

/// 文件名长度上限（字符数，含扩展名）。
///
/// Windows 单个路径分量上限 255 字符，但下载目录本身可能很深，留出余量。
const MAX_NAME_CHARS: usize = 150;

/// Windows 保留设备名。
///
/// 这些名字即使带扩展名也一样被拒绝（`CON.txt` 同样无法创建），
/// 所以判断时只看第一个点之前的部分。
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    "CONOUT$",
];

/// 字符串的稳定短哈希（FNV-1a 32 位）。
///
/// 用来给临时目录做命名空间：同一个 URL 重试时目录不变（续传仍有效），
/// 不同 URL 即使标题和清晰度相同也互不干扰。
pub fn short_hash(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// 清洗成在各平台都能落盘的文件名（不含目录）。
pub fn sanitize_filename(s: &str) -> String {
    let bad = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let mut out: String = s
        .chars()
        .map(|c| {
            if bad.contains(&c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();

    // Windows 会静默丢掉结尾的空格和点，留着会让「界面显示的名字」和
    // 「实际落盘的名字」不一致，后续按名字找文件就会失败。
    out = out.trim().trim_end_matches([' ', '.']).to_string();

    if out.is_empty() {
        return "download".into();
    }

    // 保留设备名加下划线前缀，保持可读性
    let stem = out.split('.').next().unwrap_or("").trim_end();
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        out = format!("_{out}");
    }

    truncate_keep_ext(&out, MAX_NAME_CHARS)
}

/// 按字符数截断，尽量保留扩展名（否则播放器/后续处理认不出格式）
fn truncate_keep_ext(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }

    let ext = Path::new(s)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    // 扩展名本身离谱地长（比如标题里的点被当成扩展名）就不保留它
    let (ext, ext_len) = if ext.chars().count() <= 20 {
        let n = ext.chars().count();
        (ext, n)
    } else {
        (String::new(), 0)
    };

    let keep = max.saturating_sub(ext_len).max(1);
    let base: String = s.chars().take(keep).collect();
    let base = base
        .trim_end_matches([' ', '.'])
        .to_string();
    let base = if base.is_empty() { "download".to_string() } else { base };
    format!("{base}{ext}")
}

/// 目录闸门的路径词法折叠：消化 `.`、就地折叠 `..`，并报告是否发生「上跳出根」。
///
/// 为什么必须有这一步：**尚未创建的目录 `canonicalize()` 会失败**，而深链
/// 传进来的 `..\..\Windows\Start Menu\Programs\Startup` 恰恰常常不存在——
/// 若因为「还没建」就跳过折叠，等于给逃逸开了正门。规则：
/// - `.` 分量直接丢弃；
/// - `..` 能吃掉前一个普通分量就吃掉（Pop 恰好只弹一个分量）；
/// - 吃无可吃（头顶是根/盘符/UNC，或已经弹空还想再弹）时**保留 `..` 分量**
///   并置 `escaped_up = true`：调用方据此判「想跳出给定的根」，宁拒不猜。
///
/// 这是纵深第一层（纯词法，可单测、不碰文件系统）；第二层在调用方：拿真实
/// 文件系统 canonicalize 最近的存在祖先再做前缀比对，兜住软链/junction 重定向。
/// 盘符与 UNC 前缀原样保留；相对路径保留相对性（相对根解析由调用方负责）。
pub fn fold_path_lexically(p: &Path) -> (std::path::PathBuf, bool) {
    use std::path::{Component, PathBuf};

    let mut out = PathBuf::new();
    let mut escaped_up = false;
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = out
                    .components()
                    .next_back()
                    .map(|c| matches!(c, Component::Normal(_)))
                    .unwrap_or(false);
                if can_pop {
                    out.pop();
                } else {
                    escaped_up = true;
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other),
        }
    }
    (out, escaped_up)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_illegal_chars_and_control_chars() {
        assert_eq!(sanitize_filename("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize_filename("bad\u{1}name"), "bad_name");
    }

    #[test]
    fn windows_reserved_names_get_prefixed() {
        // 裸保留名与带扩展名的保留名都必须处理
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("con.txt"), "_con.txt");
        assert_eq!(sanitize_filename("LPT9.mp4"), "_LPT9.mp4");
        // 只是以保留名开头不算保留名
        assert_eq!(sanitize_filename("console.log"), "console.log");
        assert_eq!(sanitize_filename("CON2"), "CON2");
    }

    #[test]
    fn trailing_spaces_and_dots_are_stripped() {
        assert_eq!(sanitize_filename("name. . "), "name");
        assert_eq!(sanitize_filename("  spaced  "), "spaced");
        // 全是点/空格时回退到默认名
        assert_eq!(sanitize_filename("..."), "download");
        assert_eq!(sanitize_filename("   "), "download");
    }

    #[test]
    fn long_names_keep_extension() {
        let long = format!("{}.mp4", "字".repeat(300));
        let out = sanitize_filename(&long);
        assert_eq!(out.chars().count(), MAX_NAME_CHARS);
        assert!(out.ends_with(".mp4"));
    }

    #[test]
    fn short_names_untouched() {
        assert_eq!(sanitize_filename("视频 标题.mp4"), "视频 标题.mp4");
    }

    #[test]
    fn short_hash_is_stable_and_distinct() {
        // 同一 URL 每次都要得到同样的目录名（否则续传失效）
        assert_eq!(short_hash("https://a.com/x"), short_hash("https://a.com/x"));
        // 不同 URL 必须分开（同名同清晰度的两个任务不能共用临时文件）
        assert_ne!(short_hash("https://a.com/x"), short_hash("https://a.com/y"));
        assert_eq!(short_hash("x").len(), 8);
    }
}
