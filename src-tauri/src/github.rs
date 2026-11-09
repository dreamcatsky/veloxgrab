//! GitHub Releases 直链解析。
//!
//! 用户粘一个仓库地址（`https://github.com/owner/repo`）时，直链探测只会拿到
//! 一个 HTML 页面，yt-dlp 也不认识——而这个地址背后往往就是「想下最新版
//! 安装包」的意图。这里把仓库地址翻译成「最新 release 里最合适的资产」，
//! 交给既有的直链下载器（多线程分块、断点续传、SHA-256 校验全部照用）。
//!
//! ## 为什么不复用 update.rs
//!
//! update.rs 只认**本仓库**、只挑 `*setup.exe`、且下载地址有白名单前缀
//! （它要执行安装包，必须严格）。这里面向任意仓库与任意资产，安全边界完全
//! 不同：只解析元数据，下载仍走用户可见的普通直链路径与 URL 闸门。
//!
//! ## 选择资产的规则
//!
//! 1. 排除源码包（GitHub 自动附加的 `Source code (zip/tar.gz)`）；
//! 2. 优先与当前平台匹配（Windows 上偏好 `.exe`/`.msi`/`.zip`，排除
//!    明显是 macOS/Linux 的 `.dmg`/`.pkg`/`.deb`/`.rpm`/`.AppImage`）；
//! 3. 同档内取体积最大的——同一个 release 里最大的那个通常是完整版而不是
//!    符号包/校验文件。
//!
//! GitHub API 匿名限流 60 次/小时，带 token 5000 次/小时（设置里可填）。

use serde::Serialize;
use std::time::Duration;

/// 一个可直接交给直链下载器的 release 资产。
#[derive(Debug, Clone, Serialize)]
pub struct ReleaseAsset {
    /// 仓库（`owner/repo`）
    pub repo: String,
    /// release 版本标签
    pub tag: String,
    /// 资产文件名
    pub filename: String,
    /// 下载地址（github.com 的 release 下载路径）
    pub download_url: String,
    /// 资产体积（字节，0 = 未知）
    pub size: u64,
    /// release 说明（markdown 原文，交前端按纯文本截断展示）
    pub notes: String,
    pub html_url: String,
    /// `sha256:<hex>`（旧 release 没有该字段时为 None）
    pub digest: Option<String>,
}

