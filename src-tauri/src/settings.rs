use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::AppState;

/// 一条「按扩展名分目录」规则。
///
/// 只允许单层目录名（`Video`、`压缩包`），不允许 `..`、路径分隔符或盘符：
/// 分类目录永远拼在用户已批准下载根的子目录里，规则本身不能成为第二条
/// 目录闸门。扩展名统一存小写、不带点。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CategoryRule {
    pub folder: String,
    pub extensions: Vec<String>,
}

/// 出厂默认分类：覆盖最常见的五类文件。
///
/// 默认**关闭**——分类会改变文件落点，老用户升级后不该突然发现下载目录
/// 里多出一堆子文件夹。用户在设置里显式打开才生效。
pub fn default_categories() -> Vec<CategoryRule> {
    fn rule(folder: &str, exts: &[&str]) -> CategoryRule {
        CategoryRule {
            folder: folder.into(),
            extensions: exts.iter().map(|e| e.to_string()).collect(),
        }
    }
    vec![
        rule(
            "Video",
            &[
                "mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "m4v", "ts", "mpg", "mpeg", "3gp",
            ],
        ),
        rule(
            "Music",
            &["mp3", "flac", "wav", "aac", "m4a", "ogg", "opus", "wma"],
        ),
        rule(
            "Programs",
            &[
                "exe", "msi", "msix", "apk", "dmg", "pkg", "deb", "rpm", "appimage",
            ],
        ),
        rule(
            "Archives",
            &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso"],
        ),
        rule(
            "Documents",
            &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "epub", "mobi", "azw3",
            ],
        ),
    ]
}

/// 一组自定义请求头（`Name: value`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderKV {
    pub name: String,
    pub value: String,
}

/// 请求头预设：可限定只对某个 host（含子域）生效。
///
/// `host` 为空表示对所有站点生效。为了不让设置页变成「任意请求伪造器」，
/// 保存时会过滤掉 `Host` / `Range` / `Cookie` 等由下载器自己管理的头；
/// 空 host 的预设还会拒绝 `Authorization`（避免 token 跟着任意站点走）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderPreset {
    pub name: String,
    pub host: String,
    pub headers: Vec<HeaderKV>,
}

/// 持久化设置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 默认下载目录
    pub download_dir: String,
    /// 是否按扩展名把任务分到子目录（默认关闭）。
    ///
    /// 只对「没有手选保存目录」的任务生效：用户明确指定了存哪，就尊重他，
    /// 不再套分类规则。
    pub categories_enabled: bool,
    /// 分类规则；同 folder 会在保存时合并，最多 24 条。
    #[serde(default = "default_categories")]
    pub categories: Vec<CategoryRule>,
    /// 同名文件策略：keep_both（自动改名）| overwrite（覆盖旧文件）。
    ///
    /// 覆盖只对本程序为**同一 URL** 下载过、且路径完全一致的成品生效；
    /// 来源不明的同名文件仍然递增避让，避免误删用户自己的文件。
    pub name_conflict: String,
    /// 自定义请求头预设（防盗链 Referer、UA 等）。
    #[serde(default)]
    pub header_presets: Vec<HeaderPreset>,
    /// 最大并发下载数
    pub max_concurrent: usize,
    /// 默认画质偏好: best / 1080 / 720 / 480 等
    pub default_quality: String,
    /// 有 cookies 时从哪个浏览器读取: none / chrome / edge / firefox
    pub cookies_from_browser: String,
    /// 自定义 cookie 文件路径（Netscape 格式），优先于浏览器
    pub cookies_file: String,
    /// 每个视频最多同时下载的分片数
    pub concurrent_fragments: usize,
    /// 是否总是合并为 mp4
    pub merge_to_mp4: bool,

    // ---- TDM Fast 风格补充设置（对齐其「选项/设置」面板） ----
    /// 默认下载线程数（直链分块连接数），1..64
    pub default_threads: usize,
    /// 无 Access Token 时 HuggingFace 的线程上限，0 = 不限制
    pub hf_thread_cap: usize,
    /// HuggingFace Access Token（hf_xxx）
    pub hf_token: String,
    /// 嗅探到视频时弹出清晰度选择（否则自动最高画质）
    pub popup_quality: bool,
    /// 浏览器 veloxgrab:// 链接先弹「另存为」（目录/文件名/线程 → 开始下载/取消）；
    /// 关掉则静默存入默认目录
    pub popup_save_as: bool,
    /// 全局限速 KB/s，0 = 不限
    pub rate_limit_kb: u64,
    /// 开始下载时自动切到进度视图
    pub auto_progress_popup: bool,
    /// 开机自启（后台待命）
    pub autostart: bool,
    /// 匿名使用统计
    pub telemetry: bool,
    /// 后台监听剪贴板：复制到 http(s) 链接时在窗口内提示。
    ///
    /// 只提示、不自动入队：剪贴板是任意程序都能写的地方，自动下载等于把
    /// 「往剪贴板塞一条链接」变成「让 VeloxGrab 静默下载任意文件」的原语。
    pub clipboard_watch: bool,
    /// 任务完成时提醒（闪烁任务栏 + 更新托盘提示）
    pub notify_on_complete: bool,
    /// 点窗口「×」的行为：background（最小化到托盘）| ask（询问）| quit（退出）
    pub close_mode: String,
    /// 外观主题：system（跟随系统）| dark | light
    pub theme: String,
    /// 启用代理（下载与视频解析都走代理）
    pub proxy_enabled: bool,
    /// 代理类型：http | socks5
    pub proxy_type: String,
    pub proxy_host: String,
    pub proxy_port: u16,
    pub proxy_user: String,
    pub proxy_pass: String,
    /// 界面语言 zh | en（重启生效）
    pub language: String,
    /// 用户经原生确认框明确认可过的「默认下载目录之外」的保存位置（高-1 信任集）。
    ///
    /// 深链/扩展传入、且不在默认目录子树内的路径，第一次会弹 OS 级确认框；
    /// 同意后记在这里——同一目录不再重复打扰，也让「暂停/重启后继续往自己
    /// 指定的盘下载」能活过闸门。save_settings 改动 download_dir 时，旧值会
    /// 自动落进来：历史任务的目录不能因为用户换了默认目录就突然失去信任。
    #[serde(default)]
    pub trusted_dirs: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let dir = dirs::download_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("VeloxGrab")
            .to_string_lossy()
            .to_string();
        Self {
            download_dir: dir,
            categories_enabled: false,
            categories: default_categories(),
            name_conflict: "keep_both".into(),
            header_presets: Vec::new(),
            max_concurrent: 3,
            default_quality: "best".into(),
            cookies_from_browser: "none".into(),
            cookies_file: String::new(),
            concurrent_fragments: 4,
            merge_to_mp4: true,
            default_threads: 8,
            hf_thread_cap: 4,
            hf_token: String::new(),
            popup_quality: true,
            popup_save_as: true,
            rate_limit_kb: 0,
            auto_progress_popup: true,
            autostart: false,
            telemetry: false,
            clipboard_watch: true,
            notify_on_complete: true,
            close_mode: "background".into(),
            proxy_enabled: false,
            proxy_type: "http".into(),
            proxy_host: String::new(),
            proxy_port: 0,
            proxy_user: String::new(),
            proxy_pass: String::new(),
            language: "zh".into(),
            theme: "system".into(),
            trusted_dirs: Vec::new(),
        }
    }
}

