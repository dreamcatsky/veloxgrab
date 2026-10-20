//! `veloxgrab://` 自定义协议（浏览器集成的落点）。
//!
//! ## 为什么需要它
//!
//! 用户最常见的入口是「网页/邮件里看到一个下载链接，点一下就想下」。
//! 没有协议接管时只能：复制链接 → 切到 VeloxGrab → 粘贴 → 点开始，四步。
//! 有了 `veloxgrab://` 之后，浏览器扩展只需要把当前页面/选中链接改成
//! `veloxgrab://add?url=...` 再导航，剩下的由本模块完成——一步。
//!
//! ## 注册方式
//!
//! Windows 上由 NSIS 安装包写入 HKCR\veloxgrab（见 `lib.rs` 里的
//! `ensure_protocol_registered`）。不用 `tauri_plugin_deep_link::register()`
//! 的原因：那个 API 在某些环境下会把自身目标写成 dev 时的 exe 路径，
//! 而安装后的路径是 `%LOCALAPPDATA%\VeloxGrab\veloxgrab.exe`，写错就静默失效。
//! 自己写注册表还能顺带把「未安装时点链接」的提示做对。
//!
//! ## 支持的形态
//!
//! ```text
//! veloxgrab://add?url=<http链接>&name=<文件名>&dir=<目录>&threads=<n>
//! veloxgrab://add?url=<...>              # 其余全部走默认
//! veloxgrab://<http链接>                 # 例如 veloxgrab://https://a.com/x.zip
//! ```
//!
//! 只接受 `http` / `https` 目标：用户从网页上复制来的东西不一定干净，
//! `file://`、`javascript:` 这类必须挡在外面。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// 让 reg.exe 不弹控制台窗口。
///
/// 启动时 `ensure_protocol_registered` 最多会起 5 个 reg（1 次 query + 4 次 add），
/// 每个都自带一个控制台窗口。不隐藏的话用户看到的就是「开软件时几个黑框闪一下」，
/// 这是本程序最容易被当成「有病毒」的细节。
///
/// 单独抽成函数而不是在每个调用点写 `#[cfg(windows)]` 块：这里一旦漏一个，
/// 现象只在真机上偶发复现，很难查。
fn hide_console(mut cmd: Command) -> Command {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 前端监听的事件名（`app.emit(EXTERNAL_LINK_EVENT, ...)`）。
pub const EXTERNAL_LINK_EVENT: &str = "external-link";

/// 协议名。注册表键与 URL scheme 都用它。
pub const SCHEME: &str = "veloxgrab";

/// 从一段 `veloxgrab://...` 文本里解析出下载请求。
///
/// 解析失败一律返回 `Err` 并由调用方 toast：用户点的是网页上的按钮，
/// 链接形态不受我们控制，安静忽略会让用户以为「没反应」而反复点击。
pub fn parse_deep_link(raw: &str) -> Result<LinkRequest, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("收到空的下载链接".into());
    }

    // 大小写不敏感：Windows 下浏览器可能按用户输入原样透传
    let lower = raw.to_ascii_lowercase();
    if !lower.starts_with(&format!("{SCHEME}://")) {
        return Err(format!("不是 {SCHEME}:// 链接：{raw}"));
    }

    let body = &raw[SCHEME.len() + 3..];
    if body.is_empty() {
        return Err("链接里没有下载地址".into());
    }

    // 形态一：veloxgrab://https://example.com/file.zip
    // 浏览器扩展对「整个 URL 就是要下载的东西」用这种形式，省去拼 query。
    let lb = body.to_ascii_lowercase();
    if lb.starts_with("http://") || lb.starts_with("https://") {
        return Ok(LinkRequest {
            url: body.to_string(),
            filename: None,
            save_dir: None,
            threads: None,
        });
    }

    // 形态二：veloxgrab://add?url=...&name=...
    // 只认 add 这一条路径：将来要加别的动作（例如 veloxgrab://settings）
    // 也得走显式分支，不能让任意单词都落到「下载」上。
    //
    // 这里必须容忍 `add/`：浏览器把手写/改造过的 veloxgrab:// 链接按 URL 规则
    // 规范化时会补一个斜杠（`veloxgrab://add?...` → `veloxgrab://add/?...`），
    // 于是朴素 split出来的 path 变成 "add/"、直接落到下面的「不支持的动作」。
    // 这是真实踩到的坑：网页里点扩展传来的链接时 100% 命中。
    let (path, query) = match body.split_once('?') {
        Some((p, q)) => (p, q),
        None => (body, ""),
    };
    let path = path.trim_end_matches('/');
    if !path.eq_ignore_ascii_case("add") {
        return Err(format!(
            "不支持的动作：{path}（可用：add）"
        ));
    }

    let mut url = None;
    let mut filename = None;
    let mut save_dir = None;
    let mut threads = None;

    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        let v = urlencoding::decode(v).unwrap_or_default().to_string();
        match k.to_ascii_lowercase().as_str() {
            "url" | "u" => url = Some(v),
            "name" | "filename" | "f" => filename = Some(v),
            "dir" | "d" => save_dir = Some(v),
            "threads" | "t" => threads = v.trim().parse::<usize>().ok(),
            _ => {}
        }
    }

    let url = url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "链接里缺少 url 参数".to_string())?;
    // 与 IPC 入口保持同一套校验：只放行 http/https。
    // 不在这里做「像不像媒体站」的判断——那是 probe_auto 的职责。
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("下载地址必须以 http:// 或 https:// 开头：{url}"));
    }

    Ok(LinkRequest {
        url,
        // 空串与缺省一视同仁
        filename: filename.filter(|f| !f.trim().is_empty()),
        save_dir: save_dir.filter(|d| !d.trim().is_empty()),
        threads: threads.map(|t| t.min(64)),
    })
}

