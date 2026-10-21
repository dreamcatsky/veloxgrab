//! 应用内更新：查 GitHub Releases → 下载安装包 → 静默安装并重启。
//!
//! ## 为什么不用 tauri-plugin-updater
//!
//! 官方 updater 要求每个 release 用 minisign 私钥签名、并在仓库里维护一份
//! 固定的 update manifest——发布流程从此多出一道「钥匙」仪式：私钥丢了就
//! 永远发不了更新，构建机也得配密码环境变量。本项目是单人维护的公开工具，
//! 收益配不上这套长期负担。
//!
//! 这里走一条更直的路：GitHub Releases API 本来就是 HTTPS + JSON，
//! 下载源限定 `github.com` / `objects.githubusercontent.com` /
//! `release-assets.githubusercontent.com` 这几个官方域——能拿到安装包
//! 的信任边界和人在网页上手动点下载是同一个。安装复用 NSIS 的 `/S`
//! 静默模式，装完自动重启，用户视角就是「点一下升级，等它重开」。
//!
//! ## 安装时序上唯一的坑
//!
//! NSIS 覆盖不了正在运行的 exe（文件锁）。所以不能「启动安装程序然后退出」
//! 就完事——安装器可能在进程死透之前就去写文件，静默模式下失败得无声无息。
//! 用 `cmd /C` 串一个 2 秒延迟（ping 当计时器，比 `timeout` 在无控制台时
//! 稳）：等旧进程彻底退出 → 安装（/wait）→ 重新拉起新版本。

use std::io::Write;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Serialize;
use tauri::Emitter;

use crate::commands::AppState;
use crate::settings::load_settings;

/// 更新源。换仓库名时只需要改这一处。
pub const REPO: &str = "dreamcatsky/veloxgrab";
pub(crate) const API_LATEST: &str = "https://api.github.com/repos/dreamcatsky/veloxgrab/releases/latest";

/// 只接受这几个主机的下载地址。前端传什么都拦不住这条校验。
fn is_trusted_download(url: &str) -> bool {
    ["https://github.com/", "https://objects.githubusercontent.com/", "https://release-assets.githubusercontent.com/"]
        .iter()
        .any(|p| url.starts_with(p))
}

#[derive(Serialize)]
pub struct UpdateInfo {
    /// 有比当前新的版本
    pub available: bool,
    pub current: String,
    pub latest: String,
    pub notes: String,
    pub release_url: String,
    pub download_url: String,
    pub asset_size: u64,
    /// 检查失败的原因（网络、限流等）。前端据此决定静默还是报错。
    pub error: Option<String>,
}

/// "1.2.3" / "v1.2.3" / "1.2" → [1,2,3]；解析不了返回 None。
fn parse_ver(s: &str) -> Option<[u64; 3]> {
    let t = s.trim().trim_start_matches('v');
    let mut it = t.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = match it.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    // 预发布/构建后缀（"3-beta1"、"3+build7"）截掉再解析
    let patch_raw = match it.next() {
        Some(x) => x,
        None => "0",
    };
    let patch = patch_raw.split(['-', '+']).next()?.parse().ok()?;
    Some([major, minor, patch])
}

/// latest 严格大于 current 才算有更新。解析不动的版本串一律当「没有更新」。
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_ver(current), parse_ver(latest)) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

/// 从 GitHub releases/latest 的 JSON 组装 UpdateInfo。
/// 纯函数，单独拿出来是为了能带着真实响应样例跑测试。
pub fn parse_release(v: &serde_json::Value, current: &str) -> UpdateInfo {
    // tag 常见写法是 "v1.2.0"，前端展示时统一自己加 v，这里先剥掉，
    // 否则拼出来是 "vv1.2.0"（E2E 测试抓到过）。
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    let notes = v.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let release_url = v.get("html_url").and_then(|x| x.as_str()).unwrap_or("").to_string();

    // 资产里挑 x64 安装包：名字必须带 setup.exe，优先带 x64 的那个
    let mut chosen: Option<(String, u64, String)> = None;
    if let Some(assets) = v.get("assets").and_then(|x| x.as_array()) {
        for a in assets {
            let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let url = a.get("browser_download_url").and_then(|x| x.as_str()).unwrap_or("");
            let size = a.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
            if !name.to_ascii_lowercase().ends_with("setup.exe") || url.is_empty() {
                continue;
            }
            let better = chosen.as_ref().map_or(true, |(_, _, c)| {
                name.to_ascii_lowercase().contains("x64") && !c.to_ascii_lowercase().contains("x64")
            });
            if better {
                chosen = Some((url.to_string(), size, name.to_string()));
            }
        }
    }

    let (download_url, asset_size) = match chosen {
        Some((u, s, _)) => (u, s),
        None => (String::new(), 0),
    };

    UpdateInfo {
        available: is_newer(current, &tag) && !download_url.is_empty(),
        current: current.to_string(),
        latest: tag.to_string(),
        notes,
        release_url,
        download_url,
        asset_size,
        error: None,
    }
}