impl Settings {
    /// 记住一个「用户刚在原生确认框里同意过」的目录（幂等，按字符串去重）。
    pub fn remember_trusted(&mut self, dir: &str) {
        let d = dir.trim();
        if d.is_empty() {
            return;
        }
        if !self.trusted_dirs.iter().any(|t| t == d) {
            // 上限防止被反复「同意」的目录无限撑大设置文件；溢出时丢最旧的
            if self.trusted_dirs.len() >= 32 {
                self.trusted_dirs.remove(0);
            }
            self.trusted_dirs.push(d.to_string());
        }
    }

    /// 归一化分类规则：清洗目录名、扩展名，合并同目录，限制条数。
    ///
    /// 前端表单是任意可改的输入（受损 webview 同样能提交），所以不能只靠
    /// 界面约束。规则里的 folder 最终会被 `Path::join` 到下载根上，必须在这里
    /// 挡掉 `..`、分隔符和盘符；扩展名只保留字母数字与点，避免规则把
    /// 「文件名是否以 .ext 结尾」的判断带偏。
    pub fn normalize_categories(&mut self) {
        const MAX_RULES: usize = 24;
        const MAX_EXTS: usize = 64;

        let mut out: Vec<CategoryRule> = Vec::new();
        for rule in std::mem::take(&mut self.categories) {
            let Some(folder) = normalize_category_folder(&rule.folder) else {
                continue;
            };
            let mut exts: Vec<String> = Vec::new();
            for raw in rule.extensions {
                let Some(e) = crate::util::normalize_ext(&raw) else {
                    continue;
                };
                if exts.len() >= MAX_EXTS {
                    break;
                }
                if !exts.iter().any(|x| x == &e) {
                    exts.push(e);
                }
            }
            if exts.is_empty() {
                continue;
            }
            // Windows 目录名大小写不敏感，`Video` 与 `video` 视为同一目录
            if let Some(existing) = out
                .iter_mut()
                .find(|r| r.folder.eq_ignore_ascii_case(&folder))
            {
                for e in exts {
                    if existing.extensions.len() >= MAX_EXTS {
                        break;
                    }
                    if !existing.extensions.iter().any(|x| x == &e) {
                        existing.extensions.push(e);
                    }
                }
            } else {
                out.push(CategoryRule {
                    folder,
                    extensions: exts,
                });
                if out.len() >= MAX_RULES {
                    break;
                }
            }
        }
        self.categories = out;
    }

    /// 按扩展名查分类子目录；未启用、无扩展名或没命中时返回 None。
    ///
    /// 命中顺序即规则顺序（先到先得），所以保存时合并同目录不会改变
    /// 「同一个扩展名归哪个目录」的既有语义。
    pub fn category_folder_for(&self, ext: Option<&str>) -> Option<String> {
        if !self.categories_enabled {
            return None;
        }
        let ext = crate::util::normalize_ext(ext?)?;
        self.categories
            .iter()
            .find(|r| {
                r.extensions
                    .iter()
                    .any(|e| crate::util::normalize_ext(e).as_deref() == Some(ext.as_str()))
            })
            // 纵深防御：即便规则来自未归一化的旧设置，返回前再清洗一次目录名
            .and_then(|r| normalize_category_folder(&r.folder))
    }

    /// 归一化请求头预设：校验头名/头值、过滤危险头、限制条数。
    ///
    /// 请求头是直接拼进 HTTP 请求的原始文本：头名里的空格/冒号会让解析器
    /// 把后半段当成新头，头值里的 CRLF 就是标准的响应/请求拆分注入。这里
    /// 只放行 RFC 7230 的 token 字符，值里出现控制字符直接丢弃整条。
    pub fn normalize_header_presets(&mut self) {
        const MAX_PRESETS: usize = 8;
        const MAX_HEADERS: usize = 16;

        let mut out: Vec<HeaderPreset> = Vec::new();
        for (i, preset) in std::mem::take(&mut self.header_presets)
            .into_iter()
            .enumerate()
        {
            if out.len() >= MAX_PRESETS {
                break;
            }
            let Some(host) = normalize_header_host(&preset.host) else {
                continue;
            };
            let name = {
                let n = preset.name.trim();
                if n.is_empty() {
                    format!("预设 {}", i + 1)
                } else {
                    n.chars().take(48).collect()
                }
            };
            let mut headers: Vec<HeaderKV> = Vec::new();
            for kv in preset.headers {
                let Some(name) = normalize_header_name(&kv.name) else {
                    continue;
                };
                let Some(value) = normalize_header_value(&kv.value) else {
                    continue;
                };
                if is_blocked_header(&name) {
                    continue;
                }
                // 空 host = 对所有站点生效，此时不允许携带 Authorization
                if host.is_empty() && name.eq_ignore_ascii_case("authorization") {
                    continue;
                }
                if headers.len() >= MAX_HEADERS {
                    break;
                }
                if headers.iter().any(|h| h.name.eq_ignore_ascii_case(&name)) {
                    continue;
                }
                headers.push(HeaderKV { name, value });
            }
            if headers.is_empty() {
                continue;
            }
            out.push(HeaderPreset {
                name,
                host,
                headers,
            });
        }
        self.header_presets = out;
    }

