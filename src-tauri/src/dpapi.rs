//! Windows DPAPI（CryptProtectData）：给 settings.json 里的机密做「落盘加密」。
//!
//! ## 要解决什么
//!
//! `hf_token`（HuggingFace Access Token）和 `proxy_pass`（代理密码）原先明文躺在
//! `%APPDATA%\VeloxGrab\settings.json` 里：任何以同一用户身份跑的进程（低危软件、
//! 别人临时登录你的账号、备份被拷走）都能直接读到。桌面应用做 full sandbox 不
//! 现实，但 Windows 给了现成的用户级加密——DPAPI：密文只有**同一台机器上的同一
//! 用户账户**能解开，别的机器/别的账户拿到文件也解不动。
//!
//! ## 不防什么（姿态要诚实）
//!
//! - 同用户身份运行的恶意进程仍可调用 CryptUnprotectData（DPAPI 的公开边界）；
//!   它防的是「静态文件被拷走/被顺手翻到」这一大截，不是本机提权对抗。
//! - webview 的 `get_settings` 仍拿到明文（表单要回填）；XSS 的防线是
//!   textContent 纪律与 CSP，不是这里。
//! - 加密失败（非 Windows、API 异常）时**退回明文存储**而不是拒绝保存：
//!   设置保存失败会把用户刚填的下载目录一起弄丢，比机密没加密更糟。
//!   解密失败则按「该机密不存在」处理并返回空串——绝不能把 `dpapi1:…` 密文
//!   原样拼进代理 URL 或 Authorization 头发出去。
//!
//! ## 文件格式
//!
//! 密文以 `dpapi1:<hex>` 前缀标记在**原字段**里（不加新字段，旧明文值不带前缀、
//! 原样可用），一次读盘就能区分两种世代，老用户升级零迁移。可选熵固定拼入
//! `VeloxGrab.settings.v1`：把「别的程序也拿 DPAPI 存过同一串明文」的碰撞概率
//! 从这套密文里排除出去。

/// 密文标记前缀。真 token（hf_xxx）与常规密码不会是这串开头的样子。
pub const ENC_PREFIX: &str = "dpapi1:";

/// 固定可选熵（域分隔），进 CryptProtectData 的 optional-entropy 参数。
const ENTROPY: &[u8] = b"VeloxGrab.settings.v1";

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 把明文字符串封装成落盘格式（`dpapi1:<hex>`）。
///
/// 三种原样返回（不加密、也不报错）的情形：
/// - 空串：没有机密可保护，写 `dpapi1:` 反而多一份噪音；
/// - 已经是本函数产出的格式（幂等：防止把密文再封一层）；
/// - 加密失败：宁可退回明文存储也不能让「保存设置」整个失败——保存失败会把
///   用户刚填的下载目录、代理配置一起丢掉，比机密没加密损失更大（模块头有述）。
pub fn seal(plain: &str) -> String {
    if plain.is_empty() || plain.starts_with(ENC_PREFIX) {
        return plain.to_string();
    }
    match protect(plain.as_bytes()) {
        Ok(c) => format!("{ENC_PREFIX}{}", to_hex(&c)),
        Err(e) => {
            eprintln!("[dpapi] 机密加密失败，本条退回明文存储: {e}");
            plain.to_string()
        }
    }
}

