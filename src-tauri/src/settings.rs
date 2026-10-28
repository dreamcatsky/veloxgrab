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
}

impl NetCfg {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            proxy: s.proxy_url(),
            rate_kb: s.rate_limit_kb,
            auth: None,
        }
    }

    /// 与 `from_settings` 相同，但按目标 URL 决定是否附加 HF token。
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
        net
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
    let mut s = Settings::default();
    s.download_dir = default_download_dir();
    s
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
        // fsync 后再 rename：崩溃时不留「目录项已更新、内容还在页缓存」的空文件
        if let Ok(f) = std::fs::File::open(&tmp) {
            let _ = f.sync_data();
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
        let mut s = Settings::default();
        s.download_dir = d.to_string();
        s
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
}