    /// 取目标 URL 应携带的自定义头（按 host 作用域过滤，后出现的预设覆盖同名头）。
    ///
    /// 返回值直接喂给 reqwest / yt-dlp。这里再做一次校验是纵深防御：
    /// settings.json 可能被手改，不能假设它一定经过 `save_settings`。
    pub fn headers_for_url(&self, url: &str) -> Vec<(String, String)> {
        let Some(host) = crate::util::host_of(url) else {
            return Vec::new();
        };
        let host = host.to_ascii_lowercase();
        let mut out: Vec<(String, String)> = Vec::new();
        for p in &self.header_presets {
            let scope = p.host.trim().to_ascii_lowercase();
            if !scope.is_empty() && host != scope && !host.ends_with(&format!(".{scope}")) {
                continue;
            }
            for kv in &p.headers {
                let Some(name) = normalize_header_name(&kv.name) else {
                    continue;
                };
                let Some(value) = normalize_header_value(&kv.value) else {
                    continue;
                };
                if is_blocked_header(&name) {
                    continue;
                }
                if scope.is_empty() && name.eq_ignore_ascii_case("authorization") {
                    continue;
                }
                if let Some(slot) = out.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                    slot.1 = value;
                } else {
                    out.push((name, value));
                }
            }
        }
        out
    }

    /// save_settings 专用：默认下载目录发生变更时，把**旧值**记为已信任。
    ///
    /// 没有这一步的话：用户以前经确认框同意过目录 X 并把默认改成 X，之后把
    /// 默认挪去别处，X 里还有历史任务——重启回灌后那些任务的「重下/清理」
    /// 会被闸门拦下，用户视角就是「我选过的盘突然不能用了」。
    pub fn absorb_replaced_root(&mut self, old_root: &str) {
        let o = old_root.trim();
        if !o.is_empty() && o != self.download_dir {
            self.remember_trusted(o);
        }
    }
}

/// 清洗请求头预设的 host 作用域。空串表示「对所有站点生效」。
///
/// 只接受裸主机名（可带子域）：协议、端口、路径、通配符一律拒绝，避免
/// 「看起来像 example.com，实际匹配到别处」的作用域歧义。`*.example.com`
/// 与 `.example.com` 都按 `example.com` 处理（子域匹配本来就包含它）。
fn normalize_header_host(raw: &str) -> Option<String> {
    let mut h = raw.trim().to_ascii_lowercase();
    if h.is_empty() {
        return Some(String::new());
    }
    h = h
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .to_string();
    if h.is_empty() || h.len() > 253 {
        return None;
    }
    if h.contains(['/', ':', '?', '#', '@', ' ', '\\']) {
        return None;
    }
    if !h
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return None;
    }
    if h.starts_with('.') || h.ends_with('.') || h.contains("..") {
        return None;
    }
    Some(h)
}

/// 清洗请求头名：只放行 RFC 7230 的 token 字符。
///
/// 头名里出现空格/冒号/控制字符时，HTTP 解析器会把后半段当成新的头——
/// 那等于让设置页变成任意请求伪造器。这里不做大小写转换（保留用户写法），
/// 比较时统一用 `eq_ignore_ascii_case`。
fn normalize_header_name(raw: &str) -> Option<String> {
    let n = raw.trim();
    if n.is_empty() || n.len() > 64 {
        return None;
    }
    if !n.bytes().all(is_http_token_byte) {
        return None;
    }
    Some(n.to_string())
}

/// 清洗请求头值：拒绝 CR/LF/NUL 及其它控制字符（防请求拆分注入）。
///
/// 空值直接丢弃：用户在表单里留一行空 `Name:` 通常是误操作，发出去只会
/// 让服务端困惑。值里的首尾空白按 HTTP 语义没有意义，统一 trim。
fn normalize_header_value(raw: &str) -> Option<String> {
    let v = raw.trim();
    if v.is_empty() || v.len() > 4096 {
        return None;
    }
    if v.chars().any(|c| c.is_control() && c != '\t') {
        return None;
    }
    Some(v.to_string())
}

/// RFC 7230 `token` 允许的字节。
fn is_http_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// 由下载器自己管理、绝不允许用户预设覆盖的头。
///
/// 这些头直接决定请求的语义：`Range` 决定分块边界、`Content-Length` 参与
/// 续传校验、`Host` 决定连接目标、`Cookie` 属于现有 Cookie 机制。放行它们
/// 轻则下坏文件，重则把请求指向别的主机。
fn is_blocked_header(name: &str) -> bool {
    const BLOCKED: &[&str] = &[
        "host",
        "content-length",
        "content-range",
        "range",
        "if-range",
        "connection",
        "cookie",
        "set-cookie",
        "transfer-encoding",
        "content-encoding",
        "accept-encoding",
        "expect",
        "upgrade",
        "te",
        "trailer",
        "proxy-connection",
        "proxy-authorization",
    ];
    BLOCKED.iter().any(|b| name.eq_ignore_ascii_case(b))
}

/// 请求头名是否属于「不该跟着跨站重定向跑」的敏感身份。
///
/// reqwest 只会自动剥掉 Authorization/Cookie/Proxy-Authorization；像
/// `X-Api-Key` 这类自定义凭证它会原样带到下一跳。直链侧用这个判定
/// 「有敏感头就不自动跨站跳转」，宁可报错也不把 token 交给第三方 CDN。
fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "authorization"
        || n == "proxy-authorization"
        || n == "cookie"
        || n.contains("token")
        || n.contains("secret")
        || n.contains("api-key")
        || n.contains("apikey")
        || n.contains("session")
}

/// 清洗一条规则的目录名，非法时返回 None。
///
/// 只允许**单层**目录：分隔符、盘符、`.` / `..` 一律拒绝，剩下 Windows
/// 非法字符交给 `sanitize_filename` 统一替换（它还会处理保留设备名和结尾
/// 的点/空格）。这样 `Path::join` 的结果永远只会是下载根下的一级子目录。
fn normalize_category_folder(raw: &str) -> Option<String> {
    let f = raw.trim();
    if f.is_empty() || f == "." || f == ".." {
        return None;
    }
    if f.contains(['/', '\\', ':']) {
        return None;
    }
    let cleaned = crate::util::sanitize_filename(f);
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned)
}

