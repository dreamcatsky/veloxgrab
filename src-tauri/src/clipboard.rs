//! 读取系统剪贴板里的文本（菜单「文件 → 从剪贴板添加URL」）。
//!
//! 只做一件事：拿到剪贴板里的 UTF-16 文本。之所以不用 `arboard` 之类的
//! 现成库，是因为整个程序只在 Windows 上发布，而这功能本质上就是三个
//! Win32 调用；多一个跨平台依赖等于多一份和窗口消息循环打交道的风险。
//!
//! 剪贴板是全局共享资源：`OpenClipboard` 可能因为别的程序正占着而失败，
//! 这不是错误而是常态（浏览器复制大段内容、剪贴板管理器轮询都会短暂占用），
//! 所以这里重试几次再放弃。

/// 读剪贴板文本。读不到返回空字符串（前端据此提示，而不是弹一个错误框）。
#[cfg(windows)]
pub fn read_clipboard_text() -> String {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    // CF_UNICODETEXT = 13。windows-sys 把它放在 Win32_System_Ole 下，
    // 为这一个常量引入整个 OLE 模块不划算，直接写字面量并在此注明。
    const CF_UNICODETEXT: u32 = 13;

    // 剪贴板被别的进程占用时 OpenClipboard 会失败。150ms 内试 5 次，
    // 覆盖绝大多数「另一程序刚复制完正在释放」的窗口。
    let mut opened = 0;
    for i in 0..5 {
        // 传 0 表示没有所有者窗口：我们不打算修改剪贴板，只是读。
        opened = unsafe { OpenClipboard(0) };
        if opened != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30 * (i + 1)));
    }
    if opened == 0 {
        return String::new();
    }

    // 从这里开始必须保证 CloseClipboard 一定被调用：剪贴板不关会让
    // 整个系统的复制粘贴都卡住，用户会以为是系统坏了。
    let text = unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            String::new()
        } else {
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle == 0 {
                String::new()
            } else {
                let ptr = GlobalLock(handle as _) as *const u16;
                if ptr.is_null() {
                    String::new()
                } else {
                    // 剪贴板里的文本是以 NUL 结尾的 UTF-16，长度只能靠扫描得到。
                    // 但扫描**不能越过全局块的实际大小**：别的进程可以放一块
                    // 没有 NUL 的小内存，按固定 1M 字符硬扫就是越界读，轻则
                    // 读到相邻内存的随机数据，重则踩到未映射页直接崩溃。
                    // GlobalSize 给出块字节数，取一半作字符上限，再夹 1M 上限。
                    let bytes = GlobalSize(handle as _) as usize;
                    let max_len = (bytes / 2).min(1_048_576);
                    let mut len = 0usize;
                    while len < max_len && *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(ptr, len);
                    let s = String::from_utf16_lossy(slice);
                    let _ = GlobalUnlock(handle as _);
                    s
                }
            }
        }
    };

    unsafe {
        let _ = CloseClipboard();
    }

    text
}

/// 非 Windows 平台没有实现，直接返回空——调用方本来就只面向 Windows。
#[cfg(not(windows))]
pub fn read_clipboard_text() -> String {
    String::new()
}

/// 把文本写进剪贴板。
///
/// 这是本程序**唯一**会修改系统共享状态的操作，所以收得很窄：只写
/// CF_UNICODETEXT，清空后只放一份，不碰其它格式（用户复制过一张图片再来点
/// 「复制链接」，不该顺手把他的图片剪贴内容丢掉）。
///
/// 内存所有权是这里最容易写错的地方：`SetClipboardData` 成功后句柄**归系统
/// 所有**，再 GlobalFree 就是让系统拿着已释放内存；只有 Set 失败时句柄还归
/// 我们，才必须自己释放。
#[cfg(windows)]
pub fn write_clipboard_text(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};

    // CF_UNICODETEXT = 13（同读路径的注记：常量在 OLE 模块，为一个数字不值得引）。
    const CF_UNICODETEXT: u32 = 13;

    // 剪贴板可能被别的程序短暂占着，重试策略与读路径一致。
    let mut opened = 0;
    for i in 0..5 {
        opened = unsafe { OpenClipboard(0) };
        if opened != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30 * (i + 1)));
    }
    if opened == 0 {
        return Err("剪贴板正被其它程序占用，稍后再试".into());
    }

    let result = unsafe { write_locked(text, CF_UNICODETEXT) };
    unsafe {
        let _ = CloseClipboard();
    }
    result
}

