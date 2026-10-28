//! 下载前的磁盘空间预检。
//!
//! ## 为什么值得单独一个模块
//!
//! 下载器最常见的「半夜失败」不是网络断了，而是盘满了：多线程分块会先
//! `set_len` 预分配整个文件，空间不够时写入中途失败，用户第二天只看到
//! 一个「失败」和半个临时文件。提前一次 `GetDiskFreeSpaceExW` 就能把
//! 这件事变成「提交时明确告诉你差多少」。
//!
//! ## 为什么留 5% 余量
//!
//! 盘上还有别的程序在写（日志、浏览器缓存、系统更新），刚好卡着装满的
//! 预检等于没做。5% 对 10 GB 的文件是 500 MB，足够吸收这类波动，也不会
//! 把「其实还能下」的情况误判成失败。
//!
//! ## 拿不到容量时不拦
//!
//! 网络盘、某些虚拟盘、权限受限的目录都可能让 API 失败。这时返回 Ok：
//! 预检是**优化**，不是安全边界；为了一个查不到的容量数字拦住正常下载，
//! 比偶尔盘满更让人恼火。

use std::path::Path;

/// 预留比例（百分比）。
const HEADROOM_PCT: u64 = 5;

/// 需要多少可用空间（含 5% 余量）。
///
/// 用「原值 + 原值/20」而不是 `bytes * 105 / 100`：后者在超大值上会先溢出，
/// 而 `saturating_add` 会饱和到 u64::MAX，让「明显不够」的判断仍然成立。
pub fn required_with_headroom(bytes: u64) -> u64 {
    bytes.saturating_add(bytes / (100 / HEADROOM_PCT))
}

/// 目录所在分区的可用字节数。拿不到返回 None（调用方应放行）。
#[cfg(windows)]
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // API 要的是以 NUL 结尾的宽字符路径。目录可能还不存在，但它的盘符
    // 一定存在——`GetDiskFreeSpaceExW` 只看到盘符为止，不会因为末级目录
    // 缺失而失败。
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);

    // 第二个参数是「调用者可用」的空间：比「分区总空闲」更准确——配额
    // 和保留区都算进去了。另外两个参数用不上，传空指针。
    let mut available: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Some(available)
    } else {
        None
    }
}

#[cfg(not(windows))]
pub fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

/// 检查目录所在分区是否有空间放下 `need` 字节（含余量）。
///
/// 返回的 Err 是给用户看的中文说明，包含「需要多少 / 还剩多少 / 哪个目录」，
/// 用户据此能直接决定是清盘还是换目录。
pub fn ensure_room(dir: &Path, need: u64) -> Result<(), String> {
    if need == 0 {
        return Ok(());
    }
    let Some(free) = free_bytes(dir) else {
        return Ok(());
    };
    let required = required_with_headroom(need);
    if free < required {
        return Err(format!(
            "磁盘空间不足：需要约 {}，{} 所在分区只剩 {}。请清理空间或换一个下载目录。",
            human(required),
            dir.display(),
            human(free)
        ));
    }
    Ok(())
}

/// 人类可读的字节数（1.5 GB / 820 MB）。只用于错误提示，精度到 0.1 即可。
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::{human, required_with_headroom};

    #[test]
    fn headroom_is_five_percent() {
        assert_eq!(required_with_headroom(0), 0);
        assert_eq!(required_with_headroom(1000), 1050);
        assert_eq!(required_with_headroom(10 * 1024 * 1024 * 1024), 11_274_289_152);
    }

    #[test]
    fn headroom_saturates_instead_of_wrapping() {
        assert_eq!(required_with_headroom(u64::MAX), u64::MAX);
    }

    #[test]
    fn human_units() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