/// 把用户填的代理主机规范成 `reqwest::Proxy` 能吃的 `host` / `[v6]`。
///
/// 兼容三种常见粘贴形态：`host`、`host:port`（端口以输入框为准）、
/// `http://user:pass@host:port/`。返回 None 表示确实是非法输入。
fn normalize_proxy_host(raw: &str) -> Option<String> {
    let mut h = raw.trim();
    let lower = h.to_ascii_lowercase();
    for p in ["http://", "https://", "socks5h://", "socks5://"] {
        if lower.starts_with(p) {
            h = &h[p.len()..];
            break;
        }
    }
    h = h.split(['/', '?', '#']).next().unwrap_or("");
    if let Some((_, rest)) = h.rsplit_once('@') {
        h = rest;
    }

    let mut host = if h.starts_with('[') {
        // `[v6]` / `[v6]:port`：只取到右方括号
        let end = h.find(']')?;
        h[..=end].to_string()
    } else {
        let colons = h.matches(':').count();
        if colons > 1 {
            // 裸 IPv6（`::1`、`fe80::1`）：冒号是地址的一部分，不是端口分隔符
            h.to_string()
        } else if colons == 1 {
            // 单冒号只有一种合法解释：端口。端口值以 proxy_port 输入框为准。
            let (name, port) = h.rsplit_once(':')?;
            if name.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            name.to_string()
        } else {
            h.to_string()
        }
    };
    if host.is_empty() {
        return None;
    }
    // 裸 IPv6（`::1`）→ `[::1]`；带方括号的保持原样
    if host.contains(':') && !host.starts_with('[') {
        host = format!("[{host}]");
    }
    // 白名单字符：主机名 / IPv4 / [IPv6]。有别的字符直接判非法，
    // 不给「拼进 URL 再让解析器兜底」留空间。
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return None;
    }
    Some(host)
}

fn settings_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("VeloxGrab").join("settings.json")
}

pub fn load_settings(state: &AppState) -> Settings {
    if let Ok(guard) = state.settings.lock() {
        return guard.clone();
    }
    Settings::default()
}

pub fn read_settings_from_disk() -> Settings {
    let path = settings_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Settings::default(),
    };
    // 显式标 Settings：下面第一步就是字段读写（unseal 赋 hf_token），
    // 不钉类型的话编译器无从反推 s 的形状。
    match serde_json::from_str::<Settings>(&text) {
        Ok(mut s) => {
            // 先把盘上的机密解回明文——内存里的 Settings 一律以明文形态存在，
            // 下游（proxy_url 拼 URL、hf_token_trimmed 进 Authorization 头、
            // 前端表单回填）都不必知道加密这件事。旧明文字段不带前缀，
            // unseal 原样返回，新老世代一次读盘兼容。
            s.hf_token = crate::dpapi::unseal(&s.hf_token);
            s.proxy_pass = crate::dpapi::unseal(&s.proxy_pass);
            // 设置文件同样按不可信输入处理：旧版本/手改的规则可能带 `..` 或
            // 分隔符，进内存前先清洗，避免它绕过 save_settings 的归一化。
            s.normalize_categories();
            s.name_conflict = normalize_name_conflict(&s.name_conflict).to_string();
            // 请求头预设同样按不可信输入处理：手改的 settings.json 可能带
            // CRLF 头值或 `Host` 覆盖，进内存前先过一遍白名单。
            s.normalize_header_presets();
            // 产品改名（VGrab → VeloxGrab）的遗留路径迁移。老用户 settings.json
            // 里存的默认下载目录还是 `...\Downloads\VGrab`——用户显式设置过的值
            // 优先于出厂默认，不迁移的话他们会一直往旧名目录里写。
            if migrate_legacy_download_dir(&mut s) {
                persist_migrated(&s);
            }
            s
        }
        Err(e) => {
            // 不要把坏文件直接留在原地让下次继续失败，也不要静默覆盖掉：
            // 改名备份，用户至少还能从里面找回自己填过的路径。备份名带时间戳
            // （与 history 同规则）：连续两次损坏时固定名会把第一次的遗骸顶掉。
            let stamp = crate::commands::now_ms();
            let bad = path.with_extension(format!("json.bad-{stamp}"));
            let _ = std::fs::rename(&path, &bad);
            eprintln!(
                "settings.json 解析失败（{e}），已备份到 {}，本次使用默认设置",
                bad.display()
            );
            Settings::default()
        }
    }
}

/// 把「末段恰好是 VGrab」的下载目录改成 VeloxGrab，返回是否发生了迁移。
///
/// 判据刻意收窄到路径末段的精确匹配：那是旧版默认目录的确切指纹
/// （`<下载>\VGrab`）。用户自己起的含 VGrab 的名字（比如
/// `D:\MyVGrabStuff`）不在此列，不碰。只改设置里的路径字符串，
/// 不搬目录里的文件——旧目录里可能有已下载的内容，程序无权挪动
/// 用户文件；新目录会在下次下载时自动创建。
pub(crate) fn migrate_legacy_download_dir(s: &mut Settings) -> bool {
    let trimmed = s.download_dir.trim();
    if trimmed.is_empty() {
        return false;
    }
    let p = std::path::Path::new(trimmed);
    if p.file_name().and_then(|x| x.to_str()) != Some("VGrab") {
        return false;
    }
    let Some(parent) = p.parent() else { return false };
    s.download_dir = parent.join("VeloxGrab").to_string_lossy().to_string();
    true
}

/// 迁移结果写回磁盘（原子写，与 save_settings 同一套路）。
/// 写失败不致命：内存里已经是新值，本次运行正确；下次启动再迁一次即可（幂等）。
fn persist_migrated(s: &Settings) {
    let _ = persist_settings(s);
}

