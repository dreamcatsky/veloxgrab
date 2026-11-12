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
            action: None,
            filename: None,
            save_dir: None,
            threads: None,
            referer: None,
            auto: false,
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
    // launch：浏览器扩展在桌面端未运行时用来拉起主窗（不带任务）。
    // 只要求窗口到前台，不需要任何参数；队列里可能有历史暂停任务，
    // 但「自动开始下载」是用户的决定，拉起 ≠ 恢复。
    if path.eq_ignore_ascii_case("launch") {
        return Ok(LinkRequest {
            url: String::new(),
            action: Some("launch".into()),
            filename: None,
            save_dir: None,
            threads: None,
            referer: None,
            auto: false,
        });
    }
    if !path.eq_ignore_ascii_case("add") {
        return Err(format!(
            "不支持的动作：{path}（可用：add、launch）"
        ));
    }

    let mut url = None;
    let mut filename = None;
    let mut save_dir = None;
    let mut threads = None;
    let mut referer = None;
    let mut auto = false;

    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        let v = match urlencoding::decode(v) {
            Ok(s) => s.into_owned(),
            // 非法 % 序列（截断的 `%E4%B8%` 之类）按原样保留：吞成空串会让
            // 后面报「链接里缺少 url 参数」——用户明明给了，错误信息完全误导。
            Err(_) => v.to_string(),
        };
        match k.to_ascii_lowercase().as_str() {
            "url" | "u" => url = Some(v),
            "name" | "filename" | "f" => filename = Some(v),
            "dir" | "d" => save_dir = Some(v),
            "threads" | "t" => threads = v.trim().parse::<usize>().ok(),
            // 页面地址：防盗链直链需要它作为 Referer。
            // 只接受 http(s) 字面量，其它（file:/javascript:）一律丢弃——
            // 它会直接变成发往目标服务器的请求头值。
            "referer" | "ref" | "r" => referer = Some(v),
            // auto=1：扩展显式投递（桥/NM 链接由 build_link_from_json 统一
            // 注入）。前端据此跳过「另存为」面板直接入队——见 bridge.rs 里
            // 对 auto 语义的说明。手动构造的链接不带它，面板照常。
            "auto" => auto = matches!(v.as_str(), "1" | "true" | "yes" | "on"),
            _ => {}
        }
    }

    let url = url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "链接里缺少 url 参数".to_string())?;
    // 与 IPC 入口保持同一套校验：只放行 http/https。
    // 不在这里做「像不像媒体站」的判断——那是 probe_auto 的职责。
    // scheme 按 ASCII 大小写不敏感判（bare 形态已经如此）：网页里大写的
    // HTTPS://A.COM/x 是合法 URL，按原样判会被误拒，错误文案还自相矛盾。
    let url_lower = url.to_ascii_lowercase();
    if !(url_lower.starts_with("http://") || url_lower.starts_with("https://")) {
        return Err(format!("下载地址必须以 http:// 或 https:// 开头：{url}"));
    }

    Ok(LinkRequest {
        url,
        action: None,
        auto,
        // 空串与缺省一视同仁
        filename: filename.filter(|f| !f.trim().is_empty()),
        save_dir: save_dir.filter(|d| !d.trim().is_empty()),
        threads: threads.map(|t| t.min(64)),
        referer: referer
            .map(|r| r.trim().to_string())
            .filter(|r| {
                let lower = r.to_ascii_lowercase();
                lower.starts_with("http://") || lower.starts_with("https://")
            }),
    })
}