/// 从仓库页面地址里取 `(owner, repo)`。
///
/// 只认 `github.com/<owner>/<repo>`（允许尾部 `/`、`/releases`、`/releases/latest`），
/// 其它形态（issues、blob、具体的 tag 页）一律返回 None 走原有探测路径——
/// 猜错了比不猜更糟：用户粘的是文件页，我们却去下最新 release。
pub fn parse_repo_url(url: &str) -> Option<(String, String)> {
    let rest = url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim().strip_prefix("http://github.com/"))?;
    // 切成路径分量，丢掉 query/fragment
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 || parts.len() > 4 {
        return None;
    }
    // 只接受 <owner>/<repo> 或 <owner>/<repo>/releases[/latest]
    if parts.len() > 2 {
        let tail_ok = parts[2] == "releases"
            && (parts.len() == 3 || (parts.len() == 4 && parts[3] == "latest"));
        if !tail_ok {
            return None;
        }
    }
    let (owner, repo) = (parts[0], parts[1]);
    // GitHub 的 owner/repo 只含字母数字、`-`、`_`、`.`；畸形输入直接拒，
    // 免得拼出奇怪的 API 路径
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !ok(owner) || !ok(repo) {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// 资产是否像源码包（GitHub 自动附加，不是真正的下载物）。
fn is_source_archive(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    // GitHub 的自动源码包名形如 `Source code (zip)` / `Source code (tar.gz)`，
    // 末尾是括号而不是扩展名——三种后缀都要按「包含」判。
    let looks_source = n.starts_with("source code")
        || n.contains("source-code")
        || n.contains("sources")
        || n.starts_with("source_");
    looks_source && (n.contains("zip") || n.contains("tar.gz") || n.contains("tgz"))
}

/// 平台匹配分：越高越优先；负分 = 明显不适用。
fn platform_score(name: &str) -> i32 {
    let n = name.to_ascii_lowercase();
    let foreign = [".dmg", ".pkg", ".deb", ".rpm", ".appimage", ".apk", ".ipa"]
        .iter()
        .any(|e| n.ends_with(e));
    if foreign {
        return -1;
    }
    if n.ends_with(".exe") || n.ends_with(".msi") {
        3
    } else if n.ends_with(".zip") || n.ends_with(".7z") {
        2
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        1
    } else {
        0
    }
}

/// 从 `releases/latest` 的 JSON 里挑资产。纯函数，便于用真实样例测试。
pub fn pick_asset(v: &serde_json::Value, owner: &str, repo: &str) -> Option<ReleaseAsset> {
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let notes = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let html_url = v
        .get("html_url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let mut best: Option<(i32, u64, ReleaseAsset)> = None;
    for a in v.get("assets")?.as_array()? {
        let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let url = a
            .get("browser_download_url")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if name.is_empty() || url.is_empty() || is_source_archive(name) {
            continue;
        }
        let score = platform_score(name);
        if score < 0 {
            continue;
        }
        let size = a.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        let digest = a
            .get("digest")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let cand = ReleaseAsset {
            repo: format!("{owner}/{repo}"),
            tag: tag.clone(),
            filename: name.to_string(),
            download_url: url.to_string(),
            size,
            notes: notes.clone(),
            html_url: html_url.clone(),
            digest,
        };
        let better = best
            .as_ref()
            .map(|(s, sz, _)| score > *s || (score == *s && size > *sz))
            .unwrap_or(true);
        if better {
            best = Some((score, size, cand));
        }
    }
    best.map(|(_, _, a)| a)
}

/// 取某个仓库的最新 release 资产。
pub async fn fetch_latest_asset(
    owner: &str,
    repo: &str,
    token: Option<&str>,
    proxy: Option<&str>,
) -> Result<ReleaseAsset, String> {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("VeloxGrab/", env!("CARGO_PKG_VERSION")))
        .read_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(20));
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p).map_err(|e| format!("代理配置无效: {e}"))?);
    }
    let client = b.build().map_err(|e| e.to_string())?;

    let api = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let mut rb = client
        .get(&api)
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) {
        rb = rb.header("Authorization", format!("Bearer {t}"));
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| format!("无法连接 GitHub：{e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("{owner}/{repo} 没有发布 release"));
    }
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err("GitHub API 请求过于频繁（匿名限流 60 次/小时）。可在设置里填写 GitHub Token 提高额度".into());
    }
    if !status.is_success() {
        return Err(format!("GitHub 返回 {status}"));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 release 信息失败：{e}"))?;
    pick_asset(&v, owner, repo).ok_or_else(|| "该 release 里没有适合本平台的下载文件".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_url_parsing_is_narrow() {
        assert_eq!(
            parse_repo_url("https://github.com/torvalds/linux").unwrap(),
            ("torvalds".to_string(), "linux".to_string())
        );
        assert_eq!(
            parse_repo_url("https://github.com/o/r/").unwrap(),
            ("o".to_string(), "r".to_string())
        );
        assert!(parse_repo_url("https://github.com/o/r/releases").is_some());
        assert!(parse_repo_url("https://github.com/o/r/releases/latest").is_some());
        // 具体文件 / issue / tag 页不能当成仓库根
        assert!(parse_repo_url("https://github.com/o/r/issues").is_none());
        assert!(parse_repo_url("https://github.com/o/r/blob/main/x.zip").is_none());
        assert!(parse_repo_url("https://github.com/o/r/releases/tag/v1").is_none());
        // 其它站点 / 畸形输入
        assert!(parse_repo_url("https://gitlab.com/o/r").is_none());
        assert!(parse_repo_url("https://github.com/o").is_none());
        assert!(parse_repo_url("https://github.com/o/r/../../x").is_none());
        assert!(parse_repo_url("").is_none());
    }

    #[test]
    fn asset_picking_prefers_platform_and_size() {
        let v = serde_json::json!({
            "tag_name": "v2.0.0",
            "body": "notes",
            "html_url": "https://github.com/o/r/releases/tag/v2.0.0",
            "assets": [
                { "name": "Source code (zip)", "browser_download_url": "https://github.com/o/r/archive/v2.zip", "size": 10 },
                { "name": "sources.tar.gz", "browser_download_url": "https://github.com/o/r/sources.tar.gz", "size": 20 },
                { "name": "tool.dmg", "browser_download_url": "https://github.com/o/r/tool.dmg", "size": 9000 },
                { "name": "tool-setup.exe", "browser_download_url": "https://github.com/o/r/tool-setup.exe", "size": 500 },
                { "name": "tool-portable.zip", "browser_download_url": "https://github.com/o/r/tool-portable.zip", "size": 900 },
                { "name": "tool-setup-x64.exe", "browser_download_url": "https://github.com/o/r/tool-setup-x64.exe", "size": 1200,
                  "digest": "sha256:ab" }
            ]
        });
        let a = pick_asset(&v, "o", "r").expect("必须挑出资产");
        assert_eq!(a.filename, "tool-setup-x64.exe", "同档取最大");
        assert_eq!(a.size, 1200);
        assert_eq!(a.tag, "v2.0.0");
        assert_eq!(a.digest.as_deref(), Some("sha256:ab"));
        assert_eq!(a.repo, "o/r");
    }

    #[test]
    fn asset_picking_falls_back_and_handles_empty() {
        // 只有 zip：仍可下载（分数 2）
        let v = serde_json::json!({
            "tag_name": "v1",
            "assets": [
                { "name": "Source code (tar.gz)", "browser_download_url": "https://github.com/o/r/s.tar.gz", "size": 1 },
                { "name": "data.zip", "browser_download_url": "https://github.com/o/r/data.zip", "size": 42 }
            ]
        });
        assert_eq!(pick_asset(&v, "o", "r").unwrap().filename, "data.zip");

        // 只有源码包 / 只有 macOS 资产 / 完全没有资产 → None
        let only_src = serde_json::json!({
            "assets": [{ "name": "Source code (zip)", "browser_download_url": "https://github.com/o/r/s.zip", "size": 1 }]
        });
        assert!(pick_asset(&only_src, "o", "r").is_none());
        let only_mac = serde_json::json!({
            "assets": [{ "name": "app.dmg", "browser_download_url": "https://github.com/o/r/a.dmg", "size": 1 }]
        });
        assert!(pick_asset(&only_mac, "o", "r").is_none());
        let none = serde_json::json!({ "assets": [] });
        assert!(pick_asset(&none, "o", "r").is_none());
        // 形状不对不能 panic
        assert!(pick_asset(&serde_json::json!({}), "o", "r").is_none());
    }
}