impl Settings {
    /// 组装 reqwest / yt-dlp 可用的代理 URL，未启用或填得不全时返回 None。
    ///
    /// 用户填的 user/pass 必须 percent-encode：密码里出现 `@`、`:`、`/`
    /// （企业代理很常见）时，直接拼 `user:pass@host` 会让 URL 解析器把
    /// 后半段当成主机——轻则认证失败，重则把请求发去错误的主机。
    pub fn proxy_url(&self) -> Option<String> {
        if !self.proxy_enabled {
            return None;
        }
        let host = normalize_proxy_host(&self.proxy_host)?;
        if self.proxy_port == 0 {
            return None;
        }
        // socks5h：让代理端解析域名，配合「加速被墙站点」的初衷（本地 DNS 往往拿不到正确结果）
        let scheme = if self.proxy_type == "socks5" { "socks5h" } else { "http" };
        let user = self.proxy_user.trim();
        let auth = if user.is_empty() {
            String::new()
        } else {
            format!(
                "{}:{}@",
                urlencoding::encode(user),
                urlencoding::encode(self.proxy_pass.trim())
            )
        };
        Some(format!("{scheme}://{auth}{host}:{}", self.proxy_port))
    }

    /// 去空白后的 HuggingFace token，空则 None。
    pub fn hf_token_trimmed(&self) -> Option<&str> {
        let t = self.hf_token.trim();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    }
}

/// 一个「只在指定 host（含子域）上附加」的请求头。
///
/// 需要 host 作用域的原因：HuggingFace 的 `Authorization: Bearer …` 是给
/// huggingface.co 的凭证，而直链下载器会跟随重定向到 CDN，也会被同一任务里的
/// 其它请求复用。reqwest 默认会在跨 host 重定向时剥掉 Authorization
/// （`remove_sensitive_headers`），但**初始请求**仍然必须由我们自己判断 host——
/// 把 token 发给 `evil.example` 只差一次 `contains`。
#[derive(Debug, Clone, Default)]
pub struct HostAuth {
    /// 允许携带该头的 host（精确匹配或子域匹配）
    pub host: String,
    /// 完整头值（如 `Bearer hf_xxx`）
    pub value: String,
}

impl HostAuth {
    /// 目标 URL 的 host 命中作用域时返回头值。
    pub fn for_url(&self, url: &str) -> Option<&str> {
        let host = crate::util::host_of(url)?.to_ascii_lowercase();
        let want = self.host.trim().to_ascii_lowercase();
        if want.is_empty() {
            return None;
        }
        if host == want || host.ends_with(&format!(".{want}")) {
            Some(&self.value)
        } else {
            None
        }
    }
}

/// 网络相关设置的快照，传给直链下载器（direct 模块不需要整个 Settings）。
#[derive(Debug, Clone, Default)]
pub struct NetCfg {
    pub proxy: Option<String>,
    pub rate_kb: u64,
    /// 需要附加 Authorization 的请求（目前只有 HuggingFace）
    pub auth: Option<HostAuth>,
    /// 命中当前 host 作用域的自定义请求头（已通过白名单校验）
    pub custom: Vec<(String, String)>,
}

impl NetCfg {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            proxy: s.proxy_url(),
            rate_kb: s.rate_limit_kb,
            auth: None,
            custom: Vec::new(),
        }
    }

    /// 与 `from_settings` 相同，但按目标 URL 决定是否附加 HF token 与自定义头。
    ///
    /// 调用方必须在**知道 URL** 的地方用它（探测、直链下载）；不知道 URL 的
    /// 地方（更新检查等）只能用 `from_settings`，以免把凭证带进无关请求。
    pub fn for_url(s: &Settings, url: &str) -> Self {
        let mut net = Self::from_settings(s);
        if let Some(tok) = s.hf_token_trimmed() {
            if crate::util::is_huggingface_url(url) {
                net.auth = Some(HostAuth {
                    host: "huggingface.co".into(),
                    value: format!("Bearer {tok}"),
                });
            }
        }
        net.custom = s.headers_for_url(url);
        net
    }

    /// 这批自定义头里是否含有不能跟着跨站重定向跑的身份凭证。
    pub fn has_sensitive_headers(&self) -> bool {
        self.custom.iter().any(|(n, _)| is_sensitive_header(n))
    }
}

/// 开机自启：写/删 HKCU\...\Run 键。复用系统 reg.exe，与 deeplink.rs 同一套路，
/// 不引入 winreg 依赖。失败由调用方决定是否提示——自启开关失败不该挡住整个保存。
pub fn apply_autostart(enable: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let status = if enable {
            let cmd = format!("\"{}\"", exe.display());
            Command::new("reg")
                .args(["add", key, "/v", "VeloxGrab", "/d", &cmd, "/f"])
                .creation_flags(0x0800_0000)
                .status()
        } else {
            Command::new("reg")
                .args(["delete", key, "/v", "VeloxGrab", "/f"])
                .creation_flags(0x0800_0000)
                .status()
        }
        .map_err(|e| format!("reg 失败: {e}"))?;
        // 删除时键本来就不存在也算成功（用户从没开过自启）
        if enable && !status.success() {
            return Err("写入开机自启失败".into());
        }
    }
    let _ = enable;
    Ok(())
}

/// 出厂默认设置，供前端「恢复默认」按钮取用（不落盘，用户点保存才写）。
/// 下载目录例外：默认值是「系统下载目录/VeloxGrab」，不能像 serde::default 那样
/// 固化成 "."，否则恢复默认会把用户导到程序目录里去。
#[tauri::command]
pub fn default_settings() -> Settings {
    Settings {
        download_dir: default_download_dir(),
        ..Settings::default()
    }
}

/// 默认下载目录：系统「下载」文件夹下的 VeloxGrab，拿不到时退回当前目录。
pub(crate) fn default_download_dir() -> String {
    dirs::download_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("VeloxGrab")
        .to_string_lossy()
        .to_string()
}

/// 落盘专用：把机密字段封成 DPAPI 密文，返回**新对象**，不改调用方的内存值。
///
/// 内存里的 Settings 永远是明文（proxy_url / hf_token_trimmed 拼请求头要用，
/// 前端表单回填也要用），加密只发生在写磁盘的最后一刻。
fn sealed_for_disk(s: &Settings) -> Settings {
    let mut d = s.clone();
    d.hf_token = crate::dpapi::seal(&d.hf_token);
    d.proxy_pass = crate::dpapi::seal(&d.proxy_pass);
    d
}