fn failed(current: &str, msg: String) -> UpdateInfo {
    UpdateInfo {
        available: false,
        current: current.to_string(),
        latest: String::new(),
        notes: String::new(),
        release_url: String::new(),
        download_url: String::new(),
        asset_size: 0,
        error: Some(msg),
    }
}

/// 带代理与 UA 的客户端构造。检查用总超时（小 JSON），下载用空闲超时（大文件）。
fn build_client(proxy: Option<&str>, total_timeout: Option<Duration>) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("VeloxGrab/", env!("CARGO_PKG_VERSION")));
    if let Some(t) = total_timeout {
        b = b.timeout(t);
    }
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p).map_err(|e| format!("代理配置无效: {e}"))?);
    }
    b.build().map_err(|e| e.to_string())
}

/// 拉取 releases/latest 并组装 UpdateInfo。失败信息塞 `error` 字段而不是 Err：
/// 启动静默检查与手动检查共用这条路径，前者失败不该弹任何东西。
pub(crate) async fn fetch_latest(proxy: Option<&str>, current: &str) -> UpdateInfo {
    let client = match build_client(proxy, Some(Duration::from_secs(15))) {
        Ok(c) => c,
        Err(e) => return failed(current, e),
    };
    let resp = match client
        .get(API_LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return failed(current, format!("无法连接 GitHub：{e}")),
    };
    let status = resp.status();
    if !status.is_success() {
        // 未认证 API 限流（60 次/小时/IP）是这里最常见的软失败
        let msg = if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            "GitHub API 请求过于频繁，稍后再试".to_string()
        } else {
            format!("GitHub 返回 {status}")
        };
        return failed(current, msg);
    }
    match resp.json().await {
        Ok(v) => parse_release(&v, current),
        Err(e) => failed(current, format!("解析版本信息失败：{e}")),
    }
}

/// 检查是否有新版本。代理设置照用——GitHub 直连不畅时，用户挂了代理就该走代理。
#[tauri::command]
pub async fn check_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let proxy = load_settings(&state).proxy_url();
    Ok(fetch_latest(proxy.as_deref(), &current).await)
}

#[derive(Clone, Serialize)]
struct Prog {
    downloaded: u64,
    total: u64,
    done: bool,
    error: Option<String>,
}

