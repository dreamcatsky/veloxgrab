//! 子进程输出的字节解码。
//!
//! # 为什么需要这个模块
//!
//! yt-dlp.exe 是 PyInstaller 打的独立程序（不是"系统 Python + 脚本"），
//! Windows 上**stdout/stderr 接管道时按本地 ANSI 代码页输出**——简中系统就是
//! cp936/GBK。我们曾经在 `push_common_args` 里加过 `PYTHONUTF8=1` +
//! `PYTHONIOENCODING=utf-8` 试图纠正，实测**无效**：
//!
//! ```text
//! 同一份输出，两种解码：
//!   UTF-8 → "N��֡������35% ȫ��dlss5ģ��..."
//!   GBK   → "N卡帧率提升35% 全新dlss5模型提升性能再次优化"
//! ```
//!
//! 拿 replace 兜底（`String::from_utf8_lossy`）的后果是一串 `?`：标题存进
//! history.json 就成了坏数据，之后「打开文件」「定位」「删除任务和文件」全都
//! 找不到路径（`os error 2`），而这个坏值会一直留在磁盘上。
//!
//! 所以解码策略必须分两步走，且**顺序不能反**：
//! 1. 先按严格 UTF-8 试——绝大多数 yt-dlp 输出（JSON、格式表、URL）本来就是
//!    ASCII/UTF-8，走这条不损失任何信息；
//! 2. 严格 UTF-8 失败，说明真的是本地编码，用 `MultiByteToWideChar(CP_ACP)`
//!    按**系统实际代码页**解。不能硬编码 936：英文系统是 cp1252、日文系统是
//!    cp932，写死一个只会把另一批用户坑进去。
//!
//! 两步都失败时才退回 lossy（保留了原来的"永不 panic、永不中断"性质：
//! 管道读取一旦报错，子进程写满缓冲区会卡死 `wait()`）。

/// 把子进程输出的一段字节解成字符串。
///
/// 不返回 `Result`：调用点在读管道，任何"解码失败"都不能变成 I/O 错误，
/// 否则会中断对流的管理（见上面模块注释最后一段）。
pub fn decode_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    {
        if let Some(s) = decode_ansi(bytes) {
            return s;
        }
    }
    // 走到这里是「既非 UTF-8 也非本地代码页」的字节（多半是二进制垃圾或
    // 被截断的多字节序列）。替换符比丢行好：进度解析靠行数，丢行会卡住。
    String::from_utf8_lossy(bytes).into_owned()
}

/// 按系统 ANSI 代码页（`CP_ACP`）解码。
///
/// 返回 `None` 的两种情况：
/// - 字节数超出 `i32`（不可能发生在管道行里，纯属防御）；
/// - 系统调用返回 0（该代码页下无法表示）。让调用方退回 lossy。
#[cfg(windows)]
fn decode_ansi(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};
    // CP_ACP 是常量 0，这里写字面量是为了让 `decode_ansi` 的依赖只看这一行
    // 就能读懂——`CP_ACP` 本身还需要再 import 一次，收益不值。
    const CP_ACP: u32 = 0;

    if bytes.is_empty() {
        return Some(String::new());
    }
    let len = i32::try_from(bytes.len()).ok()?;
    // 第一遍：问系统需要多少个 UTF-16 码元。这里**不带** MB_ERR_INVALID_CHARS，
    // 因为带它时非法字节会让整个转换失败（返回 0），我们就拿不到长度了。
    let need = unsafe { MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return None;
    }
    let mut wide = vec![0u16; need as usize];
    // 第二遍：真正转换。这里才带 MB_ERR_INVALID_CHARS，让"这段字节在本地代码页
    // 下也解释不通"明确失败，而不是静默换成 `?`。
    let got = unsafe {
        MultiByteToWideChar(
            CP_ACP,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            len,
            wide.as_mut_ptr(),
            need,
        )
    };
    if got <= 0 {
        return None;
    }
    wide.truncate(got as usize);
    String::from_utf16(&wide).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_utf8_pass_through() {
        assert_eq!(decode_output(b"plain ascii.mp4"), "plain ascii.mp4");
        // UTF-8 中文必须先被第一步接住，绝不能落到 ANSI 分支
        let s = "中文标题.mp4";
        assert_eq!(decode_output(s.as_bytes()), s);
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(decode_output(b""), "");
    }

    /// 回归用例：B 站视频实测的 GBK 字节，曾把标题毁成 `N????35%??dlss5?????.mp4`。
    ///
    /// 这串字节是拿 `yt-dlp --print title` 的真实输出 dump 出来的，不是手搓的
    /// ——手搓的 GBK 字节测不出"系统代码页"这条路径的行为差异。
    #[test]
    #[cfg(windows)]
    fn gbk_title_decodes_to_real_characters() {
        // "N卡帧率提升35% 全新dlss5模型提升性能再次优化" 的 cp936 编码
        let gbk: Vec<u8> = vec![
            0x4E, // N
            0xBF, 0xA8, // 卡
            0xD6, 0xA1, // 帧
            0xC2, 0xCA, // 率
            0xCC, 0xE1, // 提
            0xC9, 0xFD, // 升
            0x33, 0x35, 0x25, // 35%
            0x20, // 空格
            0xC8, 0xAB, 0xD0, 0xC2, // 全新
            0x64, 0x6C, 0x73, 0x73, 0x35, // dlss5
            0xC4, 0xA3, 0xD0, 0xCD, // 模型
        ];
        let out = decode_output(&gbk);
        // 只要落进 ANSI 分支就得是真汉字，不能有任何替换符
        assert!(!out.contains('\u{FFFD}'), "不应出现替换符: {out}");
        assert!(!out.contains('?'), "不应出现问号: {out}");
        assert!(out.contains("35%"), "ASCII 部分必须保留: {out}");
        assert!(out.contains("dlss5"), "ASCII 部分必须保留: {out}");
        assert!(out.contains('帧'), "中文必须被正确还原: {out}");
    }

    /// 两个分支都不通时，仍要返回**东西**（允许有替换符），绝不能返回空串
    /// 或 panic——调用方靠行内容解析进度，空串会把状态机卡死。
    #[test]
    fn undecodable_bytes_still_return_something() {
        let junk = [0xFFu8, 0xFE, 0x00, 0x41, 0x80];
        let out = decode_output(&junk);
        assert!(!out.is_empty(), "无解字节也必须返回非空内容");
    }
}