/// 原子写入 settings.json 的唯一入口。
///
/// 三个写者（save_settings / set_theme / 迁移回写 persist_migrated）原先各自摊开
/// 「write 同一个 settings.json.tmp → rename」。同名临时文件 + 无串行化 = 并发
/// 时 A 的半截内容会被 B 的 rename 抢去顶替，产出坏 JSON；下次启动解析失败，
/// 下载目录/Cookie 路径/代理配置整份报废。这里加进程内互斥，并把临时文件名
/// 带上序号（万一将来又有第四个写者、或跨进程场景，也不会互相覆盖半成品）。
///
/// 用 std Mutex 而不是 tokio：三个写者都是同步命令线程，锁里只有本地磁盘 IO。
fn persist_settings(s: &Settings) -> Result<(), String> {
    static SETTINGS_IO: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let path = settings_path();
    let text = serde_json::to_string_pretty(&sealed_for_disk(s)).map_err(|e| e.to_string())?;

    // 中毒也要继续写：上一个持有者 panic 不代表这次写盘会坏，让配置保存沉默
    // 才是更大的损失。锁的粒度是「一次完整写盘」，panic 只会留下一个孤儿 .tmp。
    let _guard = SETTINGS_IO.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp-{seq}"));
    let r = std::fs::write(&tmp, &text).map_err(|e| format!("写入设置失败: {e}"));
    if r.is_ok() {
        // fsync 后再 rename：崩溃时不留「目录项已更新、内容还在页缓存」的空文件。
        // 必须用**可写**句柄：Windows 的 FlushFileBuffers 对只读句柄直接失败，
        // 旧写法 `File::open`（只读）+ sync_data 等于什么都没做。
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&tmp) {
            let _ = f.sync_all();
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("保存设置失败: {e}"))?;
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

#[tauri::command]
pub fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    load_settings(&state)
}

/// 把「用户刚在原生确认框里同意过」的目录写入内存与磁盘（闸门放行后调用）。
///
/// 落盘失败只影响「下次启动要不要再问一遍」，不推翻本次放行，所以返回 ()。
pub(crate) fn remember_trusted_dir(state: &AppState, dir: &str) {
    let snapshot = {
        let Ok(mut guard) = state.settings.lock() else { return };
        guard.remember_trusted(dir);
        guard.clone()
    };
    if let Err(e) = persist_settings(&snapshot) {
        eprintln!("[settings] 信任目录落盘失败: {e}");
    }
}

#[tauri::command]
pub fn save_settings(
    state: tauri::State<'_, AppState>,
    mut settings: Settings,
) -> Result<Settings, String> {
    // 边界钳制，避免前端传入离谱值
    settings.max_concurrent = settings.max_concurrent.clamp(1, 8);
    settings.concurrent_fragments = settings.concurrent_fragments.clamp(1, 64);
    settings.default_threads = settings.default_threads.clamp(1, 64);
    settings.hf_thread_cap = settings.hf_thread_cap.min(64);
    if settings.proxy_type != "socks5" {
        settings.proxy_type = "http".into();
    }
    if settings.language != "en" {
        settings.language = "zh".into();
    }
    settings.theme = normalize_theme(&settings.theme).to_string();
    settings.close_mode = normalize_close_mode(&settings.close_mode).to_string();
    settings.name_conflict = normalize_name_conflict(&settings.name_conflict).to_string();
    // 分类规则来自前端表单：清洗目录名/扩展名、合并同目录、限制条数。
    settings.normalize_categories();
    // 请求头预设：头名/头值白名单、危险头过滤、条数上限。
    settings.normalize_header_presets();

    // 信任集只允许两个来源：原生确认框的同意（remember_trusted_dir）、以及
    // 「用户把默认目录从 A 改到 B」时旧值 A 的自动继承。**前端表单里带来的
    // trusted_dirs 一律丢弃**——闸门的设计前提就是「外部输入不得自授信任」，
    // 若这里照收，受损 webview 只要往表单里塞一句 `"trusted_dirs":["C:\\win"]`
    // 就把整个高-1 闸门旁路掉了。以**后端现值**为基准合并：确认框攒下的信任
    // 不能被一次「保存设置」抹掉，否则用户改个主题都要重新被问一遍目录。
    let (old_root, old_trusted) = match state.settings.lock() {
        Ok(guard) => (guard.download_dir.clone(), guard.trusted_dirs.clone()),
        // 中毒时按「没有旧值」处理：宁可重新问一遍，不凭空造信任
        Err(_) => (String::new(), Vec::new()),
    };
    settings.trusted_dirs = old_trusted;
    settings.absorb_replaced_root(&old_root);

    // 原子写统一走 persist_settings（进程内串行 + 独立临时文件名）：
    // 与 set_theme / 迁移回写共用一把闸，杜绝同名 .tmp 互踩。
    // 注意：absorb 只进内存，persist 失败时盘上仍是旧值——重启后以盘为准，
    // 最坏结果是多弹一次确认框，不影响正确性。
    persist_settings(&settings)?;

    if let Ok(mut guard) = state.settings.lock() {
        *guard = settings.clone();
    }
    // 开机自启跟着保存走：失败不回滚设置（设置本身已落盘），只打日志。
    if let Err(e) = apply_autostart(settings.autostart) {
        eprintln!("[settings] 开机自启设置失败: {e}");
    }
    Ok(settings)
}

/// 把主题值收敛到三个合法取值之一，认不出的按「跟随系统」处理。
///
/// 不返回错误而是静默收敛：主题是纯外观配置，为了它让整个「保存设置」失败，
/// 把用户刚改好的下载目录一起拒掉，代价完全不成比例。
pub(crate) fn normalize_theme(t: &str) -> &'static str {
    match t.trim() {
        "light" => "light",
        "dark" => "dark",
        _ => "system",
    }
}

/// 关闭行为的合法取值。认不出的一律回落到 `background`——它是升级前的老行为，
/// 误判成 `quit` 会让「关窗继续下载」的用户丢任务。
pub(crate) fn normalize_close_mode(m: &str) -> &'static str {
    match m.trim() {
        "ask" => "ask",
        "quit" => "quit",
        _ => "background",
    }
}