/// 剪贴板已打开时的写入。调用方保证随后的 CloseClipboard。
///
/// # Safety
/// 必须在 OpenClipboard 成功之后、CloseClipboard 之前调用。
#[cfg(windows)]
unsafe fn write_locked(text: &str, format: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    // UTF-16 + 结尾 NUL，按字节数分配
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = utf16.len() * std::mem::size_of::<u16>();

    // 判空用 is_null：GlobalAlloc 返回 HGLOBAL（*mut c_void），不是整数。
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if handle.is_null() {
        return Err("分配剪贴板内存失败".into());
    }
    let ptr = GlobalLock(handle);
    if ptr.is_null() {
        let _ = GlobalFree(handle);
        return Err("锁定剪贴板内存失败".into());
    }
    std::ptr::copy_nonoverlapping(utf16.as_ptr() as *const u8, ptr as *mut u8, bytes);
    let _ = GlobalUnlock(handle);

    // EmptyClipboard 只释放**被清掉的那份**旧数据，不碰我们刚分配的 handle。
    if EmptyClipboard() == 0 {
        let _ = GlobalFree(handle);
        return Err("清空剪贴板失败".into());
    }
    // SetClipboardData 收 HANDLE（isize），HGLOBAL 是裸指针，要显式转。
    if SetClipboardData(format, handle as isize) == 0 {
        // 只有 Set 失败时句柄还归我们
        let _ = GlobalFree(handle);
        return Err("写入剪贴板失败".into());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn write_clipboard_text(_text: &str) -> Result<(), String> {
    Err("本平台未实现剪贴板写入".into())
}

/// 从剪贴板文本里挑出第一个像 URL 的东西。
///
/// 用户复制的往往是「标题 + 换行 + 链接」或者一段带引号的分享文本，
/// 所以不能整段当 URL 用：逐行找第一个以 http(s):// 开头的片段。
pub fn extract_first_url(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        // 有些站点分享出来的链接被包裹在 < > 或引号里
        let line = line.trim_matches(|c| c == '<' || c == '>' || c == '"' || c == '\'');
        for token in line.split_whitespace() {
            let token = token.trim_matches(|c: char| {
                c == '<' || c == '>' || c == '"' || c == '\'' || c == ',' || c == '。'
            });
            let lower = token.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                return Some(token.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::extract_first_url;

    #[test]
    fn finds_plain_url() {
        assert_eq!(
            extract_first_url("https://example.com/a.zip"),
            Some("https://example.com/a.zip".into())
        );
    }

    #[test]
    fn finds_url_after_noise_lines() {
        let text = "分享一个文件\n标题：某某\nhttps://example.com/x.iso\n提取码 1234";
        assert_eq!(
            extract_first_url(text),
            Some("https://example.com/x.iso".into())
        );
    }

    #[test]
    fn strips_wrapping_punctuation() {
        assert_eq!(
            extract_first_url("<https://example.com/b.mp4>"),
            Some("https://example.com/b.mp4".into())
        );
        assert_eq!(
            extract_first_url("\"https://example.com/c.mp4\""),
            Some("https://example.com/c.mp4".into())
        );
    }

    #[test]
    fn returns_none_without_url() {
        assert_eq!(extract_first_url("提取码 1234"), None);
        assert_eq!(extract_first_url(""), None);
        // ftp 不算：本程序只处理 http(s)
        assert_eq!(extract_first_url("ftp://example.com/a"), None);
    }

    #[test]
    fn picks_first_of_several() {
        let text = "https://a.example/1 https://b.example/2";
        assert_eq!(
            extract_first_url(text),
            Some("https://a.example/1".into())
        );
    }
}