/// 下载新版本并静默安装，成功后本进程退出、装完自动重启。
///
/// 前端只会在 check_update 给出 available=true 后带着那个 download_url 调用，
/// 但这里仍然独立校验主机——命令面是公共入口，不能指望调用方守规矩。
#[tauri::command]
pub async fn update_now(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<(), String> {
    if !is_trusted_download(&url) {
        return Err("下载地址不在信任清单内".into());
    }

    let proxy = load_settings(&state).proxy_url();
    // 下载不设总超时（大文件 + 慢网），只依赖系统 TCP 超时
    let client = build_client(proxy.as_deref(), None)?;

    let dir = std::env::temp_dir().join("veloxgrab-update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let tmp = dir.join("VeloxGrab-setup.exe");
    // 清掉上次中断留下的半成品，避免续到坏文件上
    let _ = std::fs::remove_file(&tmp);

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("下载失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载失败：HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("写入临时文件失败: {e}"))?;

    let mut downloaded: u64 = 0;
    let mut last_emit = Instant::now();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| format!("下载中断：{e}"))?;
        file.write_all(&bytes).map_err(|e| format!("写入失败: {e}"))?;
        downloaded += bytes.len() as u64;
        // 200ms 节流：进度事件给状态条用，不需要每个 chunk 都发
        if last_emit.elapsed() >= Duration::from_millis(200) {
            last_emit = Instant::now();
            let _ = app.emit("update-progress", Prog { downloaded, total, done: false, error: None });
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);

    if total > 0 && downloaded < total {
        return Err(format!("下载不完整（{downloaded}/{total} 字节）"));
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // 安装链写成 .bat 落盘再执行，而不是把命令内联传给 cmd：
        // Rust 的 argv 转义用 \"，cmd 却按 "" 配对——内联引号过两道
        // 不同规则的解析器必乱（E2E 实测：同一条链手工执行正常、
        // 经 Rust spawn 后连日志重定向都没生效）。bat 文件内容由
        // Rust 直接写盘，命令行上只剩一个路径 token，没有跨语言转义。
        //
        // 延迟 5 秒等本进程退出：退出清理（杀下载子进程，上限 2 秒）+ 进程
        // 死透需要时间，NSIS 覆盖运行中的 exe 会静默失败。
        // 每步写 update.log：再出问题用户目录里能直接看到卡在哪一步。
        let log = dir.join("update.log");
        let bat = dir.join("install-chain.bat");
        let bat_body = format!(
            "@echo off\r\n\
             echo spawned> \"{log}\"\r\n\
             ping -n 6 127.0.0.1 >nul\r\n\
             echo installing>> \"{log}\"\r\n\
             \"{setup}\" /S\r\n\
             echo rc=%errorlevel%>> \"{log}\"\r\n\
             start \"\" \"{exe}\"\r\n\
             echo restarted>> \"{log}\"\r\n",
            log = log.display(),
            setup = tmp.display(),
            exe = exe.display(),
        );
        std::fs::write(&bat, bat_body).map_err(|e| format!("写安装脚本失败: {e}"))?;
        std::process::Command::new("cmd")
            .args(["/C", &bat.to_string_lossy()])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .map_err(|e| format!("启动安装程序失败: {e}"))?;
    }
    #[cfg(not(windows))]
    {
        let _ = &tmp;
        return Err("应用内更新目前仅支持 Windows".into());
    }

    let _ = app.emit("update-progress", Prog { downloaded, total, done: true, error: None });
    // 退出必须挪到独立线程：app.exit() 会**同步**派发 ExitRequested，
    // 而那个处理器里有 block_on(cancel_all)（E2E 实测：直接在 async 命令里
    // 调 app.exit，处理器跑进 tokio worker 线程，block_on 死锁，进程卡死不退出，
    // 安装链等不到文件解锁）。普通线程不在运行时上，block_on 安全。
    // 400ms 延迟给进度条收尾和事件送达留时间。
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        app.exit(0);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("1.1.1", "1.2.0"));
        assert!(is_newer("1.1.1", "v1.1.2"));
        assert!(is_newer("1.9.0", "1.10.0")); // 数字比较，不是字典序
        assert!(!is_newer("1.2.0", "1.2.0"));
        assert!(!is_newer("1.2.0", "1.1.9"));
        assert!(!is_newer("1.2.0", "not-a-version"));
        assert!(is_newer("1.2", "1.2.1")); // 两段式当 1.2.0
    }

    #[test]
    fn parses_real_release_shape() {
        // 取自 api.github.com/.../releases/latest 的真实结构（裁剪版）
        let v: serde_json::Value = serde_json::json!({
            "tag_name": "v1.1.1",
            "name": "VeloxGrab 1.1.1",
            "body": "修复：每次启动都弹引导",
            "html_url": "https://github.com/dreamcatsky/veloxgrab/releases/tag/v1.1.1",
            "assets": [
                { "name": "source.zip", "browser_download_url": "https://github.com/x/source.zip", "size": 10 },
                { "name": "VeloxGrab_1.1.1_x64-setup.exe",
                  "browser_download_url": "https://release-assets.githubusercontent.com/x/setup.exe",
                  "size": 20850601 }
            ]
        });
        let info = parse_release(&v, "1.1.0");
        assert!(info.available);
        assert_eq!(info.latest, "1.1.1"); // tag 的 v 前缀被剥掉，前端统一自己加
        assert_eq!(info.asset_size, 20850601);
        assert!(is_trusted_download(&info.download_url));
        // 当前已是最新 → 不提示
        assert!(!parse_release(&v, "1.1.1").available);
    }

    #[test]
    fn no_exe_asset_means_no_update() {
        let v: serde_json::Value = serde_json::json!({
            "tag_name": "v9.9.9", "body": "", "html_url": "", "assets": []
        });
        assert!(!parse_release(&v, "1.0.0").available);
    }

    #[test]
    fn untrusted_hosts_rejected() {
        assert!(!is_trusted_download("http://github.com/a/b")); // 必须 https
        assert!(!is_trusted_download("https://evil.example.com/setup.exe"));
        assert!(!is_trusted_download("file:///C:/windows/system32/cmd.exe"));
        assert!(is_trusted_download("https://github.com/o/r/releases/download/v1/a.exe"));
    }
}
