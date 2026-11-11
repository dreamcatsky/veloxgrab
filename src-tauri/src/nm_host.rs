//! Chrome/Edge Native Messaging 宿主（`veloxgrab.exe --nm-host`）。
//!
//! ## 为什么需要它
//!
//! 扩展在 VeloxGrab 未运行时无法把任务送出去：
//! - 本地回环桥要求 GUI 进程已在监听 58943；
//! - `veloxgrab://` 协议导航要求 **transient user activation**——SW/offscreen
//!   /程序化导航一律被 Chromium 静默吞掉（2026-11-11 隔离实例三组对照实锤）；
//! - `chrome.downloads.download()` 兜底重发同样不可用（立即 USER_CANCELED）。
//!
//! IDM/FDM「点下载自动弹软件」用的正是 **Native Messaging**：浏览器自己拉起
//! 注册表里登记的宿主进程（`HKCU\Software\Google\Chrome\NativeMessagingHosts\
//! <name>` 与 Edge 对应键），**不需要任何用户手势**——这是浏览器官方留给
//! 「网页 → 本机程序」的通道。本模块让 veloxgrab.exe 以 `--nm-host` 形态
//! 成为该宿主。
//!
//! ## 协议
//!
//! stdin/stdout 上是 Chrome NM 帧式消息：4 字节小端长度 + UTF-8 JSON。
//! 扩展侧 `chrome.runtime.sendNativeMessage('com.veloxgrab.host', msg)`。
//! 宿主必须回一条 JSON 作为响应（否则扩展侧报错）。
//!
//! ## 消息流
//!
//! 扩展发来 `{url, name?, dir?, threads?, referer?}`，本进程把它转成
//! `veloxgrab://add?...` 链接，然后 **以自身 exe + 链接参数再起一个进程**：
//! - GUI 已在跑 → single_instance 插件把 argv 转给首实例并拉窗口到前台，
//!   新进程立刻退出（与用户点协议链接完全同一条通路）；
//! - GUI 没在跑 → 新进程就是冷启动，天然消费 argv 里的链接。
//!
//! 两种情况用户都会看到 VeloxGrab 窗口/另存为弹出——「自动拉起」由此成立。
//!
//! ## 安全边界
//!
//! NM 宿主只能被**注册表里白名单了本扩展 ID 的浏览器**拉起；注册表写法
//! 由安装器（NSIS，见 patch-installer.mjs 注入的 SEC_NM 段）或首启注册完成，
//! `allowed_origins` 锁定扩展 ID。stdin 消息里的字段全部经过
//! `bridge::build_link_from_json` 的同一套校验（http/https、长度、referer
//! 协议白名单），不存在从 NM 注入任意命令行的通路——链接只作为**一个 argv
//! 参数**传给自己 exe，不经过 shell。

use std::io::{Read, Write};

/// NM 宿主在注册表里的名字（与安装器写入的键一致）。
pub const NM_HOST_NAME: &str = "com.veloxgrab.host";

/// 本进程是否被浏览器以 **Native Messaging 宿主** 身份拉起。
///
/// 两个判据：
///
/// 1. `--nm-host`：我们自己显式传的（手工调试 / 集成测试用）。
/// 2. **第一个参数是调用方 origin**（`chrome-extension://<id>/`）。
///    这是浏览器实际使用的形态——它按 manifest 的 `path` 直接起进程，
///    manifest 里通常没有 `args`，于是 Chromium 在 Windows 上追加的是
///    `chrome-extension://<id>/ --parent-window=<hwnd>`，**不会**传任何
///    我们自造的标志。实测（Edge 157，2026-11-11）宿主命令行原样为：
///    `"veloxgrab.exe"  chrome-extension://<id>/ --parent-window=0`
///
/// 早期只判断 `--nm-host`，结果宿主进程掉进 `run()` 起了完整 GUI：stdout
/// 被 GUI 的日志/GBK 文本污染，浏览器等不到 NM 帧，扩展侧只拿到
/// `Error when communicating with the native messaging host.`——用户看到的
/// 就是「点了下载，浏览器自己下完」。这条判据有测试锁死。
pub fn is_nm_host(argv: &[String]) -> bool {
    if argv.iter().any(|a| a == "--nm-host") {
        return true;
    }
    // 只认**第一个**参数：浏览器的形态是 `exe <origin> [--parent-window=…]`；
    // 而协议拉起是 `exe "veloxgrab://add?..."`，两者不会混淆。
    argv.get(1)
        .map(|a| {
            let a = a.trim().trim_matches('"').to_ascii_lowercase();
            a.starts_with("chrome-extension://") || a.starts_with("moz-extension://")
        })
        .unwrap_or(false)
}

/// 一次 NM 会话处理的消息上限：扩展批量抓取的现实上限远低于此。
/// 超出视为异常客户端，直接收摊。
const MAX_MESSAGES: usize = 64;
/// 单条消息上限。与桥的 body 上限（64 KiB）对齐。
const MAX_FRAME: usize = 64 * 1024;