/// seal 的逆：`dpapi1:` 前缀 → 解密；无前缀（旧明文世代）原样返回。
///
/// 解密失败（换机器、换 Windows 用户、文件被改坏）返回**空串**而不是原文：
/// 密文漏进代理 URL / Authorization 头的后果，比用户重新输一遍密码严重得多。
/// 同时把坏值在内存里就地抹掉——下次 persist 会写回空串，文件里的密文垃圾
/// 自然消失。
pub fn unseal(stored: &str) -> String {
    let Some(hex) = stored.strip_prefix(ENC_PREFIX) else {
        return stored.to_string();
    };
    match from_hex(hex).and_then(|c| unprotect(&c).ok()) {
        Some(plain) => String::from_utf8_lossy(&plain).into_owned(),
        None => {
            eprintln!("[dpapi] 机密解密失败（换机器/换用户？），按未配置处理");
            String::new()
        }
    }
}
/// 十六进制解码。奇数长度、非 hex 字符都是坏数据 → None。
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// 加密任意字节 → 密文。Windows 外返回 Err（调用方负责降级）。
#[cfg(windows)]
pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::{BOOL, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    };

    if plain.len() > u32::MAX as usize {
        return Err("数据过长".into());
    }
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let ent = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // szDataDescr 只是给弹窗看的描述（我们禁用 UI），给空指针。0.52 的
    // PCWSTR 就是 *const u16 别名，null() 直接可用。
    let ok: BOOL = unsafe {
        CryptProtectData(
            &in_blob,
            std::ptr::null(),
            &ent,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err("CryptProtectData 调用失败".into());
    }
    // 文档规定输出缓冲用 LocalFree 归还
    let result = if out.pbData.is_null() || out.cbData == 0 {
        Err("CryptProtectData 返回空缓冲".to_string())
    } else {
        let slice = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
        Ok(slice.to_vec())
    };
    if !out.pbData.is_null() {
        unsafe {
            let _ = LocalFree(out.pbData as HLOCAL);
        }
    }
    result
}

/// 解密 protect 产出的密文。机器/用户不对、数据被改坏都返回 Err。
#[cfg(windows)]
pub fn unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::{BOOL, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    };

    if cipher.len() > u32::MAX as usize {
        return Err("数据过长".into());
    }
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: cipher.len() as u32,
        pbData: cipher.as_ptr() as *mut u8,
    };
    let ent = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok: BOOL = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            &ent,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err("CryptUnprotectData 失败（换了机器或换了 Windows 用户？）".into());
    }
    let result = if out.pbData.is_null() || out.cbData == 0 {
        Err("CryptUnprotectData 返回空缓冲".to_string())
    } else {
        let slice = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
        Ok(slice.to_vec())
    };
    if !out.pbData.is_null() {
        unsafe {
            let _ = LocalFree(out.pbData as HLOCAL);
        }
    }
    result
}

#[cfg(not(windows))]
pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
    Err("本平台无 DPAPI".into())
}

#[cfg(not(windows))]
pub fn unprotect(_cipher: &[u8]) -> Result<Vec<u8>, String> {
    Err("本平台无 DPAPI".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_and_rejects_bad_input() {
        let data = [0u8, 1, 0x7f, 0x80, 0xff];
        let h = to_hex(&data);
        assert_eq!(h, "00017f80ff");
        assert_eq!(from_hex(&h).unwrap(), data);
        assert_eq!(from_hex(""), Some(Vec::new()));
        assert!(from_hex("abc").is_none(), "奇数长度");
        assert!(from_hex("gg00").is_none(), "非 hex 字符");
    }

    #[cfg(windows)]
    #[test]
    fn protect_unprotect_roundtrip() {
        let secret = b"hf_S3cr3tT0kenValue123".to_vec();
        let c = protect(&secret).expect("本机 DPAPI 应当可用");
        assert_ne!(c, secret);
        // 密文要能被 from_hex 装进 dpapi1:<hex> 格式
        let wrapped = format!("{ENC_PREFIX}{}", to_hex(&c));
        let inner = wrapped.strip_prefix(ENC_PREFIX).unwrap();
        let c2 = from_hex(inner).unwrap();
        let p = unprotect(&c2).expect("同机同用户必须解得开");
        assert_eq!(p, secret);
    }

    #[cfg(windows)]
    #[test]
    fn unprotect_rejects_garbage() {
        assert!(unprotect(&[1, 2, 3, 4, 5]).is_err(), "随机字节不是合法 DPAPI blob");
    }
}
