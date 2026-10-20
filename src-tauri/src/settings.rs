use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::AppState;

/// 持久化设置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 默认下载目录
    pub download_dir: String,
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
    /// 浏览器 vgrab:// 链接先弹「另存为」（目录/文件名/线程 → 开始下载/取消）；
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
}

impl Default for Settings {
    fn default() -> Self {
        let dir = dirs::download_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("VGrab")
            .to_string_lossy()
            .to_string();
        Self {
            download_dir: dir,
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
            proxy_enabled: false,
            proxy_type: "http".into(),
            proxy_host: String::new(),
            proxy_port: 0,
            proxy_user: String::new(),
            proxy_pass: String::new(),
            language: "zh".into(),
            theme: "system".into(),
        }
    }
}

fn settings_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("VGrab").join("settings.json")
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
    match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            // 不要把坏文件直接留在原地让下次继续失败，也不要静默覆盖掉：
            // 改名备份，用户至少还能从里面找回自己填过的路径。
            let bad = path.with_extension("json.bad");
            let _ = std::fs::rename(&path, &bad);
            eprintln!(
                "settings.json 解析失败（{e}），已备份到 {}，本次使用默认设置",
                bad.display()
            );
            Settings::default()
        }
    }
}

impl Settings {
    /// 组装 reqwest / yt-dlp 可用的代理 URL，未启用或填得不全时返回 None。
    pub fn proxy_url(&self) -> Option<String> {
        if !self.proxy_enabled {
            return None;
        }
        let host = self.proxy_host.trim();
        if host.is_empty() || self.proxy_port == 0 {
            return None;
        }
        // socks5h：让代理端解析域名，配合「加速被墙站点」的初衷（本地 DNS 往往拿不到正确结果）
        let scheme = if self.proxy_type == "socks5" { "socks5h" } else { "http" };
        let user = self.proxy_user.trim();
        let auth = if user.is_empty() {
            String::new()
        } else {
            format!("{}:{}@", user, self.proxy_pass.trim())
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

/// 网络相关设置的快照，传给直链下载器（direct 模块不需要整个 Settings）。
#[derive(Debug, Clone, Default)]
pub struct NetCfg {
    pub proxy: Option<String>,
    pub rate_kb: u64,
}

impl NetCfg {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            proxy: s.proxy_url(),
            rate_kb: s.rate_limit_kb,
        }
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
                .args(["add", key, "/v", "VGrab", "/d", &cmd, "/f"])
                .creation_flags(0x0800_0000)
                .status()
        } else {
            Command::new("reg")
                .args(["delete", key, "/v", "VGrab", "/f"])
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
/// 下载目录例外：默认值是「系统下载目录/VGrab」，不能像 serde::default 那样
/// 固化成 "."，否则恢复默认会把用户导到程序目录里去。
#[tauri::command]
pub fn default_settings() -> Settings {
    let mut s = Settings::default();
    s.download_dir = default_download_dir();
    s
}

/// 默认下载目录：系统「下载」文件夹下的 VGrab，拿不到时退回当前目录。
pub(crate) fn default_download_dir() -> String {
    dirs::download_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("VGrab")
        .to_string_lossy()
        .to_string()
}

#[tauri::command]
pub fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    load_settings(&state)
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

    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    // 原子写：直接 write 在断电/崩溃时会留下截断的 JSON，下次启动解析失败，
    // 用户改过的下载目录、Cookie 路径、并发数会全部丢失。
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入设置失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("保存设置失败: {e}"))?;

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

    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_string_pretty(&current).map_err(|e| e.to_string())?;
    // 与 save_settings 同样的原子写：主题是高频改动项（用户会来回切着看效果），
    // 截断的 settings.json 会让整份配置报废。
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入设置失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("保存设置失败: {e}"))?;

    if let Ok(mut guard) = state.settings.lock() {
        guard.theme = normalized.clone();
    }
    Ok(normalized)
}