/// 一个待入队的下载请求（来自外部链接）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkRequest {
    pub url: String,
    pub filename: Option<String>,
    pub save_dir: Option<String>,
    pub threads: Option<usize>,
}

/// 确保 HKCR\veloxgrab 指向当前可执行文件。
///
/// 由安装后的首次启动调用。写注册表需要 HKCU 写权限，安装模式是 currentUser，
/// 不需要管理员。
///
/// 已经指向本进程时就跳过：用户每次启动都重写一遍注册表毫无意义，
/// 还会让杀毒软件反复弹窗。
pub fn ensure_protocol_registered(exe: &std::path::Path) -> Result<(), String> {
    let key = format!(r"HKCU\Software\Classes\{SCHEME}");
    let current = reg_query(&key, "").ok().flatten();

    if let Some(cmd) = current {
        // 规范化后再比：注册表里的路径可能带不同的大小写或反斜杠转义
        if normalize_path(&cmd).contains(&normalize_path(&exe.to_string_lossy())) {
            return Ok(());
        }
    }

    // 带 %1 才是协议处理程序：浏览器传来的完整 URL 会替换这个占位符
    let command = format!("\"{}\" \"%1\"", exe.display());
    reg_add(&key, "", "URL:VeloxGrab Download Link")?;
    reg_add(&key, "URL Protocol", "")?;
    reg_add(&format!(r"{key}\DefaultIcon"), "", &exe.display().to_string())?;
    reg_add(
        &format!(r"{key}\shell\open\command"),
        "",
        &command,
    )?;
    Ok(())
}

/// 用系统自带 reg.exe 读写，避免引入 winreg 依赖。
///
/// 故意不用 PowerShell 的 registry provider：它在受限语言模式下不可用，
/// 且输出本地化后难以稳定解析。
fn reg_query(key: &str, value: &str) -> Result<Option<String>, String> {
    let out = hide_console(Command::new("reg"))
        .args(["query", key, "/v", value])
        .output()
        .map_err(|e| format!("reg query 失败: {e}"))?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // reg query 的输出形如:
    //     (默认)    REG_SZ    "C:\path\veloxgrab.exe" "%1"
    // 值可能为空（DefaultIcon 写的就是路径，不是空串），按「最后一个字段」取
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once("REG_SZ") {
            let v = rest.trim();
            let v = v.strip_prefix('"').unwrap_or(v);
            let v = v.strip_suffix('"').unwrap_or(v);
            if !v.is_empty() {
                return Ok(Some(v.to_string()));
            }
        }
    }
    Ok(None)
}