/// NM 模式入口：`veloxgrab.exe --nm-host`。
///
/// 永不返回调用方（处理完所有消息后直接 exit(0)）——NM 宿主进程的生命
/// 周期由浏览器管理（每个 sendNativeMessage 一个短命进程，或 connect 长连）。
pub fn run_nm_host() -> ! {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            reply_error(&format!("cannot resolve self: {e}"));
            std::process::exit(1);
        }
    };

    let mut stdin = std::io::stdin().lock();
    let mut count = 0usize;
    while count < MAX_MESSAGES {
        match read_frame(&mut stdin) {
            Ok(Some(text)) => {
                count += 1;
                // 与 HTTP 桥完全同一套字段校验/编码：veloxgrab://add?...
                let link = match crate::bridge::build_link_from_json(&text) {
                    Ok(l) => l,
                    Err(e) => {
                        reply_error(&e);
                        continue;
                    }
                };
                // 转给 GUI（已跑 = single_instance 转发；没跑 = 冷启动消费 argv）。
                // CREATE_NO_WINDOW：NM 宿主本身无窗口，子进程若弹控制台会闪黑框。
                //
                // **stdin/stdout/stderr 必须显式接管**：浏览器给本宿主开的是
                // NM 帧管道，子进程默认继承它们——GUI 的任何 stdout 输出（日志、
                // 甚至 GBK 文本）会直接灌进 NM 帧流，把回复帧搞坏；而且子进程
                // 一直持有管道写端，浏览器读完一帧后仍等不到本宿主退出，扩展侧
                // 表现为「Error when communicating with the native messaging
                // host.」。子进程走 null 是唯一安全的选择：它的输出对浏览器
                // 没有任何意义。
                let spawned = {
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        use std::process::Stdio;
                        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                        std::process::Command::new(&exe)
                            .arg(&link)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .creation_flags(CREATE_NO_WINDOW)
                            .spawn()
                    }
                    #[cfg(not(windows))]
                    {
                        use std::process::Stdio;
                        std::process::Command::new(&exe)
                            .arg(&link)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                    }
                };
                match spawned {
                    Ok(_) => reply_ok(),
                    Err(e) => reply_error(&format!("failed to start GUI: {e}")),
                }
            }
            Ok(None) => break, // stdin 关闭：浏览器结束会话
            Err(e) => {
                reply_error(&format!("bad frame: {e}"));
                break;
            }
        }
    }
    std::process::exit(0);
}

/// 读一个 NM 帧。None = 流已干净结束。
fn read_frame(r: &mut impl Read) -> Result<Option<String>, String> {
    let mut len_buf = [0u8; 4];
    // 首字节读不到 = 流关闭（正常结束）；读了 1..3 字节 = 半帧，协议错误。
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(format!("read len: {e}")),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(format!("frame length {len} out of range"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).map_err(|e| format!("read body: {e}"))?;
    String::from_utf8(buf).map_err(|e| format!("utf8: {e}")).map(Some)
}

fn reply_ok() {
    write_frame(r#"{"ok":true}"#);
}

fn reply_error(msg: &str) {
    let v = serde_json::json!({ "ok": false, "error": msg });
    write_frame(&v.to_string());
}

fn write_frame(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&(s.len() as u32).to_le_bytes());
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// 写出/更新本机 NM 宿主注册（HKCU，无需管理员）。
///
/// 每个浏览器家族一个键：Chrome 与 Edge 各自有一棵 NativeMessagingHosts
/// 树，值 = manifest JSON 路径。manifest 里锁死：
/// - `path`：本 exe 绝对路径；
/// - `type`: "stdio"；
/// - `allowed_origins`：本扩展 ID（由**当前安装路径**按 Chromium 确定性
///   算法推导——解包扩展装在哪儿 ID 就是哪个，安装路径变化时下次启动
///   自查会重写 manifest，扩展刷新后新 ID 自动进白名单）。
///
/// 什么时候调用：GUI 启动时（与协议注册同节奏）自查注册存在且 path 指向
/// 当前 exe；缺失/漂移就重写。用户移动安装目录后扩展照样能拉起新位置。
pub fn ensure_nm_registered() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let Some(ext_id) = extension_id() else {
        return Err("cannot locate extension dir".into());
    };
    let manifest = serde_json::json!({
        "name": NM_HOST_NAME,
        "description": "VeloxGrab Native Messaging host",
        "path": exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{ext_id}/")],
    });
    let manifest_path = manifest_file_path()?;
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest).unwrap())
        .map_err(|e| format!("write manifest: {e}"))?;

    for root in registry_roots() {
        set_hkcu_str(&format!(r"Software\{root}\NativeMessagingHosts\{NM_HOST_NAME}"), &manifest_path.to_string_lossy())?;
    }
    Ok(())
}