/// 一个待入队的下载请求（来自外部链接）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkRequest {
    /// 下载地址；`action = "launch"` 时为空串（那不是一个下载请求）。
    pub url: String,
    /// 动作：None = 下载（默认，兼容旧字段）；Some("launch") = 只拉起主窗。
    #[serde(default)]
    pub action: Option<String>,
    pub filename: Option<String>,
    pub save_dir: Option<String>,
    pub threads: Option<usize>,
    /// 页面地址，作为 Referer 附加到直链请求（防盗链直链需要）。
    #[serde(default)]
    pub referer: Option<String>,
    /// auto=1：扩展显式投递，前端跳过「另存为」面板直接入队。
    /// 手动构造的协议链接不带此标记（缺省 false），照常走面板。
    #[serde(default)]
    pub auto: bool,
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
        // **全等**比较而不是子串 contains：规范化后与「本程序会写出的命令」
        // 完全一致才跳过重写。旧实现用 contains——恶意软件抢注协议后把命令
        // 改成 `"<我们的exe>" "%1" & powershell -enc <payload>`，子串恰好命中，
        // 本程序的启动自检从此成为劫持的**庇护伞**（每次启动都跳过修复）。
        // 全等判据下：一致 → 不重写（杀软不弹窗）；任何篡改 → 重写为干净值
        // （自愈，这正是启动自检存在的意义）。
        let expected = format!("\"{}\" \"%1\"", exe.display());
        if normalize_path(&cmd) == normalize_path(&expected) {
            return Ok(());
        }
    }

    // 带 %1 才是协议处理程序：浏览器传来的完整 URL 会替换这个占位符
    let command = format!("\"{}\" \"%1\"", exe.display());
    reg_add(&key, "", "URL:VeloxGrab Download Link")?;
    reg_add(&key, "URL Protocol", "")?;
    // DefaultIcon 同样加引号：裸路径含空格时，逗号资源索引语法可能解析错位
    reg_add(
        &format!(r"{key}\DefaultIcon"),
        "",
        &format!("\"{}\"", exe.display()),
    )?;
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

    /// auto 标记：扩展投递的链接带 auto=1，手动构造的不带。
    /// 前端据此决定跳不跳「另存为」面板——漏解析会让拦截路径丢下载。
    #[test]
    fn auto_flag_is_parsed() {
        // 扩展投递形态（build_link_from_json 的产物总以 &auto=1 结尾）
        let r = parse_deep_link("veloxgrab://add?url=https%3A%2F%2Fa.com%2Fx.zip&auto=1").unwrap();
        assert!(r.auto, "auto=1 必须解析为 true");
        let r = parse_deep_link("veloxgrab://add?url=https%3A%2F%2Fa.com%2Fx.zip&auto=true").unwrap();
        assert!(r.auto);

        // 手动形态：缺省 = false（照常走面板）
        let r = parse_deep_link("veloxgrab://add?url=https%3A%2F%2Fa.com%2Fx.zip").unwrap();
        assert!(!r.auto);

        // 任意其它取值不视为 true（防手滑：auto=0 必须是 false）
        let r = parse_deep_link("veloxgrab://add?url=https%3A%2F%2Fa.com%2Fx.zip&auto=0").unwrap();
        assert!(!r.auto);

        // bare-url 形态永远不带 auto（手动粘贴）
        let r = parse_deep_link("veloxgrab://https://a.com/x.zip").unwrap();
        assert!(!r.auto);
    }

    /// Referer 参数（浏览器扩展带来的页面地址）：只接受 http(s)，其它丢弃。
    #[test]
    fn referer_is_parsed_and_gated() {
        let r = parse_deep_link(
            "veloxgrab://add?url=https%3A%2F%2Fcdn.example.com%2Fa.zip&referer=https%3A%2F%2Fexample.com%2Fpage",
        )
        .unwrap();
        assert_eq!(r.referer.as_deref(), Some("https://example.com/page"));

        // 非 http(s) 的 Referer（file/javascript）必须丢弃：它会变成请求头值
        for bad in ["file:///C:/x", "javascript:alert(1)", "ftp://x/y"] {
            let link = format!(
                "veloxgrab://add?url=https%3A%2F%2Fa.com%2Fx&referer={}",
                urlencoding::encode(bad)
            );
            let r = parse_deep_link(&link).unwrap();
            assert!(r.referer.is_none(), "非法 Referer 必须丢弃: {bad}");
        }
        // 缺省 / 空串都不带
        assert!(parse_deep_link("veloxgrab://add?url=https%3A%2F%2Fa.com%2Fx")
            .unwrap()
            .referer
            .is_none());
        assert!(parse_deep_link("veloxgrab://add?url=https%3A%2F%2Fa.com%2Fx&referer=")
            .unwrap()
            .referer
            .is_none());
    }

    /// launch 动作：扩展拉起桌面端用的「只唤醒」请求，不带任务。
    #[test]
    fn launch_action_parses_without_task() {
        let r = parse_deep_link("veloxgrab://launch").unwrap();
        assert_eq!(r.action.as_deref(), Some("launch"));
        assert_eq!(r.url, "", "launch 不是下载请求，url 必须为空");
        assert!(r.filename.is_none() && r.threads.is_none());

        // 浏览器规范化的补斜杠形态
        let r = parse_deep_link("veloxgrab://launch/").unwrap();
        assert_eq!(r.action.as_deref(), Some("launch"));

        // add 形态不受影响：action 缺省 = 下载
        let r = parse_deep_link("veloxgrab://add?url=https%3A%2F%2Fa.com%2Fx").unwrap();
        assert_eq!(r.action, None);
        assert_eq!(r.url, "https://a.com/x");
    }

    #[test]
    fn normalize_path_ignores_case_and_slashes() {        assert_eq!(
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