/// 同名文件策略的合法取值。认不出的按 keep_both（最安全）处理。
pub(crate) fn normalize_name_conflict(v: &str) -> &'static str {
    if v.trim().eq_ignore_ascii_case("overwrite") {
        "overwrite"
    } else {
        "keep_both"
    }
}

/// 只改主题一个字段。
///
/// 为什么不复用 `save_settings`：那会把前端表单里**当前**的每一项都写一遍，
/// 包括正被编辑但还没保存的下载目录、代理密码。用户这边刚选了「浅色」，
/// 那边手滑改了一半的表单就被顺手落盘了——这种「保存了我不想保存的东西」
/// 是最难察觉也最难撤回的一类 bug。主题要的是「立刻生效且只动主题」，
/// 所以走独立的窄接口：读盘 → 改一个字段 → 原地写回。
#[tauri::command]
pub fn set_theme(state: tauri::State<'_, AppState>, theme: String) -> Result<String, String> {
    let normalized = normalize_theme(&theme).to_string();

    let mut current = load_settings(&state);
    current.theme = normalized.clone();

    // 与 save_settings 同一把闸、同一套原子写：主题是高频改动项（用户会来回切着
    // 看效果），截断的 settings.json 会让整份配置报废。
    persist_settings(&current)?;

    if let Ok(mut guard) = state.settings.lock() {
        guard.theme = normalized.clone();
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_dir(d: &str) -> Settings {
        Settings {
            download_dir: d.to_string(),
            ..Settings::default()
        }
    }

    #[test]
    fn migrates_legacy_vgrab_default_dir() {
        let mut s = with_dir(r"C:\Users\me\Downloads\VGrab");
        assert!(migrate_legacy_download_dir(&mut s));
        assert_eq!(s.download_dir, r"C:\Users\me\Downloads\VeloxGrab");
    }

    #[test]
    fn migrates_regardless_of_case_and_separator() {
        let mut s = with_dir("C:/Users/me/Downloads/VGrab");
        assert!(migrate_legacy_download_dir(&mut s));
        assert!(s.download_dir.replace('\\', "/").ends_with("Downloads/VeloxGrab"));
    }

    #[test]
    fn leaves_user_chosen_dirs_alone() {
        // 用户自己起的含 VGrab 的名字不碰：末段不是精确的 "VGrab"
        let mut s = with_dir(r"D:\MyVGrabStuff");
        assert!(!migrate_legacy_download_dir(&mut s));
        assert_eq!(s.download_dir, r"D:\MyVGrabStuff");
        // 已经是新名 → 幂等，不重复迁移
        let mut t = with_dir(r"C:\Users\me\Downloads\VeloxGrab");
        assert!(!migrate_legacy_download_dir(&mut t));
        // 空目录（= 用出厂默认）不碰
        let mut u = with_dir("  ");
        assert!(!migrate_legacy_download_dir(&mut u));
    }

    fn proxy(host: &str, user: &str, pass: &str) -> Option<String> {
        Settings {
            proxy_enabled: true,
            proxy_host: host.into(),
            proxy_port: 1080,
            proxy_user: user.into(),
            proxy_pass: pass.into(),
            ..Settings::default()
        }
        .proxy_url()
    }

    #[test]
    fn proxy_credentials_are_percent_encoded() {
        // 密码里的 @ : / 不编码就会让解析器把后半段当主机
        assert_eq!(
            proxy("127.0.0.1", "me@corp", "p@ss:w/rd").unwrap(),
            "http://me%40corp:p%40ss%3Aw%2Frd@127.0.0.1:1080"
        );
    }

    #[test]
    fn proxy_host_accepts_pasted_url_and_ipv6() {
        assert_eq!(
            proxy("http://u:p@10.0.0.2:8080/x", "", "").unwrap(),
            "http://10.0.0.2:1080"
        );
        assert_eq!(proxy("[::1]", "", "").unwrap(), "http://[::1]:1080");
        assert_eq!(proxy("::1", "", "").unwrap(), "http://[::1]:1080");
        // 空格 / 端口非数字 → 不拼出脏 URL
        assert!(proxy("bad host", "", "").is_none());
        assert!(proxy("host:abc", "", "").is_none());
    }

    #[test]
    fn name_conflict_falls_back_to_keep_both() {
        assert_eq!(normalize_name_conflict("overwrite"), "overwrite");
        assert_eq!(normalize_name_conflict(" OVERWRITE "), "overwrite");
        assert_eq!(normalize_name_conflict("keep_both"), "keep_both");
        // 认不出的值不能变成破坏性模式
        assert_eq!(normalize_name_conflict("ask"), "keep_both");
        assert_eq!(normalize_name_conflict(""), "keep_both");
    }

    #[test]
    fn header_presets_reject_injection_and_managed_headers() {
        let mut s = Settings {
            header_presets: vec![HeaderPreset {
                name: "  ".into(),
                host: " *.Example.COM ".into(),
                headers: vec![
                    HeaderKV {
                        name: "Referer".into(),
                        value: " https://example.com/watch ".into(),
                    },
                    // 危险/由下载器管理的头必须被丢弃
                    HeaderKV {
                        name: "Host".into(),
                        value: "evil.example".into(),
                    },
                    HeaderKV {
                        name: "Range".into(),
                        value: "bytes=0-1".into(),
                    },
                    HeaderKV {
                        name: "Cookie".into(),
                        value: "sid=1".into(),
                    },
                    // CRLF 注入
                    HeaderKV {
                        name: "X-Test".into(),
                        value: "ok\r\nX-Evil: 1".into(),
                    },
                    // 头名里带冒号/空格 = 解析歧义
                    HeaderKV {
                        name: "X Bad".into(),
                        value: "1".into(),
                    },
                    HeaderKV {
                        name: "X-Ok".into(),
                        value: "1".into(),
                    },
                ],
            }],
            ..Settings::default()
        };
        s.normalize_header_presets();
        assert_eq!(s.header_presets.len(), 1);
        let p = &s.header_presets[0];
        assert_eq!(p.host, "example.com");
        assert_eq!(p.name, "预设 1");
        let names: Vec<&str> = p.headers.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["Referer", "X-Ok"]);
        assert_eq!(p.headers[0].value, "https://example.com/watch");
    }

    #[test]
    fn global_preset_may_not_carry_authorization() {
        let mut s = Settings {
            header_presets: vec![HeaderPreset {
                name: "global".into(),
                host: "".into(),
                headers: vec![
                    HeaderKV {
                        name: "Authorization".into(),
                        value: "Bearer x".into(),
                    },
                    HeaderKV {
                        name: "User-Agent".into(),
                        value: "UA/1".into(),
                    },
                ],
            }],
            ..Settings::default()
        };
        s.normalize_header_presets();
        assert_eq!(s.header_presets[0].headers.len(), 1);
        assert_eq!(s.header_presets[0].headers[0].name, "User-Agent");

        // 限定 host 时允许带 Authorization
        s.header_presets[0].host = "api.example.com".into();
        s.header_presets[0].headers.insert(
            0,
            HeaderKV {
                name: "Authorization".into(),
                value: "Bearer x".into(),
            },
        );
        s.normalize_header_presets();
        assert_eq!(s.header_presets[0].headers.len(), 2);
    }

    #[test]
    fn headers_for_url_is_host_scoped_and_last_wins() {
        let s = Settings {
            header_presets: vec![
                HeaderPreset {
                    name: "global".into(),
                    host: "".into(),
                    headers: vec![HeaderKV {
                        name: "User-Agent".into(),
                        value: "global-UA".into(),
                    }],
                },
                HeaderPreset {
                    name: "site".into(),
                    host: "example.com".into(),
                    headers: vec![
                        HeaderKV {
                            name: "Referer".into(),
                            value: "https://example.com/".into(),
                        },
                        HeaderKV {
                            name: "User-Agent".into(),
                            value: "site-UA".into(),
                        },
                    ],
                },
            ],
            ..Settings::default()
        };
        // 精确 host 与子域都命中，后出现的预设覆盖同名头
        let h = s.headers_for_url("https://cdn.example.com/a.bin");
        assert_eq!(
            h,
            vec![
                ("User-Agent".to_string(), "site-UA".to_string()),
                ("Referer".to_string(), "https://example.com/".to_string()),
            ]
        );
        // 相似域名不能命中
        let h = s.headers_for_url("https://example.com.evil.test/a.bin");
        assert_eq!(h, vec![("User-Agent".to_string(), "global-UA".to_string())]);
        // query 里出现域名同样不算
        let h = s.headers_for_url("https://evil.test/?x=example.com");
        assert_eq!(h, vec![("User-Agent".to_string(), "global-UA".to_string())]);
    }

    #[test]
    fn header_host_normalization_rejects_ambiguous_scopes() {
        assert_eq!(normalize_header_host("").as_deref(), Some(""));
        assert_eq!(
            normalize_header_host("*.Example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(normalize_header_host("example.com:8080"), None);
        assert_eq!(normalize_header_host("https://example.com"), None);
        assert_eq!(normalize_header_host("bad host"), None);
        assert_eq!(normalize_header_host(".."), None);
    }

    #[test]
    fn netcfg_scopes_custom_headers_and_flags_sensitive() {
        let s = Settings {
            header_presets: vec![HeaderPreset {
                name: "api".into(),
                host: "example.com".into(),
                headers: vec![
                    HeaderKV {
                        name: "X-Api-Key".into(),
                        value: "k".into(),
                    },
                    HeaderKV {
                        name: "Referer".into(),
                        value: "https://example.com/".into(),
                    },
                ],
            }],
            ..Settings::default()
        };
        let net = NetCfg::for_url(&s, "https://cdn.example.com/a.bin");
        assert_eq!(net.custom.len(), 2);
        assert!(net.has_sensitive_headers());

        let net = NetCfg::for_url(&s, "https://other.test/a.bin");
        assert!(net.custom.is_empty());
        assert!(!net.has_sensitive_headers());
    }

    #[test]
    fn categories_are_normalized_and_merged() {
        let mut s = Settings {
            categories_enabled: true,
            categories: vec![
                CategoryRule {
                    folder: " Video ".into(),
                    extensions: vec![".MP4".into(), "mp4".into(), "mkv".into()],
                },
                // 同目录（大小写不敏感）合并，扩展名去重
                CategoryRule {
                    folder: "video".into(),
                    extensions: vec!["MKV".into(), "webm".into()],
                },
                // 越界目录与非法扩展名整条丢弃
                CategoryRule {
                    folder: "../escape".into(),
                    extensions: vec!["exe".into()],
                },
                CategoryRule {
                    folder: "Bad".into(),
                    extensions: vec!["a/b".into()],
                },
            ],
            ..Settings::default()
        };
        s.normalize_categories();
        assert_eq!(s.categories.len(), 1);
        assert_eq!(s.categories[0].folder, "Video");
        assert_eq!(s.categories[0].extensions, vec!["mp4", "mkv", "webm"]);
    }

    #[test]
    fn category_lookup_respects_switch() {
        let mut s = Settings {
            categories_enabled: false,
            ..Settings::default()
        };
        assert_eq!(s.category_folder_for(Some("mp4")), None);

        s.categories_enabled = true;
        assert_eq!(
            s.category_folder_for(Some(".MP4")).as_deref(),
            Some("Video")
        );
        assert_eq!(
            s.category_folder_for(Some("zip")).as_deref(),
            Some("Archives")
        );
        assert_eq!(s.category_folder_for(Some("unknown")), None);
        assert_eq!(s.category_folder_for(None), None);

        // 未归一化的脏规则不能借 lookup 逃出下载根
        s.categories = vec![CategoryRule {
            folder: "../escape".into(),
            extensions: vec!["mp4".into()],
        }];
        assert_eq!(s.category_folder_for(Some("mp4")), None);
    }

    #[test]
    fn host_auth_is_scoped_to_host() {
        let a = HostAuth {
            host: "huggingface.co".into(),
            value: "Bearer t".into(),
        };
        assert_eq!(a.for_url("https://huggingface.co/x"), Some("Bearer t"));
        assert_eq!(
            a.for_url("https://cdn-lfs.huggingface.co/x"),
            Some("Bearer t")
        );
        assert_eq!(a.for_url("https://evil.example/?h=huggingface.co"), None);
        assert_eq!(a.for_url("https://huggingface.co.evil.example/x"), None);
    }
}
