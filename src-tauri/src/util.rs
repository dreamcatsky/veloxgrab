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

/// 从 URL 里取出 host（不含 userinfo 与端口），失败返回 None。
///
/// 只做字符串层面的解析（不引入 `url` crate 到 util 层）：输入在调用方已经过
/// http/https 前缀校验。`direct` 的媒体站判定与 HuggingFace token 的域名作用域
/// 都必须以它为唯一入口——早先 `direct` 每个模块各写一份，`ytdlp` 则用
/// `contains`，正是「host 判定不统一」把 token 送给了错误的主机。
pub fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    // IPv6 字面量（[::1]:8080）不会出现在媒体站名单里，按端口截断即可
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// 是否指向 HuggingFace 主域（精确 host 或子域）。
///
/// **不能用 `url.contains("huggingface.co")`**：`https://evil.example/?x=huggingface.co`
/// 也会命中。这个函数是「要不要把 hf_token 放进 Authorization 头」的唯一判据，
/// 判宽了等于把用户的 Access Token 交给攻击者。
pub fn is_huggingface_url(url: &str) -> bool {
    host_of(url)
        .map(|h| {
            let h = h.to_ascii_lowercase();
            h == "huggingface.co" || h.ends_with(".huggingface.co")
        })
        .unwrap_or(false)
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

/// 把用户填的扩展名收敛成「小写、不带点」的规范形态。
///
/// 分类目录规则里允许用户写成 `.MP4`、`MP4`、` mp4 `，落盘前统一成 `mp4`；
/// 只保留字母数字和 `.`（复合扩展名 `tar.gz`），其余字符一律拒绝——扩展名
/// 最终会参与「文件名是否以 `.ext` 结尾」的判断，放行路径分隔符等于给规则
/// 开了一条往目录外匹配的口子。空串、超长、前后带点同样视为非法。
pub fn normalize_ext(raw: &str) -> Option<String> {
    let raw = raw.trim();
    // `..mp4` 不是「带点的扩展名」，是路径上跳的写法，直接拒绝
    if raw.starts_with("..") {
        return None;
    }
    let e = raw.trim_start_matches('.').to_ascii_lowercase();
    if e.is_empty() || e.len() > 24 {
        return None;
    }
    if e.starts_with('.') || e.ends_with('.') || e.contains("..") {
        return None;
    }
    if !e.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        return None;
    }
    Some(e)
}

/// 从文件名或 URL 推导扩展名（小写、不带点）。
///
/// 直链任务建单时还没有 Content-Disposition，只能先看用户给的文件名或 URL
/// 末段；查不到就返回 None（不分类，落回根目录）。URL 先切掉 query/fragment，
/// 否则 `file.mp4?token=...` 会把整串当成扩展名。
pub fn ext_of_name(name: &str) -> Option<String> {
    let base = name.split(['?', '#']).next().unwrap_or(name);
    let last = base.rsplit(['/', '\\']).next().unwrap_or(base);
    let (stem, ext) = last.rsplit_once('.')?;
    // 隐藏文件（`.gitignore`）或纯点结尾没有真正的扩展名
    if stem.is_empty() || stem.ends_with('.') {
        return None;
    }
    normalize_ext(ext)
}

/// 剥掉 Windows 的 verbatim（`\\?\`）前缀，还原为普通 Win32 拼写。
///
/// `std::fs::canonicalize` 在 Windows 上返回 `\\?\C:\Users\...`，UNC 返回
/// `\\?\UNC\server\share\...`。这个拼写一旦持久化（settings.json 的
/// `trusted_dirs`、任务的 `save_dir`），再与用户/设置里书写的普通路径做
/// **词法**比较时永远不相等——前缀分量 `\\?\C:` 与 `C:` 是两个不同的
/// Component，目录闸门与信任集会整体静默失配。所有「canonicalize 结果要
/// 与外部字符串比较或持久化」的场合都必须先过这里。
pub(crate) fn strip_verbatim_prefix(p: &Path) -> std::path::PathBuf {
    if !cfg!(windows) {
        return p.to_path_buf();
    }
    let s = p.as_os_str().to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        // verbatim UNC → 普通 UNC（`\\server\share`）
        let mut out = String::from(r"\\");
        out.push_str(rest);
        std::path::PathBuf::from(out)
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
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
/// 盘符与 UNC 前缀原样保留（canonicalize 的 verbatim 拼写除外——入口先剥，
/// 见 `strip_verbatim_prefix`：不剥的话持久化下来的 verbatim 串永远比不过
/// 普通拼写的根与信任集）；相对路径保留相对性（相对根解析由调用方负责）。
pub fn fold_path_lexically(p: &Path) -> (std::path::PathBuf, bool) {
    use std::path::{Component, PathBuf};

    let p = &strip_verbatim_prefix(p);

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
    fn host_of_ignores_scheme_userinfo_and_port() {
        assert_eq!(host_of("https://user:pw@a.b.com:8443/x?y#z"), Some("a.b.com"));
        assert_eq!(host_of("http://127.0.0.1:8000/f"), Some("127.0.0.1"));
        assert_eq!(host_of("https:///x"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn huggingface_host_match_is_exact_or_subdomain() {
        assert!(is_huggingface_url("https://huggingface.co/a/b/resolve/main/x"));
        assert!(is_huggingface_url("https://CDN-LFS.huggingface.co/x"));
        // 关键回归：query 里出现域名不算、相似域名不算、后缀伪造不算
        assert!(!is_huggingface_url("https://evil.example/?x=huggingface.co"));
        assert!(!is_huggingface_url("https://huggingface.co.evil.example/x"));
        assert!(!is_huggingface_url("https://nothuggingface.co/x"));
    }

    #[test]
    fn normalize_ext_strips_dots_and_rejects_paths() {
        assert_eq!(normalize_ext(".MP4").as_deref(), Some("mp4"));
        assert_eq!(normalize_ext(" 7Z ").as_deref(), Some("7z"));
        assert_eq!(normalize_ext("tar.gz").as_deref(), Some("tar.gz"));
        assert_eq!(normalize_ext("..mp4"), None);
        assert_eq!(normalize_ext("a/b"), None);
        assert_eq!(normalize_ext(""), None);
        assert_eq!(normalize_ext("."), None);
    }

    #[test]
    fn ext_of_name_handles_urls_and_hidden_files() {
        assert_eq!(ext_of_name("movie.MP4").as_deref(), Some("mp4"));
        assert_eq!(
            ext_of_name("https://e.com/a/b/file.zip?token=x#frag").as_deref(),
            Some("zip")
        );
        assert_eq!(ext_of_name("https://e.com/download").as_deref(), None);
        assert_eq!(ext_of_name(".gitignore").as_deref(), None);
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