fn reg_add(key: &str, value: &str, data: &str) -> Result<(), String> {
    let status = hide_console(Command::new("reg"))
        .args(["add", key, "/v", value, "/d", data, "/f"])
        .status()
        .map_err(|e| format!("reg add 失败: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("写入注册表失败: {key}"))
    }
}

/// 路径规范化：小写 + 统一反斜杠，用于判断注册表是否已指向自己。
fn normalize_path(p: &str) -> String {
    p.trim().trim_matches('"').replace('/', "\\").to_ascii_lowercase()
}

/// 安装目录下的可执行文件路径（用于注册与提示文案）。
pub fn installed_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_form_with_all_fields() {
        let r = parse_deep_link(
            "veloxgrab://add?url=https%3A%2F%2Fexample.com%2Fa.zip&name=%E8%A7%86%E9%A2%91.mp4&dir=D%3A%5Cdl&threads=8",
        )
        .unwrap();
        assert_eq!(r.url, "https://example.com/a.zip");
        assert_eq!(r.filename.as_deref(), Some("视频.mp4"));
        assert_eq!(r.save_dir.as_deref(), Some("D:\\dl"));
        assert_eq!(r.threads, Some(8));
    }

    #[test]
    fn parses_bare_url_form() {
        let r = parse_deep_link("veloxgrab://https://a.com/x.zip").unwrap();
        assert_eq!(r.url, "https://a.com/x.zip");
        assert!(r.filename.is_none());
        assert!(r.threads.is_none());
    }

    #[test]
    fn bare_url_is_case_insensitive() {
        let r = parse_deep_link("VELOXGRAB://HTTPS://A.COM/x.zip").unwrap();
        assert_eq!(r.url, "HTTPS://A.COM/x.zip");
    }

    #[test]
    fn threads_are_clamped() {
        // 上限跟 direct::MAX_THREADS 一致（64）：扩展传 9999 要被钳住
        let r = parse_deep_link("veloxgrab://add?url=https://a.com/x&threads=9999").unwrap();
        assert_eq!(r.threads, Some(64));
    }

    #[test]
    fn empty_optional_fields_become_none() {
        let r = parse_deep_link("veloxgrab://add?url=https://a.com/x&name=&dir=&threads=abc").unwrap();
        assert!(r.filename.is_none());
        assert!(r.save_dir.is_none());
        assert!(r.threads.is_none());
    }

    #[test]
    fn rejects_non_http_targets() {
        // file:// 与 javascript: 必须挡住
        assert!(parse_deep_link("veloxgrab://add?url=file:///C:/Windows/x.exe").is_err());
        assert!(parse_deep_link("veloxgrab://add?url=javascript:alert(1)").is_err());
    }

    #[test]
    fn rejects_foreign_scheme_and_missing_url() {
        assert!(parse_deep_link("http://example.com").is_err());
        assert!(parse_deep_link("veloxgrab://").is_err());
        assert!(parse_deep_link("veloxgrab://add?name=x.mp4").is_err());
    }

    #[test]
    fn rejects_unknown_action() {
        let e = parse_deep_link("veloxgrab://settings?url=https://a.com/x").unwrap_err();
        assert!(e.contains("不支持的动作"), "实际错误: {e}");
    }

    #[test]
    fn tolerates_trailing_slash_after_action() {
        // 浏览器按 URL 规则规范化 veloxgrab://add?... 时会补slash，path 变成 add/
        let r = parse_deep_link("veloxgrab://add/?url=https%3A%2F%2Fa.com%2Ff.zip&threads=4").unwrap();
        assert_eq!(r.url, "https://a.com/f.zip");
        assert_eq!(r.threads, Some(4));
    }

    #[test]
    fn tolerates_url_containing_query_and_ampersand() {
        // 目标 URL 自带 query 时不能把 &id=2 当成外层参数
        let r = parse_deep_link("veloxgrab://add?url=https%3A%2F%2Fa.com%2Ff%3Fid%3D2%26x%3D1").unwrap();
        assert_eq!(r.url, "https://a.com/f?id=2&x=1");
    }

    #[test]
    fn normalize_path_ignores_case_and_slashes() {
        assert_eq!(
            normalize_path("\"C:\\Users\\A\\veloxgrab.exe\""),
            normalize_path("c:/users/a/veloxgrab.exe")
        );
    }

    // ---- 与 lib.rs 的 deep_link_from_args 配合的端到端形状测试 ----
    // 真正抓 bug 的是这条链路：注册表把 URL 作为 "%1" 传进来，
    // Windows 会带上引号，浏览器则可能原样透传（大小写、编码都不保证）。
    #[test]
    fn registry_style_argv_is_accepted() {
        // 模拟 reg_add 里写的 `"<exe>" "%1"`：链接带引号
        let argv = vec![
            r"C:\Users\me\AppData\Local\VeloxGrab\veloxgrab.exe".to_string(),
            r#""veloxgrab://add?url=https%3A%2F%2Fexample.com%2Fa.zip&threads=8""#.to_string(),
        ];
        let link = argv
            .into_iter()
            .skip(1)
            .map(|a| a.trim().trim_matches('"').to_string())
            .find(|a| a.to_ascii_lowercase().starts_with("veloxgrab://"))
            .expect("应该找到链接");
        let r = parse_deep_link(&link).unwrap();
        assert_eq!(r.url, "https://example.com/a.zip");
        assert_eq!(r.threads, Some(8));
    }

    #[test]
    fn argv_without_link_is_ignored() {
        // 用户正常双击图标启动时 argv 里没有链接，不能误触发
        let argv = vec!["veloxgrab.exe".to_string()];
        let found = argv
            .into_iter()
            .skip(1)
            .find(|a| a.to_ascii_lowercase().starts_with("veloxgrab://"));
        assert!(found.is_none());
    }
}
