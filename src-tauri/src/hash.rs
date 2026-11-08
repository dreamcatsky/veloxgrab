//! SHA-256 文件摘要（Windows BCrypt，流式读取）。
//!
//! 两个消费方：update 的安装包校验、engine 的下载完整性校验（任务的
//! sha256 字段）。从 update.rs 抽出来是因为「算一个文件的摘要」不该属于
//! 「应用内更新」这个业务模块。
//!
//! 为什么走 BCrypt 而不是 sha2 之类：windows-sys 已在依赖树里，BCrypt
//! 暴露系统首选 RNG/哈希提供程序，一个 unsafe 块换零新增依赖。

/// 流式计算文件的 SHA-256。64 KiB 块读取，句柄全程 RAII 风格收尾。
#[cfg(windows)]
pub fn sha256_file(path: &std::path::Path) -> Result<[u8; 32], String> {
    use windows_sys::Win32::Security::Cryptography as bc;

    let mut file = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut alg: bc::BCRYPT_ALG_HANDLE = std::ptr::null_mut();
    // SAFETY: 句柄在本函数内创建与释放；所有指针都指向本栈帧内的有效分配。
    unsafe {
        if bc::BCryptOpenAlgorithmProvider(&mut alg, bc::BCRYPT_SHA256_ALGORITHM, std::ptr::null(), 0) < 0 {
            return Err("初始化 SHA-256 失败".into());
        }
        // 主体包一层闭包，保证任何失败路径都会走到下面的销毁句柄
        let result = (|| {
            let mut got: u32 = 0;
            let mut obj_len: u32 = 0;
            if bc::BCryptGetProperty(
                alg,
                bc::BCRYPT_OBJECT_LENGTH,
                &mut obj_len as *mut u32 as *mut u8,
                4,
                &mut got,
                0,
            ) < 0
            {
                return Err("读取 SHA-256 参数失败".to_string());
            }
            let mut hash_len: u32 = 0;
            if bc::BCryptGetProperty(
                alg,
                bc::BCRYPT_HASH_LENGTH,
                &mut hash_len as *mut u32 as *mut u8,
                4,
                &mut got,
                0,
            ) < 0
            {
                return Err("读取 SHA-256 参数失败".to_string());
            }
            // 摘要长度写死 32 是调用方与 BCrypt 的契约，对不上说明拿错算法了
            if hash_len != 32 {
                return Err(format!("非预期的摘要长度 {hash_len}"));
            }
            let mut obj = vec![0u8; obj_len as usize];
            let mut hash: bc::BCRYPT_HASH_HANDLE = std::ptr::null_mut();
            if bc::BCryptCreateHash(alg, &mut hash, obj.as_mut_ptr(), obj_len, std::ptr::null(), 0, 0) < 0 {
                return Err("创建 SHA-256 上下文失败".to_string());
            }
            let inner = (|| {
                use std::io::Read;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = file.read(&mut buf).map_err(|e| format!("读取文件失败: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    if bc::BCryptHashData(hash, buf.as_ptr(), n as u32, 0) < 0 {
                        return Err("计算 SHA-256 失败".to_string());
                    }
                }
                let mut out = [0u8; 32];
                if bc::BCryptFinishHash(hash, out.as_mut_ptr(), 32, 0) < 0 {
                    return Err("计算 SHA-256 失败".to_string());
                }
                Ok(out)
            })();
            bc::BCryptDestroyHash(hash);
            inner
        })();
        bc::BCryptCloseAlgorithmProvider(alg, 0);
        result
    }
}

#[cfg(not(windows))]
pub fn sha256_file(_path: &std::path::Path) -> Result<[u8; 32], String> {
    Err("本平台未实现 SHA-256".into())
}

/// 十六进制摘要的小写规范化 + 合法性校验。长度不对或含非 hex 字符返回 None。
pub fn normalize_digest(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() != 64 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(t.to_ascii_lowercase())
}

/// 摘要比对：两个 64 位 hex 串按小写全等。
pub fn digest_matches(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_digest_accepts_hex64_only() {
        let ok = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(normalize_digest(ok).as_deref(), Some(ok));
        assert_eq!(normalize_digest(ok.to_uppercase().as_str()).as_deref(), Some(ok));
        assert_eq!(normalize_digest("  "), None);
        assert_eq!(normalize_digest("abc"), None);
        assert_eq!(normalize_digest(&"g".repeat(64)), None);
        assert_eq!(normalize_digest(&"a".repeat(63)), None);
    }

    #[test]
    fn digest_matches_is_case_insensitive() {
        let lower = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(digest_matches(lower, &lower.to_uppercase()));
        assert!(!digest_matches(lower, &lower.replace('a', "b")));
    }

    /// "abc" 的 SHA-256：与 update.rs 测试同一锚点，锁跨模块实现的等价性。
    #[test]
    #[cfg(windows)]
    fn sha256_of_abc() {
        let dir = std::env::temp_dir().join(format!("vg-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abc.bin");
        std::fs::write(&f, b"abc").unwrap();
        let got = sha256_file(&f).unwrap();
        assert_eq!(
            crate::dpapi::to_hex(&got),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