/// 扩展 ID：解包扩展的确定性 ID = SHA-256(安装路径, UTF-16LE) 的 hex 串
/// 前 32 个字符逐字符映射 0-9a-f → a-p。安装路径 = exe 同级的 extension\ 目录。
fn extension_id() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let ext_dir = exe.parent()?.join("extension");
    let s = ext_dir.to_string_lossy();
    Some(id_from_path(&s))
}

/// 与 Chromium 一致的路径→ID 算法（已在真机用两对 (路径, ID) 校准）。
/// 输入按 UTF-16LE 编码（Windows 路径的 Chromium 内部形态），SHA-256 hex
/// 前 32 字符逐字符映射 0-9a-f → a-p。哈希走系统 BCrypt（hash.rs），
/// 不为此加 sha2 依赖。
pub fn id_from_path(path: &str) -> String {
    let bytes: Vec<u8> = path.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let hash = match crate::hash::sha256_bytes(&bytes) {
        Ok(h) => h,
        Err(_) => return String::new(), // 非 Windows 测试环境；NM 仅在 Windows 有意义
    };
    hash[..16]
        .iter()
        .flat_map(|b| [(b >> 4) as usize, (b & 0xf) as usize])
        .map(|n| char::from(b'a' + n as u8))
        .collect()
}

fn manifest_file_path() -> Result<std::path::PathBuf, String> {
    let dir = dirs::config_dir().ok_or("no config dir")?;
    Ok(dir.join("VeloxGrab").join("nm-manifest.json"))
}

/// 两个浏览器家族的注册表根。写 HKCU 不需要管理员。
fn registry_roots() -> &'static [&'static str] {
    &["Google\\Chrome", "Microsoft\\Edge"]
}

fn set_hkcu_str(key: &str, value: &str) -> Result<(), String> {
    use std::process::Command;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = Command::new("reg")
            .args(["add", &format!("HKCU\\{key}"), "/ve", "/t", "REG_SZ", "/d", value, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("reg: {e}"))?;
        if !out.status.success() {
            return Err(format!("reg add {key}: {}", String::from_utf8_lossy(&out.stderr)));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (key, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两对 (路径 → ID) 都在真机浏览器里出现过（vgx-test 隔离实例与
    /// D:\Tools 安装目录），锁定 id_from_path 与 Chromium 实现一致。
    /// 任何一端算法漂移（编码、映射表、取段）都会让 NM 白名单对不上号，
    /// 浏览器侧表现为「指定host的本地主机未获允许」——极难排查，必须锁死。
    #[test]
    #[cfg(windows)]
    fn id_from_path_matches_chromium() {
        assert_eq!(
            id_from_path("C:\\Users\\Dreamcat\\AppData\\Local\\Temp\\vgx-test\\ext"),
            "calignidppfemogphocbimnfgeecfcng"
        );
        assert_eq!(
            id_from_path("D:\\Tools\\VeloxGrab\\extension"),
            "addlbhpjfbpffehjhmhfnhjpiieeebne"
        );
    }

    /// 浏览器拉起宿主的真实 argv 必须被判成 NM 模式。
    ///
    /// 这条曾经漏判：只认自造的 `--nm-host`，而 Chromium 传的是扩展
    /// origin——宿主掉进 Tauri 路径起了完整 GUI，浏览器等不到帧，扩展侧
    /// 报 "Error when communicating with the native messaging host."，
    /// 用户侧表现为「点了下载，浏览器自己下完」。
    #[test]
    fn chromium_argv_is_recognized_as_nm_host() {
        let real = vec![
            r"D:\Tools\VeloxGrab\veloxgrab.exe".to_string(),
            "chrome-extension://addlbhpjfbpffehjhmhfnhjpiieeebne/".to_string(),
            "--parent-window=0".to_string(),
        ];
        assert!(is_nm_host(&real));

        // 带引号/大写/尾随空格的变体同样要认
        let quoted = vec![
            "veloxgrab.exe".to_string(),
            "  \"CHROME-EXTENSION://abc/\"  ".to_string(),
        ];
        assert!(is_nm_host(&quoted));

        // 显式标志（手工调试）仍然有效
        assert!(is_nm_host(&[
            "veloxgrab.exe".to_string(),
            "--nm-host".to_string()
        ]));
    }

    /// 正常启动、协议拉起都**不能**被判成 NM 模式——否则用户双击图标
    /// 会得到一个没有窗口、只读 stdin 就退出的进程。
    #[test]
    fn ordinary_launch_is_not_nm_host() {
        assert!(!is_nm_host(&["veloxgrab.exe".to_string()]));
        assert!(!is_nm_host(&[
            r"D:\Tools\VeloxGrab\veloxgrab.exe".to_string(),
            "veloxgrab://add?url=https%3A%2F%2Fexample.com%2Fa.zip".to_string(),
        ]));
        // origin 出现在第二位之后不是浏览器形态（浏览器永远放第一个参数）
        assert!(!is_nm_host(&[
            "veloxgrab.exe".to_string(),
            "veloxgrab://add?url=https%3A%2F%2Fa.com%2Fb".to_string(),
            "chrome-extension://abc/".to_string(),
        ]));
    }
}
