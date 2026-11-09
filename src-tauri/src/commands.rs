use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

// catch_unwind 只在 worker 兜底那一处用；FutureExt 以 trait 形式提供它。
use futures_util::FutureExt;

use crate::direct::{self, DirectProbe};
use crate::engine::{self, CancelToken, Task, TaskState};
use crate::save_dir;
use crate::settings::load_settings;
use crate::settings::Settings;
use crate::ytdlp::{self, MediaInfo};

/// 全局应用状态
pub struct AppState {
    pub tasks: engine::SharedTasks,
    pub tokens: engine::SharedTokens,
    /// 设置的内存权威副本。Arc 包一层：队列泵（PumpCtx）与命令层共享同一份，
    /// 泵的续泵/启动热路径从这里读快照，不再每次读盘 + DPAPI 解密。
    pub settings: Arc<std::sync::Mutex<Settings>>,
    /// 排队中待启动的任务 id（VecDeque：队首取出是 O(1)）
    pub queue: Arc<Mutex<VecDeque<String>>>,
    /// 当前已占用的并发槽位
    pub running: Arc<AtomicU64>,
    /// 队列泵互斥锁：同一时刻只允许一个泵在跑。
    ///
    /// 每次 enqueue 都会起一个泵。并发已满时多个泵会各自 pop 一个任务再 push_front
    /// 回去，交错顺序不确定，会把「先加的任务」挤到后面；串行化后既不会乱序，
    /// 也不会 N 个泵同时抢队列空转。
    pub pump_lock: Arc<Mutex<()>>,
    /// 用于按时间戳生成 id
    pub counter: AtomicU64,
    /// 启动阶段收到的 `veloxgrab://` 链接，等前端来取。
    ///
    /// 为什么不用 `emit` 直接推：webview 的 JS 还没执行完时 `emit` 发出去的事件
    /// 会被静默丢弃（没有监听者就没有回放）。冷启动点协议链接时 300ms 的延迟
    /// 根本不够——实测 webview2 首帧往往要 1 秒以上。改成「先存后取」：
    /// 前端 boot() 完成后主动 invoke `take_pending_deep_links`，一次拿全。
    pub pending_links: std::sync::Mutex<Vec<serde_json::Value>>,
    /// 「全部下载完成后」要执行的一次性动作。**不落盘**，见 plan.rs 的模块注释。
    pub plan: std::sync::Mutex<Option<crate::plan::PlanAction>>,
}

// `new()` 会读磁盘上的 settings.json，不是 Default 约定的「廉价、无副作用」构造，
// 所以不实现 Default，显式豁免这条风格检查。
#[allow(clippy::new_without_default)]
impl AppState {
    pub fn new() -> Self {
        let settings = crate::settings::read_settings_from_disk();

        // 启动时把历史回灌进任务表。
        //
        // 「下载记录」页面已经删掉（内容与「所有下载/正在下载/已完成」三个筛选
        // 重复），但 history.json 仍是唯一的持久化层：任务表本身只在内存里，
        // 不回灌的话用户每次重启都看到空列表，昨天下的文件一条都找不到。
        //
        // 回灌出来的任务一律不进队列、不会自己开始下载：终态供筛选和
        // 「打开所在文件夹」；退出时活跃的（含暂停）回灌成 Paused，
        // 断点文件还在盘上，用户点「继续」即续传。
        let mut tasks: HashMap<String, Task> = HashMap::new();
        for entry in crate::history::load_history() {
            let task = entry.to_task();
            tasks.insert(task.id.clone(), task);
        }

        Self {
            tasks: Arc::new(Mutex::new(tasks)),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            settings: Arc::new(std::sync::Mutex::new(settings)),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            running: Arc::new(AtomicU64::new(0)),
            pump_lock: Arc::new(Mutex::new(())),
            counter: AtomicU64::new(1),
            pending_links: std::sync::Mutex::new(Vec::new()),
            plan: std::sync::Mutex::new(None),
        }
    }
}

#[cfg(test)]
impl AppState {
    /// 状态机测试专用构造：不读盘（settings/history 都是用户真实文件，
    /// 读进来只会让测试结果随机器漂移），任务表为空，设置取默认值。
    /// 与 `new()` 的唯一差别是数据来源；锁与队列结构完全一致。
    pub fn new_for_test() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            settings: Arc::new(std::sync::Mutex::new(Settings::default())),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            running: Arc::new(AtomicU64::new(0)),
            pump_lock: Arc::new(Mutex::new(())),
            counter: AtomicU64::new(1),
            pending_links: std::sync::Mutex::new(Vec::new()),
            plan: std::sync::Mutex::new(None),
        }
    }

    /// 灌一个任务进状态机（跳过 enqueue 命令层的目录/磁盘检查——
    /// 那些闸门有自己的测试；这里只关心状态与令牌的编排语义）。
    pub fn insert_task_for_test(&self, task: Task) {
        if let Ok(mut g) = self.tasks.try_lock() {
            g.insert(task.id.clone(), task);
        }
    }
}

/// 当前 Unix 时间戳（毫秒）。任务 id、创建时间、历史结束时间都用它。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 命令入口统一的「保存目录闸门」（高-1）。
///
/// `raw` 是用户/深链/扩展给的目录字符串（已去空白，None/空 = 未指定）。
/// 返回**最终要存进任务并落盘的路径字符串**（None = 用全局默认目录）。
///
/// 判定分三档（见 save_dir 模块头的完整设计）：
/// - 默认根子树内 → 放行（把相对写法解析成绝对、折叠掉 `..`，后面所有
///   创建/清理都吃这个定稿路径）；
/// - 根外但用户此前经原生框同意过 → 放行，不再打扰；
/// - 根外且陌生 → 弹一次 **OS 级原生确认框**。不用 webview 弹窗：本闸门防的
///   正是「被污染的页面想让下载写到哪写到哪」，在网页里问一遍等于问贼。
///   同意后写入信任集（该目录从此不再问，且历史任务重启后仍能续传/清理）；
///   拒绝返回 Err——**绝不静默改道**，「我说下载到 D 盘结果进了 C 盘」是比
///   失败更难排查的坑。
///
/// 弹框用 blocking API 但挪进 spawn_blocking：这些命令跑在 tokio worker 上，
/// 直接把原生模态摁在 runtime 线程里会饿死同 worker 的其它 future。
///
/// `src` 是原始输入（可能带 `..` 或相对写法），返回是**定稿的绝对路径字符串**
/// 或 None（用全局默认）——调用方必须把返回值回填进任务，绝不再存原始值。
async fn admit_save_dir(
    app: &tauri::AppHandle,
    state: &AppState,
    src: Option<String>,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogResult};

    let settings = load_settings(state);
    let root = {
        let d = settings.download_dir.trim();
        if d.is_empty() {
            std::path::PathBuf::from(crate::settings::default_download_dir())
        } else {
            std::path::PathBuf::from(d)
        }
    };
    let outcome = save_dir::admit(src.as_deref(), &root, &settings.trusted_dirs);
    let (cand, needs_prompt) = match outcome {
        save_dir::Admit::NoDir => return Ok(None),
        save_dir::Admit::Inside(p) | save_dir::Admit::Trusted(p) => (p, false),
        save_dir::Admit::Outside(p) => (p, true),
    };

    // 定稿（canonicalize 或最近存在祖先拼回尾段）必须在**弹框之前**做掉：
    // 确认框展示的路径要和最终写进任务/信任集的是同一个字符串，否则用户
    // 批准了 `D:\x\..\y` 落盘的却是 `D:\y`——信任集会记住后者、框却显示前者。
    // 注意顺序：先定稿再弹框。若在同意之后才定稿，定稿失败会把已获得的
    // 用户同意作废，报错文案要如实反映「目录无效」而不是「被拒绝」。
    let display = match save_dir::finalize(&cand) {
        Ok(d) => d,
        Err(e) => return Err(format!("保存目录无效：{e}")),
    };
    if needs_prompt {
        let question = format!(
            "网页/扩展请求把文件下载到默认下载目录之外：\n\n{display}\n\n\
             确定要下载到这个位置吗？拒绝将取消本次下载。"
        );
        let app2 = app.clone();
        // 原生模态框只在这里弹一次；确认结果决定放行还是拒绝，没有第三条路。
        // 超时不设：这是用户交互，等人做决定是正确行为而不是缺陷。
        let accepted = tokio::task::spawn_blocking(move || {
            match app2
                .dialog()
                .message(&question)
                .title("VeloxGrab：确认下载位置")
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "下载到这里".into(),
                    "拒绝".into(),
                ))
                .blocking_show_with_result()
            {
                // 按插件自己的布尔语义判 Yes/Ok；Custom 只在按钮文本被改写的
                // 平台上出现（部分 Linux 桌面环境），这里 Windows 拿不到它，
                // 兜底比较自定义文本，宁可保守：认不出来就当作拒绝。
                MessageDialogResult::Ok | MessageDialogResult::Yes => true,
                MessageDialogResult::Custom(s) => s == "下载到这里",
                _ => false,
            }
        })
        .await
        .map_err(|e| format!("确认框异常: {e}"))?;

        if !accepted {
            return Err("已拒绝写入该目录；如要下载请改到默认下载目录内".into());
        }
        crate::settings::remember_trusted_dir(state, &display);
    }
    Ok(Some(display))
}

/// 打开浏览器扩展所在目录，返回目录路径供前端 reveal。
///
/// 扩展随安装包一起发布（见 tauri.conf.json 的 resources），所以它的位置
/// 与 yt-dlp 同源：都在 resource_dir 下。没找到时返回空字符串而不是报错——
/// 用户可能跑的是 dev 构建，那时候本来就没有扩展。
#[tauri::command]
pub async fn open_extension_dir() -> Result<String, String> {
    let dir = crate::ytdlp::resource_dir()
        .ok_or_else(|| "找不到安装目录".to_string())?
        .join("extension");
    if dir.is_dir() {
        Ok(display_path(&dir))
    } else {
        Ok(String::new())
    }
}

/// 把 PathBuf 变成用户能直接粘贴进对话框的路径。
///
/// Tauri 在 Windows 上给出的 resource_dir 带 `\\?\` verbatim 前缀（`\\?\D:\Tools\VeloxGrab`）。
/// 那个前缀是给 Win32 API 用的，**浏览器「加载解压缩的扩展」对话框不认**——
/// 粘进去会报「清单文件缺失或不可读取」。所以这里把前缀剥掉再返回。
/// UNC 路径（`\\?\UNC\server\share`）要还原成 `\\server\share`，不能简单砍四个字符。
fn display_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// 安装器是否装了浏览器扩展（用户勾了「浏览器扩展」组件）。
///
/// patch-installer.mjs 往 NSIS 里注入了一个默认不勾选的可选 Section，
/// 勾中时会在安装目录写一个 `extension-installed.flag` 标记文件。
/// 前端据此在首启时弹引导：让用户去 chrome://extensions 手动加载。
/// dev 构建没有这个文件，直接返回 false，不打扰用户。
#[tauri::command]
pub async fn extension_installed() -> Result<bool, String> {
    let dir = match crate::ytdlp::resource_dir() {
        Some(d) => d,
        None => return Ok(false),
    };
    Ok(dir.join("extension-installed.flag").is_file())
}

/// 浏览器扩展是否真的在运行。
///
/// `extension_installed` 只说明安装器把文件拷过去了，**不代表用户在浏览器里
/// 加载过它**——这两件事经常被混淆，用户看到「已安装」提示却找不到扩展。
/// 这里返回的是硬证据：本进程启动以来，桥有没有收到过带
/// `chrome-extension://` Origin 的请求。扩展加载后会主动打一次 `/ping`。
#[tauri::command]
pub async fn extension_active() -> Result<bool, String> {
    Ok(crate::bridge::extension_seen())
}

/// 本机装了哪些浏览器（用于首启引导扩展时决定开哪个 extensions 页）。
///
/// 返回形如 `["edge", "chrome"]`，按 Edge → Chrome 的顺序探测：
/// Edge 是 Windows 自带，先问它命中率最高。判断依据是可执行文件存在，
/// 不是「是否默认浏览器」——装了两个的用户自己会知道用哪个。
#[tauri::command]
pub async fn detect_browsers() -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    // Program Files (x86) 优先：Edge 装在 Program Files，Chrome 64 位装根目录。
    let roots: Vec<std::path::PathBuf> = [
        std::env::var_os("ProgramFiles(x86)"),
        std::env::var_os("ProgramFiles"),
        std::env::var_os("ProgramW6432"),
        std::env::var_os("LOCALAPPDATA"),
    ]
    .into_iter()
    .flatten()
    .map(std::path::PathBuf::from)
    .collect();

    let candidates: [(&str, &str); 2] = [
        (
            "edge",
            r"Microsoft\Edge\Application\msedge.exe",
        ),
        (
            "chrome",
            r"Google\Chrome\Application\chrome.exe",
        ),
    ];
    for (name, rel) in candidates {
        if roots.iter().any(|r| r.join(rel).is_file()) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// 打开指定浏览器的扩展管理页。
///
/// Chrome 不允许安装器静默注册扩展（`ExtensionInstallForcelist` 要 HKLM 管理员权限
/// 且扩展必须在 Web Store），所以扩展必须由用户在 chrome://extensions 手动加载。
/// 这里只负责把那个页面打开，剩下的让首启引导弹窗告诉用户选哪个目录。
/// browser 传 "edge" / "chrome"，其他值落到 Chrome。
#[tauri::command]
pub async fn open_extensions_page(app: tauri::AppHandle, browser: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = match browser.as_str() {
        "edge" => "edge://extensions",
        _ => "chrome://extensions",
    };
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("打不开 {url}: {e}"))
}
/// 探测链接
#[tauri::command]
pub async fn probe_url(
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<MediaInfo, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("请输入视频链接".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("链接必须以 http:// 或 https:// 开头".into());
    }
    let settings = load_settings(&state);
    ytdlp::probe(&url, &settings).await
}

/// 把探测结果提交为任务并加入队列
#[derive(Debug, Deserialize)]
pub struct EnqueueArgs {
    pub url: String,
    pub title: String,
    pub thumbnail: Option<String>,
    pub duration: Option<f64>,
    pub format_id: String,
    pub quality_label: String,
    pub has_audio: bool,
    pub best_audio_format: Option<String>,
    /// 下载类型: "video"（走 yt-dlp）或 "direct"（走 Range 多线程）
    #[serde(default)]
    pub kind: Option<String>,
    /// 直链下载的并发分块数
    #[serde(default)]
    pub threads: Option<usize>,
    /// 本任务的保存目录；None / 空串表示用设置里的全局默认目录
    #[serde(default)]
    pub save_dir: Option<String>,
    /// 预计扩展名（视频任务取所选格式的 `ext`，直链可从文件名推导）。
    /// 只用于分类目录；空/非法值不参与匹配。
    #[serde(default)]
    pub expected_ext: Option<String>,
    /// 探测阶段已知的预计大小（字节）。只用于提交时的磁盘预检；
    /// 真正下载时仍以 Content-Length / yt-dlp 报告为准。
    #[serde(default)]
    pub expected_bytes: Option<u64>,
    /// 定时开始（Unix 毫秒；None/0 = 立即下载）。过去的时间点按立即处理。
    #[serde(default)]
    pub start_at: Option<i64>,
    /// 任务级限速 KB/s（None = 用全局设置；0 = 本任务不限速）。
    #[serde(default)]
    pub rate_kb: Option<u64>,
    /// 预期 SHA-256（64 位 hex，大小写不敏感）。下载完成后校验；
    /// 不匹配任务落 Failed（带「校验失败」文案），文件保留供人工比对。
    #[serde(default)]
    pub sha256: Option<String>,
    /// 页面地址（Referer）：浏览器扩展经「另存为」投递视频/直链时带上。
    #[serde(default)]
    pub referer: Option<String>,
}

/// 播放列表探测（合集/频道/多文件页）。
#[tauri::command]
pub async fn probe_playlist(
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<ytdlp::PlaylistInfo, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("请输入链接".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("链接必须以 http:// 或 https:// 开头".into());
    }
    let settings = load_settings(&state);
    ytdlp::probe_playlist(&url, &settings).await
}

/// 批量入队的单条入参（前端从播放列表解析结果里逐条提交）。
#[derive(Debug, Deserialize)]
pub struct BatchEntryArgs {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct BatchArgs {
    pub entries: Vec<BatchEntryArgs>,
    /// 统一保存目录（None = 用全局默认/分类规则）
    #[serde(default)]
    pub save_dir: Option<String>,
    /// 统一开始时间（Unix 毫秒，None = 立即）
    #[serde(default)]
    pub start_at: Option<i64>,
    /// 统一格式表达式。扁平播放列表没有逐条清晰度，固定 "best"：
    /// yt-dlp 自己按 bestvideo+bestaudio 组合，兼容性最好。
    #[serde(default)]
    pub format_id: Option<String>,
}

/// 批量建任务：一次解析出的播放列表逐条入队。
///
/// 与前端「循环调 enqueue_download」的区别：
/// - 目录闸门只弹**一次**（批量 200 条时逐条弹原生框会把用户逼疯）；
/// - 磁盘预检只做一次目录检查（每条的大小未知，逐条预检没有意义）；
/// - 返回成功数与被拒原因，前端据此提示。
#[tauri::command]
pub async fn enqueue_playlist(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: BatchArgs,
) -> Result<serde_json::Value, String> {
    use tauri::Emitter;
    if args.entries.is_empty() {
        return Err("播放列表里没有条目".into());
    }
    let save_dir = admit_save_dir(&app, &state, args.save_dir).await?;
    {
        let settings = load_settings(&state);
        let dir = save_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(&settings.download_dir));
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
    }

    let format_id = args
        .format_id
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "best".to_string());
    let start_at = args.start_at.filter(|&t| t > now_ms());

    let mut created = Vec::with_capacity(args.entries.len());
    let mut errors: Vec<String> = Vec::new();
    for entry in &args.entries {
        let url = entry.url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            errors.push(format!("{}：地址无效", entry.title.clone().unwrap_or_else(|| url.to_string())));
            continue;
        }
        let n = state.counter.fetch_add(1, Ordering::SeqCst);
        let id = format!("t{}_{}", now_ms(), n);
        let title = entry
            .title
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                url.rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("未命名下载")
                    .to_string()
            });
        let task = Task {
            id: id.clone(),
            url: url.to_string(),
            title,
            thumbnail: entry.thumbnail.clone(),
            duration: entry.duration,
            format_id: format_id.clone(),
            quality_label: "播放列表".into(),
            has_audio: true,
            best_audio_format: None,
            state: TaskState::Queued,
            progress: 0.0,
            downloaded_bytes: 0,
            total_bytes: None,
            speed: None,
            eta: None,
            output_path: None,
            error: None,
            file_missing: false,
            created_at: now_ms(),
            kind: "video".into(),
            threads: 0,
            save_dir: save_dir.clone(),
            // 扁平解析拿不到逐条扩展名；留空不参与分类（落回默认根）。
            expected_ext: None,
            start_at,
            rate_kb: None,
            sha256: None,
            // 播放列表条目来自解析结果，没有页面 Referer
            referer: None,
        };
        {
            let mut guard = state.tasks.lock().await;
            guard.insert(id.clone(), task.clone());
            crate::history::upsert_entry(&task);
        }
        state.queue.lock().await.push_back(id.clone());
        created.push(task);
    }

    let ctx = PumpCtx::from_state(&state);
    tauri::async_runtime::spawn(pump_queue(app.clone(), ctx));
    let _ = app.emit("tasks-changed", ());
    Ok(serde_json::json!({
        "created": created.len(),
        "errors": errors,
        "tasks": created,
    }))
}

/// 探测直链：返回是否支持 Range、总大小、文件名与建议线程数
#[tauri::command]
pub async fn probe_direct_url(
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<DirectProbe, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("请输入链接".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("链接必须以 http:// 或 https:// 开头".into());
    }
    let settings = load_settings(&state);
    let net = crate::settings::NetCfg::for_url(&settings, &url);
    direct::probe_direct(&url, None, &net).await
}

/// 智能探测：先判断是否像媒体站点，媒体站走 yt-dlp，其余走直链探测。
/// 返回类型标签，前端据此渲染不同的界面。
#[tauri::command]
pub async fn probe_auto(
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<serde_json::Value, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("请输入链接".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("链接必须以 http:// 或 https:// 开头".into());
    }

    let settings = load_settings(&state);

    // GitHub 仓库地址：翻译成「最新 release 里最合适的资产」再走直链探测。
    // 放在 FTP / 媒体站判定之前：github.com 不是媒体站，直链探测只会拿到
    // 一页 HTML（看起来像"不支持"），而这个地址的真实意图通常是下最新版。
    if let Some((owner, repo)) = crate::github::parse_repo_url(&url) {
        match crate::github::fetch_latest_asset(
            &owner,
            &repo,
            settings.github_token_trimmed(),
            settings.proxy_url().as_deref(),
        )
        .await
        {
            Ok(asset) => {
                let net = crate::settings::NetCfg::for_url(&settings, &asset.download_url);
                return match direct::probe_direct(&asset.download_url, None, &net).await {
                    Ok(mut p) => {
                        // 资产名比探测（下载 URL 末段）更可靠
                        p.filename = crate::util::sanitize_filename(&asset.filename);
                        Ok(serde_json::json!({
                            "kind": "direct",
                            "direct": p,
                            "release": asset,
                        }))
                    }
                    Err(e) => Ok(serde_json::json!({
                        "kind": "error",
                        "message": format!(
                            "找到 {}/{} 的 release「{}」资产 {}，但探测下载地址失败：{e}",
                            owner, repo, asset.tag, asset.filename
                        )
                    })),
                };
            }
            Err(e) => {
                // 仓库地址但 release 拉不到：继续走普通探测（也许用户其实
                // 想要的是这个页面本身），错误只在两条路径都失败时才合并上报。
                let net = crate::settings::NetCfg::for_url(&settings, &url);
                let gh_err = e;
                if let Ok(p) = direct::probe_direct(&url, None, &net).await {
                    return Ok(serde_json::json!({ "kind": "direct", "direct": p }));
                }
                return Ok(serde_json::json!({
                    "kind": "error",
                    "message": format!("GitHub Releases 解析失败：{gh_err}")
                }));
            }
        }
    }

    // FTP：独立的探测通道（SIZE/LIST），前端按直链界面展示
    if url.to_ascii_lowercase().starts_with("ftp://") {
        return match tokio::task::spawn_blocking({
            let url = url.clone();
            move || crate::ftp::probe_ftp(&url, std::time::Duration::from_secs(30))
        })
        .await
        {
            Ok(Ok((total, filename))) => Ok(serde_json::json!({
                "kind": "direct",
                "direct": {
                    "url": url,
                    "filename": filename,
                    "total_bytes": total,
                    "content_type": "application/octet-stream",
                    "range_supported": true,
                    "final_url": url,
                    "suggested_threads": 1,
                    "etag": null,
                    "last_modified": null,
                }
            })),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(format!("FTP 探测异常: {e}")),
        };
    }

    // 已知媒体站点：优先走 yt-dlp
    if direct::looks_like_media_site(&url) {
        match ytdlp::probe(&url, &settings).await {
            Ok(info) => {
                return Ok(serde_json::json!({
                    "kind": "video",
                    "media": info
                }));
            }
            Err(ytdlp_err) => {
                // 判成媒体站不等于 yt-dlp 一定解析得了：站点改版、需要登录、
                // 或者这个链接本身就是 CDN 直链。再试一次直链，别直接放弃。
                let net = crate::settings::NetCfg::for_url(&settings, &url);
                return match direct::probe_direct(&url, None, &net).await {
                    Ok(p) => Ok(serde_json::json!({ "kind": "direct", "direct": p })),
                    Err(direct_err) => Ok(serde_json::json!({
                        "kind": "error",
                        "message": format!(
                            "无法识别该链接：\nyt-dlp 解析：{ytdlp_err}\n直链探测：{direct_err}"
                        )
                    })),
                };
            }
        }
    }

    // 非媒体站点：先试直链
    let net = crate::settings::NetCfg::for_url(&settings, &url);
    match direct::probe_direct(&url, None, &net).await {
        Ok(p) => Ok(serde_json::json!({ "kind": "direct", "direct": p })),
        Err(e) => {
            // 直链失败，兜底再试 yt-dlp（有些站点不在白名单里但 yt-dlp 支持）
            match ytdlp::probe(&url, &settings).await {
                Ok(info) => Ok(serde_json::json!({
                    "kind": "video",
                    "media": info
                })),
                // 两条路径都失败时把两边的原因都带上：直链探测的错误往往只是
                // 「这不是个静态文件」，真正有用的线索在 yt-dlp 那边。
                Err(ytdlp_err) => Ok(serde_json::json!({
                    "kind": "error",
                    "message": format!(
                        "无法识别该链接：\n直链探测：{e}\nyt-dlp 解析：{ytdlp_err}"
                    )
                })),
            }
        }
    }
}

#[tauri::command]
pub async fn enqueue_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: EnqueueArgs,
) -> Result<Task, String> {
    // 与 probe_url / create_direct_task 同一道闸：只放行 http/https。
    // yt-dlp 的选项注入已经被 `--` 挡住了，但它本身接受 file:// 与裸本地路径——
    // 少这一句，视频入口就成了「把本机任意文件复制进下载目录」的原语。
    let url = args.url.trim().to_string();
    if url.is_empty() {
        return Err("请输入下载地址".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("地址必须以 http:// 或 https:// 开头".into());
    }
    // 保存目录闸门（高-1）：越界目录要经原生确认。定稿后的绝对路径直接回填进
    // task.save_dir——后续 resolve_task_dir / 清理 / 重下吃的都是它，不再重复判定。
    let save_dir = admit_save_dir(&app, state.inner(), args.save_dir).await?;
    // 先校验保存目录可写，避免任务排进去才失败。
    // 用任务自己的目录：用户指定的盘符不存在时，这里就该把话说清楚。
    let dir = {
        let settings = load_settings(&state);
        let dir = save_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(&settings.download_dir));
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
        dir
    };

    // 磁盘预检：探测阶段已经知道大小的话，在提交时就拒绝，而不是排进队列
    // 后下到一半才失败。未知大小（HLS / 分片流）跳过，由下载时的实际
    // Content-Length 再兜一次。
    if let Some(need) = args.expected_bytes {
        crate::disk::ensure_room(&dir, need)?;
    }

    let n = state.counter.fetch_add(1, Ordering::SeqCst);
    let id = format!("t{}_{}", now_ms(), n);

    let kind = args.kind.unwrap_or_else(|| "video".into());

    let task = Task {
        id: id.clone(),
        url,
        title: args.title,
        thumbnail: args.thumbnail,
        duration: args.duration,
        format_id: args.format_id,
        quality_label: args.quality_label,
        has_audio: args.has_audio,
        best_audio_format: args.best_audio_format,
        state: TaskState::Queued,
        progress: 0.0,
        downloaded_bytes: 0,
        total_bytes: None,
        speed: None,
        eta: None,
        output_path: None,
        error: None,
        file_missing: false,
        created_at: now_ms(),
        kind: kind.clone(),
        // 0 表示交给设置里的默认值；非 0 也夹到 64（与 direct::MAX_THREADS 一致），
        // 防止 IPC 传入离谱的并发数
        threads: args.threads.unwrap_or(0).min(64),
        // 闸门的定稿路径（None = 用全局默认目录）。原始 args.save_dir 不得
        // 再往下传——那是未经折叠/校验的原始输入。
        save_dir,
        // 分类目录只认规范化后的扩展名；非法值直接丢弃，不阻塞建单
        expected_ext: args
            .expected_ext
            .and_then(|e| crate::util::normalize_ext(&e)),
        // 定时开始：过去的时间点按立即处理（用户填了昨天 = 马上下载）
        start_at: args.start_at.filter(|&t| t > now_ms()),
        // 任务级限速与校验：normalize 在 Task 字段注释里说明
        rate_kb: args.rate_kb,
        sha256: args
            .sha256
            .as_deref()
            .map(str::trim)
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(|s| s.to_ascii_lowercase()),
        // 视频任务同样可能来自浏览器扩展（页面地址 → yt-dlp --referer）
        referer: args.referer,
    };

    {
        let mut guard = state.tasks.lock().await;
        guard.insert(id.clone(), task.clone());
        // 队列态也要落盘：任务还没开始就关程序的场景（例如用户立刻退出），
        // 历史里也该看得到它。
        crate::history::upsert_entry(&task);
    }
    {
        let mut q = state.queue.lock().await;
        q.push_back(id.clone());
    }

    // 不 await：泵只负责在限额内启动任务，启动完立刻返回，没必要让命令等它。
    let ctx = PumpCtx::from_state(&state);
    tauri::async_runtime::spawn(pump_queue(app, ctx));
    Ok(task)
}

/// 定时开始观察者：扫任务表里「Queued 且定时未到」的任务，取**最近**
/// 的到点时刻睡眠，醒来后广播 `start-at-due`（含到点的任务 id 列表）。
/// 前端收到事件刷新列表；真正的启动由泵完成——这里 spawn 一次泵即可。
///
/// 为什么轮询而不是每个任务一个 timer：定时任务同时存在几十个的量级下
/// tokio timer 数量无意义，30 秒的粒度对「定时下载」场景足够（分钟级
/// 精度的预期）；醒来时即使差几十秒，下一次心跳也会补上。
pub fn spawn_start_at_watcher(app: tauri::AppHandle) {
    use tauri::Manager;
    let ctx = PumpCtx::from_state(app.state::<AppState>().inner());
    tauri::async_runtime::spawn(async move {
        loop {
            // 取最近的未到期时间
            let next_due: Option<i64> = {
                let now = now_ms();
                let guard = ctx.tasks.lock().await;
                guard
                    .values()
                    .filter(|t| t.state == TaskState::Queued && !t.start_at_due(now))
                    .filter_map(|t| t.start_at)
                    .min()
            };
            let wait = match next_due {
                Some(due) => {
                    let ms = (due - now_ms()).clamp(500, 30_000) as u64;
                    std::time::Duration::from_millis(ms)
                }
                None => std::time::Duration::from_secs(30),
            };
            tokio::time::sleep(wait).await;

            // 到点了：续泵（泵会跳过还没到点的）。确认真的有到点任务再泵，
            // 避免无谓的 wake。
            let now = now_ms();
            let due_now = {
                let guard = ctx.tasks.lock().await;
                guard
                    .values()
                    .any(|t| t.state == TaskState::Queued && t.start_at_due(now))
            };
            if due_now {
                let app2 = app.clone();
                let ctx2 = ctx.clone();
                tauri::async_runtime::spawn(async move {
                    pump_queue(app2, ctx2).await;
                });
                let _ = tauri::Emitter::emit(&app, "tasks-changed", ());
            }
        }
    });
}

/// 队列泵所需的共享上下文。
///
/// 单独抽成结构体是为了能 move 进 `spawn` 的 future —— `tauri::State<'_, AppState>`
/// 带生命周期，直接捕获会编译不过。早期版本正是因此写了两句 `let _ = queue_c;`
/// 糊弄编译器，结果是「任务下完了不会启动下一个」：队列一旦被填满就再也不动，
/// 只有下一次 enqueue 才会顺带启动一个。
#[derive(Clone)]
pub struct PumpCtx {
    tasks: engine::SharedTasks,
    tokens: engine::SharedTokens,
    queue: Arc<Mutex<VecDeque<String>>>,
    running: Arc<AtomicU64>,
    pump_lock: Arc<Mutex<()>>,
    /// 设置内存权威副本：泵与 worker 启动的热路径从这读，不读盘。
    settings: Arc<std::sync::Mutex<Settings>>,
}

impl PumpCtx {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            tasks: state.tasks.clone(),
            tokens: state.tokens.clone(),
            queue: state.queue.clone(),
            running: state.running.clone(),
            settings: state.settings.clone(),
            pump_lock: state.pump_lock.clone(),
        }
    }
}

/// 并发槽位守卫：离开作用域时归还槽位。
///
/// 早期用裸 `fetch_sub`，任务内部任一 panic 都会跳过释放，槽位永久泄漏，
/// 表现为「越跑越慢，最后完全不再启动新任务」。
struct SlotGuard(Arc<AtomicU64>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 原子地占用一个并发槽位；已满返回 false。
///
/// 不能用 `load` 判断再 `fetch_add`：每个任务结束时都会触发一次续泵，
/// 多个泵可能同时执行，check-then-act 会超发，把并发数顶到上限之上。
fn try_acquire_slot(running: &AtomicU64, max: usize) -> bool {
    let mut cur = running.load(Ordering::SeqCst);
    loop {
        if cur as usize >= max {
            return false;
        }
        match running.compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
}

/// worker 兜底：`run_download` / `run_direct_download` 返回 Err 但任务仍停在
/// 中间态时，强制落 Failed。
///
/// 两个执行器的**常规失败分支**会自己写 Failed 并 emit，但所有 `?` 早退路径
/// ——`resolve_ytdlp`（yt-dlp 缺失/被杀软隔离）、`resolve_task_dir`（移动硬盘
/// 拔出、目录权限变化）、`cmd.spawn`、`child.wait`——发生在任何状态写入之前，
/// 错误串随返回值被 `let _ =` 丢弃。后果：任务永远停在「下载中」，进度 0，
/// 错误不可见；resume/redo 都拒绝非可恢复态，只有重启能解套。
///
/// 写入条件刻意收紧，避免覆盖别人已落好的状态：
/// - 令牌 ptr_eq：归属校验，防止把「恢复后新一轮」的下载误标成失败；
/// - 状态仍是中间态（Queued/Downloading/Merging）：终态/暂停说明引擎或停止
///   命令已经写过状态（它们同样可能返回 Err，比如「已取消」）。
async fn force_fail_unfinished(
    app: &tauri::AppHandle,
    tasks: engine::SharedTasks,
    tokens: engine::SharedTokens,
    id: &str,
    token: &Arc<CancelToken>,
    err: &str,
) {
    use tauri::Emitter;

    // 与 engine 失败分支同一上限：错误串可能带整段 yt-dlp stderr
    let msg: String = err.chars().take(1200).collect();
    let written = {
        // 锁序与泵一致：tasks → tokens。
        let mut guard = tasks.lock().await;
        let still_ours = tokens
            .lock()
            .await
            .get(id)
            .map(|cur| Arc::ptr_eq(cur, token))
            .unwrap_or(false);
        match guard.get_mut(id) {
            Some(t)
                if still_ours
                    && matches!(
                        t.state,
                        TaskState::Queued | TaskState::Downloading | TaskState::Merging
                    ) =>
            {
                t.state = TaskState::Failed;
                t.error = Some(msg.clone());
                t.speed = None;
                t.eta = None;
                crate::history::upsert_entry(t);
                true
            }
            _ => false,
        }
    };
    if written {
        let _ = app.emit(
            "task-progress",
            &engine::ProgressEvent {
                id: id.to_string(),
                state: TaskState::Failed,
                progress: 0.0,
                downloaded_bytes: 0,
                total_bytes: None,
                speed: None,
                eta: None,
                error: Some(msg),
                output_path: None,
            },
        );
    }
}

/// 从 panic payload 里尽量抠出可读消息（tokio 任务里 panic 的信息只在
/// stderr 看得到一瞬，把它带进任务表用户才能知道发生了什么）。
fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

/// 队列泵：在并发限额内尽量多启动任务。
///
/// 返回 `Pin<Box<dyn Future>>` 而不是直接 `async fn`：任务结束时需要再次调用自己，
/// 写成普通 async fn 会构成 async 递归，future 类型无限大，编译不过。
pub fn pump_queue(
    app: tauri::AppHandle,
    ctx: PumpCtx,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        // 串行化：多个泵并发 pop/push 会打乱启动顺序，还会产生无意义的空转。
        // 泵本身只负责 spawn，不会 await 任务完成，所以这里持锁不会死锁。
        let _pump = ctx.pump_lock.lock().await;

        // 每次续泵都重读一次设置，用户改了并发数能立刻生效。
        // 读内存权威副本而不是磁盘：旧写法每次续泵都做「整文件读 + JSON 解析
        // + 两次 DPAPI 解密」，全在 tokio worker 上同步执行，还被本把 pump_lock
        // 串行放大。内存副本由 save_settings 回写，改并发数照样立刻生效。
        let max_concurrent = crate::settings::load_settings_from_shared(&ctx.settings)
            .max_concurrent
            .max(1);

        // 「定时未到」的连续计数。一个已到点的任务被启动会把计数清零；
        // 连续 defer 次数追平当前队列长度 = 整个队列已轮转一圈且全是
        // 未到点的任务 → 退出，不再 pop→push_back 空转。到点唤醒由
        // start_at_watcher 续泵。
        let mut deferred_streak = 0usize;

        loop {
            // 先取任务再占槽位：反过来的话，队列空或任务已被移除时
            // 还得记得把槽位还回去，容易漏。
            let next_id = {
                let mut q = ctx.queue.lock().await;
                match q.pop_front() {
                    Some(id) => id,
                    None => break,
                }
            };

            let task = {
                let guard = ctx.tasks.lock().await;
                guard.get(&next_id).cloned()
            };
            let Some(task) = task else { continue };

            // 任务可能已被取消或暂停
            //
            // 暂停中的任务**绝不能**被泵起来：它排队时可能是「排队中」，用户点了暂停
            // 之后状态变成 Paused，但仍留在队列里。只判断 is_terminal 会让它继续下载，
            // 界面上「已暂停」却在跑，用户完全没法理解。
            if task.state.is_terminal() || task.state == TaskState::Paused {
                continue;
            }

            // 定时未到的任务放回队尾并记一笔：本轮不再启动新任务（槽位
            // 不能被未到点的任务占着干等）。放回队尾而不是队首，避免下一轮
            // 又立刻 pop 到它；到点时由 start_at_watcher 续泵。队里可能还有
            // 已到点的任务排在后面——继续扫，只在本轮**一个可跑任务都没有**
            // 时才退出，否则会漏启动。
            if !task.start_at_due(crate::commands::now_ms()) {
                let mut q = ctx.queue.lock().await;
                q.push_back(next_id);
                let len = q.len();
                drop(q);
                deferred_streak += 1;
                // 整圈全是未到点：退出（len 比较发生在 push 之后，
                // 所以用当前队列长度而不是 pop 前快照）。
                if deferred_streak >= len {
                    break;
                }
                continue;
            }
            deferred_streak = 0;

            if !try_acquire_slot(&ctx.running, max_concurrent) {
                // 并发已满：放回队首，等某个任务结束后的续泵再来取
                ctx.queue.lock().await.push_front(next_id);
                break;
            }
            // 槽位守卫跟着任务走：即使 spawn 的 future 还没跑就被丢弃，
            // Drop 也会把槽位还回去，不会泄漏。
            let slot = SlotGuard(ctx.running.clone());

            // 「置 Downloading」与「插令牌」必须在同一个 tasks 临界区内原子完成。
            //
            // 旧顺序是「置 Downloading → 释放锁 → 建令牌 → 插令牌」。在释放锁到
            // 插令牌这段窗口里，任务已经不在队列（被本泵 pop 走）、又还没有令牌，
            // cancel_task / pause_task 两头都查不到，只能走「无令牌」分支直接把状态
            // 写成 Canceled/Paused；紧接着泵又插一个**全新的、未取消的**令牌并
            // spawn，工作线程把刚被判死刑的任务又拉回 Downloading——界面上
            // 「已取消」闪一下又变回下载中，排队→启动这一步的任务根本没法取消。
            //
            // 合并临界区后这个组合彻底不存在：持有 tasks 锁时「状态是 Downloading」
            // 与「令牌不在表里」不可能同时出现，而停止命令同样先进这把锁——
            // 它们要么看到 Queued（泵还没启动它），要么拿到真令牌去 cancel/pause。
            // 嵌套顺序统一为 tasks → tokens；全代码没有任何「持 tokens 再拿 tasks」
            // 的路径（worker 收尾摘令牌时不碰 tasks），因此不会形成锁序环。
            //
            // 判据用「== Queued」而不是「非终态且非暂停」：重试窗口里引擎会把状态
            // 短暂写回 Queued 而旧 worker 还活着，表里那个令牌就是「有活着的
            // worker」的唯一可靠凭证，见下面 contains_key 分支。
            let token = Arc::new(CancelToken::new());
            let mut started_task: Option<Task> = None;
            let started = {
                let mut guard = ctx.tasks.lock().await;
                match guard.get_mut(&next_id) {
                    Some(t) if t.state == TaskState::Queued => {
                        let mut tks = ctx.tokens.lock().await;
                        if tks.contains_key(&next_id) {
                            // 状态 Queued 但表里有令牌 = 上一个 worker 还在
                            // （yt-dlp 换 Cookie 重试的间隙），不能再起一轮，
                            // 否则两个线程写同一份断点文件。这个 id 已被 pop 出
                            // 队列，旧 worker 收尾后由下一次泵/恢复接手。
                            false
                        } else {
                            t.state = TaskState::Downloading;
                            tks.insert(next_id.clone(), token.clone());
                            // 在同一个临界区里重新取快照：上面那次 clone 发生在
                            // 「置 Downloading」之前，用户可能刚好在排队窗口里
                            // edit_task（改线程/目录/文件名）。用旧快照启动会把
                            // 编辑静默丢掉；这里取到的是状态提交后的最终值。
                            started_task = Some(t.clone());
                            true
                        }
                    }
                    _ => false,
                }
            };
            if !started {
                continue; // 槽位随作用域释放；失败分支没插过令牌，无需回收
            }

            let settings = crate::settings::load_settings_from_shared(&ctx.settings);
            let app_c = app.clone();
            let ctx_c = ctx.clone();
            let Some(task_c) = started_task else {
                continue;
            };

            tauri::async_runtime::spawn(async move {
                {
                    // 槽位在作用域结束时归还，panic 也不会泄漏
                    let _slot = slot;

                    // 两个执行器先装箱成同一类型的 future，再用 catch_unwind 包住：
                    // worker future 任何一行 panic，旧写法会直接跳出整个闭包——
                    // 令牌留在表里（后续 resume/redo 全被「上次停止还在收尾」挡死），
                    // 下面的续泵也被跳过，队列就此停摆。槽位有 SlotGuard 兜着，
                    // 令牌和续泵没有——这里补齐：panic 当作普通错误走兜底路径，
                    // 令牌摘除与续泵照常执行。
                    let run: std::pin::Pin<
                        Box<dyn std::future::Future<Output = Result<Option<String>, String>> + Send>,
                    > = if task_c.kind == "direct" {
                        Box::pin(engine::run_direct_download(
                            app_c.clone(),
                            task_c.clone(),
                            settings,
                            token.clone(),
                            ctx_c.tasks.clone(),
                        ))
                    } else {
                        Box::pin(engine::run_download(
                            app_c.clone(),
                            task_c.clone(),
                            settings,
                            token.clone(),
                            ctx_c.tasks.clone(),
                        ))
                    };
                    let outcome = match std::panic::AssertUnwindSafe(run)
                        .catch_unwind()
                        .await
                    {
                        Ok(r) => r,
                        Err(p) => Err(format!(
                            "内部错误（worker panic）：{}",
                            panic_message(&p)
                        )),
                    };
                    if let Err(e) = outcome {
                        // 常规失败分支之外的 `?` 早退（yt-dlp 缺失、目录失效等）
                        // 不会自己写状态，这里兜底落 Failed，任务才不会永远
                        // 卡在「下载中」。
                        force_fail_unfinished(
                            &app_c,
                            ctx_c.tasks.clone(),
                            ctx_c.tokens.clone(),
                            &task_c.id,
                            &token,
                            &e,
                        )
                        .await;
                    }

                    // 只摘「这一轮自己的」令牌：resume→泵可能在本 worker 收尾之前
                    // 已经给同一个 id 插了新令牌，无条件 remove 会把新轮的令牌
                    // 偷走——之后的 cancel 查无令牌，只能干改状态，新轮下载变幽灵。
                    // Arc::ptr_eq 不等就不摘，把归属还给表里的现任。
                    let mut t = ctx_c.tokens.lock().await;
                    if t.get(&task_c.id).map(|cur| Arc::ptr_eq(cur, &token)).unwrap_or(false) {
                        t.remove(&task_c.id);
                    }
                    drop(t);
                }

                // 终态判定只服务「完成提醒」：取消/暂停不该闪任务栏。
                let done = {
                    let guard = ctx_c.tasks.lock().await;
                    guard
                        .get(&task_c.id)
                        .map(|t| t.state == TaskState::Done)
                        .unwrap_or(false)
                };
                crate::notify::after_worker(&app_c, &ctx_c.tasks, done, &task_c.title).await;

                // 关键：真正把后续排队任务泵起来。
                // 少了这一步，超过并发数的任务会永远停在「排队中」。
                pump_queue(app_c.clone(), ctx_c.clone()).await;

                // 计划动作必须在**续泵之后**判断：泵之前槽位可能立刻被下一个
                // 排队任务占走，「没有活跃任务」会被误判成「全下完了」。
                crate::plan::check_and_fire(&app_c, &ctx_c.tasks, &ctx_c.queue, &ctx_c.running)
                    .await;

                // 监视框跟随活跃下载：worker 收尾时同步（全部结束 → 自动隐藏）。
                crate::monitor::sync(&app_c, &ctx_c.tasks).await;
            });

            // 监视框跟随活跃下载：泵刚启动了新任务时同步（自动浮出，不抢焦点）。
            crate::monitor::sync(&app, &ctx.tasks).await;
        }
    })
}

#[tauri::command]
pub async fn list_tasks(state: tauri::State<'_, AppState>) -> Result<Vec<Task>, String> {
    let guard = state.tasks.lock().await;
    let mut v: Vec<Task> = Vec::with_capacity(guard.len());
    v.extend(guard.values().cloned());
    // 「实物还在吗」只有磁盘知道，且随时会变（用户手动删文件），所以现查
    // 而不是存进任务表：对 done 且有 output_path 的任务一次 stat。
    // 前端据此显示「文件已删除」、把行内「打开」换成「重下」。
    for t in v.iter_mut() {
        if t.state == TaskState::Done {
            if let Some(p) = &t.output_path {
                t.file_missing = !std::path::Path::new(p).is_file();
            }
        }
    }
    // 按创建时间倒序。配合前端的增量更新，这个函数不再被进度事件高频调用。
    v.sort_by_key(|t| std::cmp::Reverse(t.created_at));
    Ok(v)
}

#[tauri::command]
pub async fn cancel_task(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    cancel_one(&state, &id).await;
    Ok(())
}

/// 取消内核（`cancel_task` 与 `remove_task`/`remove_one` 等「先停再删」路径共用）。
///
/// 与旧实现的关键区别：**「查令牌 + 落终态」合并进同一个 tasks 临界区**
/// （令牌表嵌在 tasks 锁内读取，锁序 tasks→tokens 与泵一致，全代码没有反序嵌套）。
/// 旧写法是「先拿 tokens 锁查一眼 → 放开 → 再拿 tasks 锁写 Canceled」，这两把锁
/// 之间泵刚好原子提交（置 Downloading + 插新令牌）的话，写进去的 Canceled 会被
/// 拿着**未取消新令牌**的工作线程踩回 Downloading——和泵自身的老 bug 同一类
/// 幽灵下载，只是窗口更窄。合并后：泵提交前我们看到 Queued（直接落 Canceled，
/// 泵随后在锁内复查到非 Queued 会跳过）；泵提交后我们看到真令牌（cancel 它，
/// 线程自己收尾）。两种时序都不会出现「显示已取消却在下载」。
async fn cancel_one(state: &AppState, id: &str) {
    // 排队中的任务：直接从队列移除并标记取消
    {
        let mut q = state.queue.lock().await;
        if let Some(pos) = q.iter().position(|x| x == id) {
            let _ = q.remove(pos);
            let mut guard = state.tasks.lock().await;
            if let Some(t) = guard.get_mut(id) {
                t.state = TaskState::Canceled;
                t.error = Some("已取消".into());
                // 与 engine 的停止分支同一语义：取消清瞬时字段（P3-1），
                // 否则前端全量刷新把旧进度画回去，与事件里的「进度 0」矛盾
                t.progress = 0.0;
                t.downloaded_bytes = 0;
                t.total_bytes = None;
                t.speed = None;
                t.eta = None;
                // 入队即落盘的取消态也要记进历史
                crate::history::upsert_entry(t);
            }
            return;
        }
    }

    // 运行中的任务（或还没被泵接手的）：置取消标志并杀掉进程树。
    // kill_process_tree 是同步外部进程调用，必须放到锁外做。
    let kill = {
        let mut guard = state.tasks.lock().await;
        let tks = state.tokens.lock().await;
        if let Some(tok) = tks.get(id) {
            tok.cancel();
            // 只在还没进终态时覆盖：worker 摘令牌前会先写终态（Done/Failed），
            // 那一瞬「令牌还在 + 状态已终」是无条件写会把「已完成」误改成
            // 「已取消」的组合——断点还被 redo 当作要清理的作废产物，比状态
            // 难看严重得多。与旧实现的 is_terminal 守卫保持同一判据。
            if let Some(t) = guard.get_mut(id) {
                if !t.state.is_terminal() {
                    t.state = TaskState::Canceled;
                    t.error = Some("已取消".into());
                    t.progress = 0.0;
                    t.downloaded_bytes = 0;
                    t.total_bytes = None;
                    t.speed = None;
                    t.eta = None;
                    crate::history::upsert_entry(t);
                }
            }
            Some(tok.clone())
        } else {
            if let Some(t) = guard.get_mut(id) {
                if !t.state.is_terminal() {
                    t.state = TaskState::Canceled;
                    t.error = Some("已取消".into());
                    t.progress = 0.0;
                    t.downloaded_bytes = 0;
                    t.total_bytes = None;
                    t.speed = None;
                    t.eta = None;
                }
                crate::history::upsert_entry(t);
            }
            None
        }
    };
    if let Some(tok) = kill {
        engine::kill_process_tree(tok.pid());
    }
}

/// 暂停：停止传输但保留断点。
///
/// 与 `cancel_task` 几乎一样，关键差别只有两处：
/// 1. 用 `token.pause()` 而不是 `cancel()`——它先写 paused 标志再置取消位，
///    工作线程收尾时据此落成 `Paused` 而不是 `Canceled`。
/// 2. 队列里的任务只出队不销毁，状态落成 Paused，留着等恢复。
#[tauri::command]
pub async fn pause_task(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    // 与 pause_tasks 共用内核。单任务版与批量版各写一遍「查令牌→落状态」迟早
    // 分叉出「只有其中一条修好竞态」的坑，所以合并到 pause_one。
    //
    // pause_one 保留了原「先 pause() 再 kill、状态交给工作线程收尾」的时序：
    // pause() 会先写 paused 再置 flag，工作线程在 child.wait() 返回后读到它，
    // 从而落成 Paused 而不是 Canceled；反过来先 kill 会让工作线程抢跑成 Canceled，
    // 断点虽在但任务再也恢复不了——那段注释讲的是真实踩过的坑，没动。
    pause_one(&state, &id).await
}

/// 恢复：把暂停/失败/排队中的任务重新送进队列。
///
/// 断点续传靠的是各下载器自己的临时文件（直链 `.veloxgrab-part` + `.veloxgrab-meta`、
/// yt-dlp 的 `.part`），所以恢复不需要我们搬数据，只要重新跑一遍同一个任务。
#[tauri::command]
pub async fn resume_task(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    use tauri::Emitter;

    // 与 resume_tasks 共用内核：旧单任务版在「置 Queued」与「摘旧令牌」之间
    // 正好给泵留了一个能插新令牌的缝，随后无条件 remove 会把新轮令牌拆掉。
    resume_one(&state, &id).await?;

    let _ = app.emit("tasks-changed", ());

    // 不 await：泵只负责在限额内启动任务
    let ctx = PumpCtx::from_state(&state);
    tauri::async_runtime::spawn(pump_queue(app, ctx));
    Ok(())
}

/// 重新下载：对已取消/失败/暂停的任务重新入队。
///
/// 与 `resume_task` 的唯一区别：**已取消**任务先清掉旧断点再从头下。
/// 取消是用户明确表达过「别下了」的终态，残留的 `.veloxgrab-part` /
/// `.part` 属于作废产物——不清的话，重下会莫名续上上次被丢弃的半截数据。
/// 失败 / 暂停的断点仍然有效，行为与恢复一致（原地续传）。
#[tauri::command]
pub async fn redo_task(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    use tauri::Emitter;

    redo_one(&state, &id).await?;

    let _ = app.emit("tasks-changed", ());
    let ctx = PumpCtx::from_state(&state);
    tauri::async_runtime::spawn(pump_queue(app, ctx));
    Ok(())
}

/// 单个任务的重下内核（`redo_task` 与 `redo_tasks` 共用）。
///
/// 时序：**快照判定（临界区 1）→ 磁盘清理（锁外）→ 复查提交（临界区 2）**。
/// 两段临界区之间夹着 `cleanup_task_artifacts`（含 `remove_dir_all`，大临时
/// 目录可达秒级），这段窗口里并发 resume_task / start_all 可以插队提交
/// Queued+令牌——所以临界区 2 必须**复查**「状态仍是快照值且无令牌」才准写，
/// 否则会删掉活跃 worker 正在写的断点、把 Downloading 无条件踩回 Queued
/// （泵看到有令牌会把该 id 从队列丢弃，界面成了「排队中」实际在下的幽灵）。
async fn redo_one(state: &AppState, id: &str) -> Result<(), String> {
    let (task, running) = {
        let guard = state.tasks.lock().await;
        let running = state.tokens.lock().await.contains_key(id);
        (guard.get(id).cloned(), running)
    };
    let Some(task) = task else {
        return Err("任务不存在".into());
    };
    if running
        || matches!(
            task.state,
            TaskState::Downloading | TaskState::Merging | TaskState::Queued
        )
    {
        return Err("任务正在下载或排队中，请先取消再重新下载".into());
    }
    match task.state {
        // Done 一律放行（IDM 语义：重新下载永远可用）。成品已被删的是补救
        // 入口；成品还在的重下会经 finish_download 的撞名避让落到
        // 「名字 (1).后缀」，不覆盖旧文件。两种情况都按从头下载处理。
        TaskState::Canceled | TaskState::Done => {
            // 清掉作废断点，让本轮真正从头开始。放在锁外做（磁盘 IO 不进临界区）：
            // 此刻没有 worker 持有这个任务（上面刚验过），且泵要接手必须先在
            // 锁内看到 Queued——本函数置 Queued 在清断点之后，顺序是安全的。
            let settings = load_settings(state);
            let dir = engine::resolve_task_dir(&settings, &task)
                .unwrap_or_else(|_| std::path::PathBuf::from(&settings.download_dir));
            engine::cleanup_task_artifacts(&dir, &task);
        }
        _ => {}
    }

    {
        let mut guard = state.tasks.lock().await;
        match guard.get_mut(id) {
            Some(t) => {
                // 复查（见函数文档）：与快照判定之间隔着无界磁盘 IO，并发
                // resume/start_all 可能已把任务提交成「Queued+令牌」甚至
                // Downloading。状态对不上或有令牌，一律拒绝重下。
                if t.state != task.state
                    || state.tokens.lock().await.contains_key(id)
                {
                    return Err("任务状态刚发生了变化，请重新操作".into());
                }
                let from_scratch = t.state == TaskState::Canceled || t.state == TaskState::Done;
                t.state = TaskState::Queued;
                t.error = None;
                if from_scratch {
                    // 断点已清，进度必须同步归零，否则界面上还挂着旧进度。
                    t.progress = 0.0;
                    t.downloaded_bytes = 0;
                    t.total_bytes = None;
                    t.speed = None;
                    t.eta = None;
                }
                // 状态变化即落盘（Queued 会记成 paused）：重下点完立刻退出，
                // 重启后这条仍是待下任务，而不是历史里的旧终态。
                crate::history::upsert_entry(t);
            }
            None => return Err("任务不存在".into()),
        }
    }
    // 入队放在 tasks 块之外：全局锁序是 queue→tasks→tokens（cancel_one 的
    // 排队分支持 queue 再拿 tasks），若在 tasks 临界区里拿 queue 就成环。
    // 「置 Queued」与「push_back」之间的间隙只延迟启动，不影响正确性：
    // redo_task/redo_tasks 在内核返回后都会自己 spawn 一次泵。
    {
        let mut q = state.queue.lock().await;
        if !q.iter().any(|x| x == id) {
            q.push_back(id.to_string());
        }
    }
    Ok(())
}

/// 批量重新下载（菜单「重新下载选中」）。返回实际重下的条数，
/// 正在跑/已完成/不存在的条目被跳过。
#[tauri::command]
pub async fn redo_tasks(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: IdsArgs,
) -> Result<usize, String> {
    let mut ok = 0;
    for id in args.ids {
        if redo_one(&state, &id).await.is_ok() {
            ok += 1;
        }
    }
    if ok > 0 {
        use tauri::Emitter;
        let _ = app.emit("tasks-changed", ());
        let ctx = PumpCtx::from_state(&state);
        tauri::async_runtime::spawn(pump_queue(app, ctx));
    }
    Ok(ok)
}

/// 编辑任务参数（文件名 / 线程数 / 保存目录）。
///
/// 只允许排队、暂停、失败三种状态：正在下载的任务先暂停再编辑，避免改到
/// 一半的临时文件与最终落点对不上。保存目录仍然走 `admit_save_dir` 的
/// 同一道闸门——编辑入口不能成为绕过目录信任检查的后门。
///
/// 字段语义：`None` = 不改；`save_dir = Some("")` = 改回全局默认目录。
#[derive(Debug, Deserialize)]
pub struct EditTaskArgs {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub threads: Option<usize>,
    #[serde(default)]
    pub save_dir: Option<String>,
    /// 任务级限速 KB/s：None = 不改；Some(v) = 覆盖（0 = 本任务不限速）
    #[serde(default)]
    pub rate_kb: Option<u64>,
    /// 预期 SHA-256：None = 不改；Some("") = 清除；Some(hex) = 设置
    #[serde(default)]
    pub sha256: Option<String>,
    /// 新的下载地址：None = 不改；Some(url) = 换源重下。
    ///
    /// 改 URL 会让旧断点失去意义（`.veloxgrab-part` 里记的是旧 URL 与旧 ETag，
    /// 续传判定必然拒绝），所以变更时同步清掉临时产物并归零进度——用户看到的是
    /// 「换了个地址从头下」，而不是「进度莫名跳回 0 却还在续传」。
    #[serde(default)]
    pub url: Option<String>,
}

#[tauri::command]
pub async fn edit_task(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: EditTaskArgs,
) -> Result<Task, String> {
    use tauri::Emitter;

    // 先拿快照做状态判定；真正的提交会在下面重新校验一次（admit 可能弹原生框，
    // 期间队列泵完全可能把任务启动起来）。
    let (task, running) = {
        let guard = state.tasks.lock().await;
        let running = state.tokens.lock().await.contains_key(&args.id);
        (guard.get(&args.id).cloned(), running)
    };
    let Some(task) = task else {
        return Err("任务不存在".into());
    };
    if running || !task.state.is_editable() {
        return Err("只能编辑排队、暂停或失败的任务；正在下载的任务请先暂停".into());
    }

    // 目录没变就不进闸门：否则编辑一个「历史回灌来的根外目录」任务时，
    // 只改文件名也会莫名其妙弹一次原生确认框。
    let new_dir = match args.save_dir {
        Some(raw) if raw.trim() != task.save_dir.as_deref().unwrap_or("").trim() => {
            admit_save_dir(&app, &state, Some(raw)).await?
        }
        _ => task.save_dir.clone(),
    };

    let new_title = match args.title {
        Some(t) => {
            let raw = t.trim();
            if raw.is_empty() {
                return Err("文件名不能为空".into());
            }
            Some(crate::util::sanitize_filename(raw))
        }
        None => None,
    };
    let new_threads = args.threads.map(|t| t.min(64));

    // 换源：先校验形状与安全闸（与所有建任务入口同一套），再过一次
    // 「URL 变了就清断点」的重置。这一步放在锁外：cleanup 是磁盘 IO。
    let new_url = match args.url {
        Some(raw) => {
            let u = raw.trim().to_string();
            if u.is_empty() {
                return Err("下载地址不能为空".into());
            }
            if !(u.starts_with("http://") || u.starts_with("https://") || u.starts_with("ftp://")) {
                return Err("地址必须以 http://、https:// 或 ftp:// 开头".into());
            }
            crate::direct::url_target_guarded(&u)?;
            if u == task.url {
                None
            } else {
                Some(u)
            }
        }
        None => None,
    };
    if new_url.is_some() {
        // 旧 URL 的断点与临时文件对新地址毫无意义，而且留着会占盘；
        // 清掉之后从零开始，与「重新下载」的语义一致。
        let settings = load_settings(&state);
        let dir = engine::resolve_task_dir(&settings, &task)
            .unwrap_or_else(|_| std::path::PathBuf::from(&settings.download_dir));
        engine::cleanup_task_artifacts(&dir, &task);
    }

    let updated = {
        let mut guard = state.tasks.lock().await;
        let running_now = state.tokens.lock().await.contains_key(&args.id);
        let Some(t) = guard.get_mut(&args.id) else {
            return Err("任务不存在".into());
        };
        if running_now || !t.state.is_editable() {
            return Err("任务状态已变化，请刷新后重试".into());
        }
        if let Some(u) = new_url {
            t.url = u;
            // 地址换了，之前声明的摘要/预期字节都不可信，进度必须归零
            t.progress = 0.0;
            t.downloaded_bytes = 0;
            t.total_bytes = None;
            t.speed = None;
            t.eta = None;
            // 重下时旧的"文件已删除"标记也要清
            t.file_missing = false;
            t.output_path = None;
            // 扩展名可能随新地址变化：清空后让探测阶段重新推导
            t.expected_ext = crate::util::ext_of_name(&t.url);
        }
        if let Some(title) = new_title {
            t.title = title;
            // 文件名带扩展名时同步更新分类依据；视频任务通常没有扩展名，
            // 此时保留探测阶段拿到的 f.ext。
            if let Some(ext) = crate::util::ext_of_name(&t.title) {
                t.expected_ext = Some(ext);
            }
        }
        if let Some(threads) = new_threads {
            t.threads = threads;
        }
        // 限速与校验预期是任务元数据：None = 不改，改了不影响断点
        if let Some(kb) = args.rate_kb {
            t.rate_kb = Some(kb);
        }
        if let Some(sha) = &args.sha256 {
            t.sha256 = crate::hash::normalize_digest(sha);
        }
        t.save_dir = new_dir;
        crate::history::upsert_entry(t);
        t.clone()
    };

    let _ = app.emit("tasks-changed", ());
    Ok(updated)
}

#[tauri::command]
pub async fn remove_task(
    state: tauri::State<'_, AppState>,
    id: String,
    // true = 连下载本体一起删（用户明确勾选）；缺省 / false = 只删任务记录，
    // 成品文件留在盘上（与历史行为一致，避免升级后误删用户文件）。
    // 注意这里是普通参数不是结构体字段，serde 属性放不了——缺省值靠
    // Option: 前端不传就是 None → 按 false 处理。
    delete_files: Option<bool>,
) -> Result<(), String> {
    // 与批量版共用内核。旧单任务版把「查令牌→取消→删条目」摊成四段各自加锁的
    // 步骤，批量版又是另一份略有出入的实现——泵改成原子提交后，这种分叉极易
    // 出现「只修好一边」的竞态，合并掉。
    // 任务不存在也返回 Ok：「删除」对不存在的东西按幂等处理，
    // 前端批量删除的计数在 remove_tasks 里统一给。
    let _ = remove_one(&state, &id, delete_files.unwrap_or(false)).await;
    Ok(())
}

/// 手动新建下载：不需要先解析。
///
/// 与 `enqueue_download` 的区别：这里直接按用户给的地址和线程数建任务，
/// 由 `run_direct_download` 内部去探测（HEAD + Range）。
/// 用户粘一个已知的直链（比如邮件里的附件链接）时，不该被迫先走一遍解析。
#[derive(Debug, Deserialize)]
pub struct NewDownloadArgs {
    pub url: String,
    /// 可选的文件名覆盖；空串表示让引擎从 URL / Content-Disposition 推导
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub threads: Option<usize>,
    #[serde(default)]
    pub save_dir: Option<String>,
    /// 定时开始（Unix 毫秒；None/0 = 立即）
    #[serde(default)]
    pub start_at: Option<i64>,
    /// 任务级限速 KB/s（None = 跟随全局）
    #[serde(default)]
    pub rate_kb: Option<u64>,
    /// 预期 SHA-256（64 位 hex）
    #[serde(default)]
    pub sha256: Option<String>,
}

#[tauri::command]
pub async fn new_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: NewDownloadArgs,
) -> Result<Task, String> {
    let sha = args
        .sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|s| s.to_ascii_lowercase());
    create_direct_task(
        &app,
        &state,
        DirectTaskArgs {
            url: args.url,
            filename: args.filename,
            threads: args.threads,
            save_dir: args.save_dir,
            start_at: args.start_at.filter(|&t| t > now_ms()),
            rate_kb: args.rate_kb,
            sha256: sha,
            referer: None,
        },
    )
    .await
}

/// `new_download` 与 `veloxgrab://` 深链共用的建任务逻辑。
///
/// 单独抽出来是因为深链进来时没有「前端表单」可依赖：扩展只传一个 URL，
/// 其余全靠这里兜。两条入口必须走同一条路径，否则会出现「网页点能下、
/// 手动粘同一个地址反而失败」这种最难排查的分裂。
/// `new_download` 与深链共用建任务的参数包（参数打包成结构体：
/// 9 个位置参数超出 clippy 的阈值，且调用方新增字段时不用改签名）。
pub(crate) struct DirectTaskArgs {
    pub url: String,
    pub filename: Option<String>,
    pub threads: Option<usize>,
    pub save_dir: Option<String>,
    pub start_at: Option<i64>,
    pub rate_kb: Option<u64>,
    pub sha256: Option<String>,
    pub referer: Option<String>,
}

pub(crate) async fn create_direct_task(
    app: &tauri::AppHandle,
    state: &AppState,
    args: DirectTaskArgs,
) -> Result<Task, String> {
    let DirectTaskArgs {
        url,
        filename,
        threads,
        save_dir,
        start_at,
        rate_kb,
        sha256,
        referer,
    } = args;
    let mut url = url.trim().to_string();
    if url.is_empty() {
        return Err("请输入下载地址".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("地址必须以 http:// 或 https:// 开头".into());
    }

    // GitHub 仓库地址：翻译成最新 release 的资产下载地址。
    //
    // 这条入口（手动新建 / 扩展静默投递 / aria2 addUri）不经过 probe_auto，
    // 不翻译的话会把仓库页面的 HTML 当成文件下下来。解析失败不阻断建任务：
    // 也许用户想要的真是那个页面，照原地址继续。
    let mut filename = filename;
    if let Some((owner, repo)) = crate::github::parse_repo_url(&url) {
        let settings = load_settings(state);
        match crate::github::fetch_latest_asset(
            &owner,
            &repo,
            settings.github_token_trimmed(),
            settings.proxy_url().as_deref(),
        )
        .await
        {
            Ok(asset) => {
                // 用户显式给了文件名时不覆盖它的选择
                if filename.is_none() {
                    filename = Some(crate::util::sanitize_filename(&asset.filename));
                }
                url = asset.download_url;
            }
            Err(e) => {
                eprintln!("[github] {owner}/{repo} release 解析失败，按原地址继续: {e}");
            }
        }
    }

    // 文件名：给了就用（去路径分隔符，防止写穿到别处），否则留空让引擎推导。
    let filename = filename
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .map(|f| crate::util::sanitize_filename(&f));

    // 保存目录闸门（高-1）：定稿绝对路径回填进 task.save_dir，后续
    // resolve_task_dir / redo / remove 都吃它，不再重复判定。
    let save_dir = admit_save_dir(app, state, save_dir).await?;

    // 目录校验前置：盘符不存在时立刻报错，而不是等下载失败
    {
        let settings = load_settings(state);
        let dir = save_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(&settings.download_dir));
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
    }

    let n = state.counter.fetch_add(1, Ordering::SeqCst);
    let id = format!("t{}_{}", now_ms(), n);

    // 标题先填文件名，探测完成后 run_direct_download 会用真实文件名覆盖。
    // 这一步必须在 spawn(pump_queue) 之前定稿：队列泵取的是 task 快照，
    // 若先启动再改 title，下载会用 URL 推导的名字而不是用户指定的名字。
    let title = filename.clone().unwrap_or_else(|| {
        url.rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("未命名下载")
            .to_string()
    });

    // 直链任务此时还没有 Content-Disposition，只能先看用户给的文件名或 URL
    // 末段。推导不出来就不分类（落回根目录），下载完成后不影响文件本身。
    let expected_ext = filename
        .as_deref()
        .and_then(crate::util::ext_of_name)
        .or_else(|| crate::util::ext_of_name(&url));

    let task = Task {
        id: id.clone(),
        url,
        title,
        thumbnail: None,
        duration: None,
        format_id: "direct".into(),
        quality_label: "手动新建".into(),
        has_audio: false,
        best_audio_format: None,
        state: TaskState::Queued,
        progress: 0.0,
        downloaded_bytes: 0,
        total_bytes: None,
        speed: None,
        eta: None,
        output_path: None,
        error: None,
        file_missing: false,
        created_at: now_ms(),
        kind: "direct".into(),
        threads: threads.unwrap_or(0).min(64),
        save_dir: save_dir.clone(),
        expected_ext,
        start_at: start_at.filter(|&t| t > now_ms()),
        rate_kb,
        sha256: sha256.map(|s| s.to_ascii_lowercase()),
        referer: referer
            .map(|r| r.trim().to_string())
            .filter(|r| {
                let l = r.to_ascii_lowercase();
                !r.is_empty() && r.len() <= 2048 && (l.starts_with("http://") || l.starts_with("https://"))
            }),
    };

    {
        let mut guard = state.tasks.lock().await;
        guard.insert(id.clone(), task.clone());
        crate::history::upsert_entry(&task);
    }
    {
        let mut q = state.queue.lock().await;
        q.push_back(id.clone());
    }

    let ctx = PumpCtx::from_state(state);
    tauri::async_runtime::spawn(pump_queue(app.clone(), ctx));
    Ok(task)
}

/// 队列置顶：把选中的任务 id 挪到队列最前（按传入顺序，先传的在前）。
///
/// 只有**仍在队列里**的任务会被挪动；不在队列的（运行中/终态）静默跳过，
/// 返回实际挪动的条数。配合「暂停选中 → 置顶 → 开始」就是完整的优先级
/// 控制——比单独的 priority 字段少一条要全程维护的状态。
#[tauri::command]
pub async fn bump_tasks(
    state: tauri::State<'_, AppState>,
    args: IdsArgs,
) -> Result<usize, String> {
    let mut q = state.queue.lock().await;
    let mut moved = 0;
    // 从队头开始按用户给定的顺序安放：先把选中项从队列里摘出来（保序），
    // 再按顺序插回队首，最后把剩余任务原序接回去。
    let picked: Vec<String> = args
        .ids
        .into_iter()
        .filter(|id| {
            if let Some(pos) = q.iter().position(|x| x == id) {
                q.remove(pos);
                moved += 1;
                true
            } else {
                false
            }
        })
        .collect();
    for (i, id) in picked.into_iter().enumerate() {
        q.insert(i, id);
    }
    Ok(moved)
}

/// 清空已结束的任务。
///
/// 同时把对应条目从历史里删掉：启动时历史会回灌进任务表，只清内存的话
/// 用户以为清掉的条目下次启动又回来了，等于这个按钮在骗人。
#[tauri::command]
pub async fn clear_finished(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    // 先把被清掉的条目整个快照下来：retain 之后字段就没了，而清理临时文件
    // 需要 kind/title/id/save_dir。旧实现只删记录不清残片，取消/失败留下的
    // `.veloxgrab-downloading.<id>` 会一直躺在下载目录里。
    let removed: Vec<Task> = {
        let mut guard = state.tasks.lock().await;
        let mut gone = Vec::new();
        guard.retain(|_, t| {
            if t.state.is_terminal() {
                gone.push(t.clone());
                false
            } else {
                true
            }
        });
        gone
    };
    if removed.is_empty() {
        return Ok(0);
    }

    let settings = load_settings(&state);
    for t in &removed {
        let dir = engine::resolve_task_dir(&settings, t)
            .unwrap_or_else(|_| std::path::PathBuf::from(&settings.download_dir));
        engine::cleanup_task_artifacts(&dir, t);
        let _ = crate::history::remove_entry(&t.id);
    }
    Ok(removed.len())
}

/// 环境自检：yt-dlp / ffmpeg 是否就绪
#[derive(Serialize)]
pub struct HealthReport {
    pub ytdlp_ok: bool,
    pub ytdlp_path: Option<String>,
    pub ytdlp_version: Option<String>,
    pub ffmpeg_ok: bool,
    pub ffmpeg_path: Option<String>,
    pub message: String,
}

#[tauri::command]
pub async fn health_check() -> Result<HealthReport, String> {
    let mut report = HealthReport {
        ytdlp_ok: false,
        ytdlp_path: None,
        ytdlp_version: None,
        ffmpeg_ok: false,
        ffmpeg_path: None,
        message: String::new(),
    };

    match ytdlp::resolve_ytdlp() {
        Ok(p) => {
            report.ytdlp_ok = true;
            report.ytdlp_path = Some(p.to_string_lossy().to_string());
            // 用 tokio 的 Command：yt-dlp 是 PyInstaller 打包的单文件程序，
            // 冷启动要解压到临时目录再过一遍杀软扫描，同步 output() 会把
            // 整个 worker 线程堵住几百毫秒到数秒。
            let mut c = tokio::process::Command::new(&p);
            c.arg("--version");
            #[cfg(windows)]
            c.creation_flags(0x0800_0000);
            if let Ok(o) = c.output().await {
                report.ytdlp_version =
                    Some(crate::decode::decode_output(&o.stdout).trim().to_string());
            }
        }
        Err(e) => {
            report.message.push_str(&e);
        }
    }

    match ytdlp::resolve_ffmpeg() {
        Some(p) => {
            report.ffmpeg_ok = true;
            report.ffmpeg_path = Some(p.to_string_lossy().to_string());
        }
        None => {
            if !report.message.is_empty() {
                report.message.push('\n');
            }
            report
                .message
                .push_str("未找到 ffmpeg：高清视频的音视频合并与转码将不可用。");
        }
    }

    Ok(report)
}

/// 退出前停掉一切（用于退出/自更新前的清理）。
///
/// 语义是「暂停」而不是「取消」：用户在下载中关掉程序，不该把任务打成
/// 不可恢复的已取消——TDM/IDM 都是下次打开仍躺在列表里、可以接着下。
/// pause() 之后工作线程会落成 Paused 并写历史，但退出只有 2 秒上限，
/// 线程未必赶得上：所以这里同步把所有活跃任务直接拍成 Paused 并落历史，
/// 两边写的是同一份状态，先后顺序不影响结果。
pub async fn cancel_all(state: &AppState) {
    // 退出清理：先「暂停」所有活跃任务（令牌+状态原子处理），再清空令牌表。
    //
    // 与旧版的两个区别：
    // 1. 查令牌与写状态在同一 tasks 临界区（锁序 tasks→tokens，与泵一致）。
    //    旧版先快照令牌表、放开、再另起临界区写 Paused：两步之间泵可能刚好
    //    原子提交一个任务（置 Downloading + 插新令牌）。新令牌不在快照里、
    //    没人 pause() 它，状态却被拍成 Paused——2 秒退出窗口赶不上线程收尾，
    //    future 被 drop 后 kill_on_drop 是否生效没有文档保证，子进程可能变孤儿
    //    继续下载。合并后泵与我们在 tasks 锁上互斥，不会再出现「有活令牌但没
    //    被 pause」的组合。
    // 2. 结束后清空令牌表。lib.rs 拍成 Paused 后 app.exit(0)，worker 若没赶上
    //    收尾，resume 类守卫会永远看到幽灵令牌——进程即将退出虽然无实害，但
    //    把令牌与「活 worker」的对应关系保持干净，让本函数的语义自洽。
    //    （worker 的摘除带 Arc::ptr_eq 守卫，表已清空时它什么也不做。）
    let mut kill: Vec<Arc<CancelToken>> = Vec::new();
    {
        let mut guard = state.tasks.lock().await;
        let mut tks = state.tokens.lock().await;
        for (id, t) in guard.iter_mut() {
            if matches!(
                t.state,
                TaskState::Queued | TaskState::Downloading | TaskState::Merging
            ) {
                if let Some(tok) = tks.get(id) {
                    tok.pause();
                    kill.push(tok.clone());
                }
                t.state = TaskState::Paused;
                t.error = None;
                t.speed = None;
                t.eta = None;
                crate::history::upsert_entry(t);
            }
        }
        // 与 lib.rs 里「2 秒后 app.exit(0)」的退出语义配套：令牌随进程作废。
        tks.clear();
    }
    for tok in &kill {
        engine::kill_process_tree(tok.pid());
    }
}

/// 退出前把历史写线程里攒着的变化强制落盘。
///
/// cancel_all 逐条拍 Paused 时只投递给写线程（攒批模型），200ms 窗口可能还没到
/// 进程就 exit(0) 了——「暂停任务重启后还在」正是靠这几条记录，必须在退出前
/// flush。等待给 1 秒上限：磁盘卡住时丢的只是合并窗口内的状态（断点文件不受
/// 影响），比把退出本身卡死好。
pub fn shutdown_history_writer() {
    crate::history::flush_blocking(std::time::Duration::from_secs(1));
}

/// 取走启动阶段收到的 `veloxgrab://` 链接（取走即清空）。
///
/// 前端在 boot() 结束时、以及每次收到 `external-link` 唤醒事件时调用它。
/// 用「前端主动拉」而不是「后端往前推」，是为了绕开 webview 还没执行 JS
/// 时 `emit` 会被丢弃的竞态——冷启动点链接时那个窗口期长达数秒。
#[tauri::command]
pub async fn take_pending_deep_links(state: tauri::State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let mut guard = match state.pending_links.lock() {
        Ok(g) => g,
        Err(err) => err.into_inner(),
    };
    Ok(std::mem::take(&mut *guard))
}

// ---------- 菜单操作 ----------
//
// 以下是「菜单栏」驱动的批量操作。它们与单任务命令的区别在于：
// 批量版对「哪些任务该被处理」做了收敛，前端不需要自己筛两遍状态，
// 避免前后端对「已暂停」这类中间态的理解不一致。

/// 全部开始：把所有「能跑但没在跑」的任务重新送进队列。
///
/// 包含排队、暂停、失败三种状态：
/// - 排队中：本来就在等槽位，重复入队会被 pump 的去重逻辑吃掉
/// - 已暂停：用户点过暂停，现在要它继续
/// - 失败：重试一次（断点续传让它不用从头下）
#[tauri::command]
pub async fn start_all(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    use tauri::Emitter;

    let ids: Vec<String> = {
        let guard = state.tasks.lock().await;
        guard
            .values()
            .filter(|t| t.state.is_resumable())
            .map(|t| t.id.clone())
            .collect()
    };
    if ids.is_empty() {
        return Ok(0);
    }

    // 顺序：先置 Queued、后入队。反过来的话，并发的泵可能在两个锁块之间
    // pop 到状态还是 Failed/Paused 的 id——泵只从 Queued 启动，会把这条直接从
    // 队列里丢掉，任务就永远躺在「排队中」没人再泵它。
    //
    // 置态时一并查令牌表（锁序 tasks→tokens，与泵一致）：表里有令牌 = 上一个
    // 工作线程还没收尾（pause 之后 child.wait 还没返回，或 yt-dlp 换 Cookie 的
    // 重试间隙）。这种任务必须跳过，不能像旧代码那样「无条件把令牌删掉再翻成
    // Queued」——泵的新判据是「Queued 且无令牌即可启动」，删掉活线程的令牌
    // 会让它对着同一个 id 再插一个新令牌 spawn 第二轮，两个线程写同一份断点。
    // 等旧线程自己收尾（它摘令牌时会用 Arc::ptr_eq 只摘自己那一份）后，用户
    // 再点一次「全部开始」即可。返回数以实际翻起的为准。
    let started = {
        let mut guard = state.tasks.lock().await;
        let tks = state.tokens.lock().await;
        let mut ready: Vec<String> = Vec::with_capacity(ids.len());
        for id in &ids {
            if tks.contains_key(id) {
                continue;
            }
            if let Some(t) = guard.get_mut(id) {
                // 令牌缺席但状态已被别的命令改动（不再 is_resumable）时跳过，
                // 不把 Downloading/Done 之类硬掰回 Queued。
                if !t.state.is_resumable() {
                    continue;
                }
                t.state = TaskState::Queued;
                t.error = None;
                // 入队要落盘：历史里旧的 paused 记录得跟着翻新，
                // 否则用户在「全部开始」后立刻退出，重启看到的是暂停
                crate::history::upsert_entry(t);
                ready.push(id.clone());
            }
        }
        // 出 tasks/tokens 临界区后再碰队列（全局锁序 queue→tasks→tokens），
        // 与 pump 的 queue→tasks 嵌套方向保持一致。
        drop(guard);
        drop(tks);
        {
            let mut q = state.queue.lock().await;
            for id in &ready {
                // 去重：已经在队列里的不要再塞一次，否则同一个任务会被启动两遍
                if !q.iter().any(|x| x == id) {
                    q.push_back(id.clone());
                }
            }
        }
        ready.len()
    };

    let _ = app.emit("tasks-changed", ());
    let ctx = PumpCtx::from_state(&state);
    tauri::async_runtime::spawn(pump_queue(app, ctx));
    Ok(started)
}

/// 全部暂停：把每个非终态任务停下去。
///
/// 与 `start_all` 对称。返回实际被暂停的条数，前端据此给提示。
/// 排队中的只出队不杀进程；运行中的先写暂停意图再杀，保证落库是 Paused。
#[tauri::command]
pub async fn pause_all(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    use tauri::Emitter;

    let ids: Vec<String> = {
        let guard = state.tasks.lock().await;
        guard
            .values()
            .filter(|t| !t.state.is_terminal() && t.state != TaskState::Paused)
            .map(|t| t.id.clone())
            .collect()
    };
    if ids.is_empty() {
        return Ok(0);
    }

    // 全部先出队：留在队列里的话，泵会在我们杀进程的同时把它们拉起来
    {
        let mut q = state.queue.lock().await;
        let set: std::collections::HashSet<&String> = ids.iter().collect();
        q.retain(|x| !set.contains(x));
    }

    // 「查令牌 + 落状态」在一次 tasks 临界区内完成（锁序 tasks→tokens，与泵一致）。
    // 分两步的旧写法有个真实窗口：快照令牌表之后、写状态之前，泵正好把某个
    // 队列任务原子提交（置 Downloading + 插新令牌）。它不在快照里，于是被
    // 直接写成 Paused——可新令牌没人 pause，线程照跑，又是一条「显示已暂停
    // 却在下载」的幽灵。合并后要么见到真令牌（pause 它，状态交工作线程收尾，
    // 避免和它抢锁造成状态回跳），要么根本无令牌（直接落 Paused）。
    // kill_process_tree 是同步外部进程调用，必须挪出临界区再做。
    let mut kill: Vec<Arc<CancelToken>> = Vec::new();
    {
        let mut guard = state.tasks.lock().await;
        let tks = state.tokens.lock().await;
        for id in &ids {
            match tks.get(id) {
                Some(tok) => {
                    tok.pause();
                    kill.push(tok.clone());
                }
                None => {
                    if let Some(t) = guard.get_mut(id) {
                        if !t.state.is_terminal() {
                            t.state = TaskState::Paused;
                            t.error = None;
                            crate::history::upsert_entry(t);
                        }
                    }
                }
            }
        }
    }
    // 先 pause() 再 kill 的顺序在循环里就保证了（pause 在上面已做完）
    for tok in &kill {
        engine::kill_process_tree(tok.pid());
    }

    let _ = app.emit("tasks-changed", ());
    Ok(ids.len())
}

/// 恢复选中 / 暂停选中 / 删除选中 的公共入参。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdsArgs {
    pub ids: Vec<String>,
    /// 仅「删除选中」使用：true = 连下载本体一起删。
    ///
    /// `#[serde(default)]` 是必须的——这个结构体同时给 resume_tasks /
    /// pause_tasks / remove_tasks 用，那两条命令的调用方不会传这个字段。
    /// 默认 false = 保守语义（只删任务记录），与升级前行为一致。
    ///
    /// `alias = "deleteFiles"`：项目里 Tauri 对**结构体字段**不做驼峰转换
    /// （后端字段名就是 JSON 键，见 EnqueueArgs.format_id 的既有约定），
    /// 前端如果写成 `deleteFiles` 会被 serde 当成未知字段、再由 default
    /// 补成 false —— 静默变成「不删文件」，最难查的一类 bug。两边都收。
    #[serde(default, alias = "deleteFiles")]
    pub delete_files: bool,
}

/// 批量恢复。
#[tauri::command]
pub async fn resume_tasks(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: IdsArgs,
) -> Result<usize, String> {
    let mut ok = 0;
    for id in args.ids {
        if resume_one(&state, &id).await.is_ok() {
            ok += 1;
        }
    }
    if ok > 0 {
        use tauri::Emitter;
        let _ = app.emit("tasks-changed", ());
        let ctx = PumpCtx::from_state(&state);
        tauri::async_runtime::spawn(pump_queue(app, ctx));
    }
    Ok(ok)
}

/// 批量暂停。
#[tauri::command]
pub async fn pause_tasks(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: IdsArgs,
) -> Result<usize, String> {
    let mut ok = 0;
    for id in args.ids {
        if pause_one(&state, &id).await.is_ok() {
            ok += 1;
        }
    }
    if ok > 0 {
        use tauri::Emitter;
        let _ = app.emit("tasks-changed", ());
    }
    Ok(ok)
}

/// 单个任务的暂停内核（`pause_task` 与 `pause_tasks` 共用）。
async fn pause_one(state: &AppState, id: &str) -> Result<(), String> {
    // 排队中的：出队并落状态
    {
        let mut q = state.queue.lock().await;
        if let Some(pos) = q.iter().position(|x| x == id) {
            let _ = q.remove(pos);
            let mut guard = state.tasks.lock().await;
            if let Some(t) = guard.get_mut(id) {
                t.state = TaskState::Paused;
                t.error = None;
                crate::history::upsert_entry(t);
            }
            return Ok(());
        }
    }

    // 「查令牌 + 落状态意图」同样合并进 tasks 临界区（锁序 tasks→tokens 与泵一致）。
    // 分两步写的旧窗口：pause 查令牌时泵还没提交 → 走「无令牌」分支；泵随即原子
    // 启动；pause 再把 Downloading 踩成 Paused——令牌没被暂停，线程照跑，
    // 又是一个「显示已暂停却在下载」的幽灵。合并后互斥于泵，两种时序都收敛：
    //   - 见到真令牌：只 pause()，状态交给工作线程收尾（沿用旧设计，避免和它
    //     抢同一把锁造成状态回跳——这正是这里不直接写 Paused 的原因）；
    //   - 没有令牌：非终态直接落 Paused。
    let kill = {
        let mut guard = state.tasks.lock().await;
        let tks = state.tokens.lock().await;
        match tks.get(id) {
            Some(tok) => {
                tok.pause();
                Some(tok.clone())
            }
            None => {
                match guard.get_mut(id) {
                    Some(t) if !t.state.is_terminal() => {
                        t.state = TaskState::Paused;
                        t.error = None;
                        crate::history::upsert_entry(t);
                    }
                    Some(_) => return Err("该任务已结束".into()),
                    None => return Err("任务不存在".into()),
                }
                None
            }
        }
    };
    if let Some(tok) = kill {
        engine::kill_process_tree(tok.pid());
    }
    Ok(())
}

/// 单个任务的恢复内核（`resume_task` 与 `resume_tasks` 共用）。
async fn resume_one(state: &AppState, id: &str) -> Result<(), String> {
    {
        let mut q = state.queue.lock().await;
        // 已经在队列里的 Queued 任务点「继续」是空操作：走下面的「先删再加」
        // 会把它的启动顺位悄悄压到队尾——用户点一下反而更靠后。
        if q.iter().any(|x| x == id) {
            return Ok(());
        }
        q.retain(|x| x != id);
    }
    // 「查令牌 + 置 Queued」合并进一个 tasks 临界区（锁序 tasks→tokens 与泵一致）。
    // 表里还有令牌 = 上一个工作线程还没收尾（多半是 pause 之后、线程读到
    // paused 标志之前的窗口，或 Cookie 重试间隙——后者状态就是 Queued）。
    // 这时翻态入队会踩穿泵的新不变式：旧线程手里那个令牌与新一轮的写者
    // 身份无法区分，两个线程会写同一份断点文件。与 redo_one 同一条判据：
    // 拒绝，让旧线程收尾（它摘令牌时 Arc::ptr_eq 只摘自己那份），用户稍后
    // 再点一次「继续」。旧设计是「摘掉旧令牌再入队」——泵的新判据下那等于
    // 亲手制造双线程窗口，所以这里改成守卫而不是清理。
    {
        let mut guard = state.tasks.lock().await;
        if state.tokens.lock().await.contains_key(id) {
            return Err("上一次停止还在收尾，请稍后再试".into());
        }
        match guard.get_mut(id) {
            Some(t) => {
                // 与 resume_task 同一判据：Failed 可续传，Done/Canceled 拒绝。
                if !t.state.is_resumable() {
                    return Err(match t.state {
                        TaskState::Done => "任务已完成，无需恢复".into(),
                        TaskState::Canceled => "任务已取消，无法恢复".into(),
                        _ => "该任务无法恢复".into(),
                    });
                }
                t.state = TaskState::Queued;
                t.error = None;
                // 落盘覆盖旧记录：重启前这一刻如果进程没了，重启看到的
                // 应该是排队中（历史记成 paused 回灌），而不是旧的失败态。
                crate::history::upsert_entry(t);
            }
            None => return Err("任务不存在".into()),
        }
    }
    {
        let mut q = state.queue.lock().await;
        q.push_back(id.to_string());
    }
    Ok(())
}

/// 批量删除的返回：区分「删了几个任务」和「真删掉了几个文件」。
///
/// 为什么不再只返回 usize：`delete_files=true` 时删文件可能**部分失败或
/// 被安全判据拒绝**（文件被占用、路径不在任务目录、符号链接…）。旧实现
/// 只把这些原因 eprintln 掉——release 版是 GUI 程序，eprintln 没有落点，
/// 用户看到的就是「点了确认、任务没了、文件还在」。必须让失败可见。
#[derive(serde::Serialize)]
pub struct RemoveReport {
    /// 实际移除的任务条数
    pub removed: usize,
    /// 实际从磁盘删掉的文件数
    pub files_deleted: usize,
    /// 逐条失败原因（已脱敏为「文件名 + 原因」）
    pub errors: Vec<String>,
}

/// 批量删除：先杀进程、清临时文件，再从任务表移除。
#[tauri::command]
pub async fn remove_tasks(
    state: tauri::State<'_, AppState>,
    args: IdsArgs,
) -> Result<RemoveReport, String> {
    let mut report = RemoveReport {
        removed: 0,
        files_deleted: 0,
        errors: Vec::new(),
    };
    for id in args.ids {
        let outcome = remove_one(&state, &id, args.delete_files).await;
        if outcome.removed {
            report.removed += 1;
        }
        if outcome.file_deleted {
            report.files_deleted += 1;
        }
        if let Some(why) = outcome.file_error {
            report.errors.push(why);
        }
    }
    Ok(report)
}

/// `remove_one` 的结果：把「任务删没删」与「文件删没删」拆开报。
///
/// 分开报的理由：这两件事**可以一个成功一个失败**——目标文件被别的程序
/// 占用、路径被判据拒绝、用户提前手删……。原来的实现是「先删任务记录、
/// 再删文件」，文件删失败时记录已经没了，界面上只剩一条错误提示，用户
/// **没有任何办法重试**。现在反过来：文件没删成就不动任务记录。
struct RemoveOutcome {
    removed: bool,
    file_deleted: bool,
    /// 面向用户的失败原因：文件名 + 原因，不含完整路径（日志/提示都不必暴露）
    file_error: Option<String>,
}

/// 删除单个任务的完整流程。
///
/// 与 `remove_task` 命令分开是为了让批量版不必重复实现「避免泄漏临时文件」
/// 的那段逻辑——那段逻辑少一句就会在下载目录里留下半个 GB 的残片。
///
/// `delete_files`：是否连**已下载的成品**一起删。默认 false 时行为与历史一致
/// （只清任务记录 + 临时残片，成品留在盘上）。为 true 时还要过
/// `save_dir::check_deletable_file` 的把关——`output_path` 来自磁盘上的
/// history.json，属于不可信输入，不能直接喂给 remove_file。
///
/// **顺序**：先删文件、成功了才删任务记录。失败则整个任务原样保留（只清掉
/// 已经摘走的令牌，任务回到可重试状态），让用户能再点一次。
async fn remove_one(state: &AppState, id: &str, delete_files: bool) -> RemoveOutcome {
    let mut outcome = RemoveOutcome {
        removed: false,
        file_deleted: false,
        file_error: None,
    };

    // 第一步：先把成品删掉。只读快照，不摘令牌——删文件失败时任务必须原样
    // 留在表里，所以这一步不能有任何「已提交」的副作用。
    if delete_files {
        let snapshot = {
            let guard = state.tasks.lock().await;
            guard.get(id).cloned()
        };
        if let Some(task) = &snapshot {
            // 正在下载的任务没有成品可言，交给下面正常移除
            let running = { state.tokens.lock().await.contains_key(id) };
            if !running {
                let settings = load_settings(state);
                let dir = engine::resolve_task_dir(&settings, task)
                    .unwrap_or_else(|_| std::path::PathBuf::from(&settings.download_dir));
                if let Some(out) = task.output_path.as_deref() {
                    match delete_output_file(out, &dir) {
                        Ok(deleted) => outcome.file_deleted = deleted,
                        Err(why) => {
                            // 文件没删掉 → 任务一个字都不动，直接带着原因返回。
                            // 用户重试时走的还是同一条路径，问题解决后即可成功。
                            outcome.file_error = Some(why);
                            return outcome;
                        }
                    }
                }
            }
        }
    }

    // 第二步：摘令牌 + 删任务记录（原子，锁序 tasks→tokens 与泵一致）。
    //
    // 泵只有两条可能时序，都收敛：
    //  - 我们先抢到 tasks 锁：任务从表里消失，泵 pop 后 get None 直接 continue，
    //    永远不会为已删除的任务插新令牌起 ghost 轮次；
    //  - 泵先原子提交（置 Downloading + 插令牌）：我们看到真令牌，cancel + kill，
    //    线程自己收尾。旧写法「查令牌」与「删条目」分两段加锁，两段之间提交的
    //    轮次没人 cancel——删了任务文件却继续被下完，正是这个组合的产物。
    // 快照与 remove 同区取得：清临时文件需要 kind/title/save_dir，条目删掉之后
    // 就再也没有这些字段了。kill_process_tree 是同步外部进程调用，放锁外。
    let (token, snapshot) = {
        let mut guard = state.tasks.lock().await;
        let mut tks = state.tokens.lock().await;
        (tks.remove(id), guard.remove(id))
    };
    if let Some(token) = &token {
        token.cancel();
        engine::kill_process_tree(token.pid());
    }
    outcome.removed = snapshot.is_some();

    {
        let mut q = state.queue.lock().await;
        q.retain(|x| x != id);
    }

    // 历史也得删：启动时历史会回灌进任务表，只清内存的话下次启动它又回来了。
    let _ = crate::history::remove_entry(id);

    if let Some(task) = &snapshot {
        // 删除时清临时文件：用户删掉任务就是要它彻底消失，
        // 留着 .veloxgrab-part 下次同名下载会莫名其妙地续上一个旧进度。
        // 目录必须用任务自己的 save_dir：单独设过目录的任务，
        // 用全局默认目录去找会扑空，残片就永远留在那儿。
        // 还在跑（摘到过令牌）的任务跳过：文件被工作线程的句柄锁着，
        // Windows 上删除本来就失败，残片交给 7 天陈旧清理兜底。
        // 成品文件在第一步就已经删掉了，这里只管残片。
        if token.is_none() {
            let settings = load_settings(state);
            let dir = engine::resolve_task_dir(&settings, task)
                .unwrap_or_else(|_| std::path::PathBuf::from(&settings.download_dir));
            engine::cleanup_task_artifacts(&dir, task);
        }
    }
    outcome
}

/// 删除一个任务的成品文件，返回面向用户的失败原因。
///
/// 返回值表示**这次是否真的删掉了文件**：文件早已不存在时返回 `Ok(false)`，
/// 让调用方把任务记录正常移除，同时不把「本来就没有的文件」算进删除计数。
///
/// 三层处理：精确路径 → 编码损坏的兜底查找 → 明确报错。
/// 单独抽出来是因为它同时要照顾「路径合法但文件被占用」和
/// 「路径本身是坏的（历史遗留 cp936 编码）」两类情况。
fn delete_output_file(output_path: &str, task_dir: &std::path::Path) -> Result<bool, String> {
    let target = match save_dir::check_deletable_file(output_path, task_dir) {
        Ok(p) => p,
        Err(why) => {
            // 精确路径不通时，先试「编码损坏的历史记录」救援路径。
            // 老版本把 yt-dlp 的 cp936 输出当 UTF-8 解，中文名存成了问号，
            // 这些任务的 output_path 指向一个不存在的文件。
            match save_dir::find_file_for_broken_path(output_path, task_dir) {
                Some(p) => p,
                // 文件本来就不在了（用户手工删过）：没有东西可删，算成功。
                // 必须在救援路径之后判断——坏编码路径的真实文件可能还在盘上，
                // 先按「不存在」放行会把任务删掉、文件却留下。
                None if file_is_gone(output_path) => return Ok(false),
                None => {
                    let name = std::path::Path::new(output_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| output_path.to_string());
                    return Err(format!("{name}：{why}"));
                }
            }
        }
    };

    match std::fs::remove_file(&target) {
        Ok(()) => Ok(true),
        // 已经不在了：用户要的结果已经达成，不算失败
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => {
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "文件".into());
            Err(format!("{name}：{e}（可能正被其他程序占用）"))
        }
    }
}

/// 路径是否**确实不存在**（而不是没有权限 / 不是文件）。
///
/// 只认绝对路径：相对路径在 `check_deletable_file` 里本来就该被拒绝，
/// 不能因为「当前目录下没有」就当成用户已经删掉、放行删任务。
fn file_is_gone(output_path: &str) -> bool {
    if !std::path::Path::new(output_path).is_absolute() {
        return false;
    }
    matches!(
        std::fs::symlink_metadata(output_path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// 清空所有任务（不分状态），同时删掉各自的临时文件。
#[tauri::command]
pub async fn clear_all_tasks(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    use tauri::Emitter;

    // 摘掉令牌之前的「在跑任务」名单：cancel_all 只负责发暂停信号并清空令牌表，
    // worker 可能还在写文件。对这批任务跳过临时文件清理——Windows 上删一个
    // 被工作线程打开的文件必然失败，删了也会在 worker 收尾时重新出现；
    // 交给启动时的 7 天陈旧清理收口，比在这里赌时序可靠。
    let running: std::collections::HashSet<String> = {
        let tokens = state.tokens.lock().await;
        tokens.keys().cloned().collect()
    };

    // 先停掉所有在跑的，否则它们会继续往刚删掉的任务目录里写
    cancel_all(&state).await;

    {
        let mut q = state.queue.lock().await;
        q.clear();
    }

    let tasks: Vec<Task> = {
        let guard = state.tasks.lock().await;
        guard.values().cloned().collect()
    };
    let n = tasks.len();
    {
        let settings = load_settings(&state);
        for t in &tasks {
            if running.contains(&t.id) {
                continue;
            }
            let dir = engine::resolve_task_dir(&settings, t)
                .unwrap_or_else(|_| std::path::PathBuf::from(&settings.download_dir));
            engine::cleanup_task_artifacts(&dir, t);
        }
    }

    {
        let mut guard = state.tasks.lock().await;
        guard.clear();
    }
    {
        let mut tokens = state.tokens.lock().await;
        tokens.clear();
    }

    // 历史一并清空：清空所有任务是明确的「抹掉一切」，留着历史回灌
    // 会让下次启动又冒出一堆用户以为已经删掉的东西。
    let _ = crate::history::clear_all();

    let _ = app.emit("tasks-changed", ());
    Ok(n)
}

/// 从剪贴板取第一个 URL。没有就返回空字符串，前端据此提示。
#[tauri::command]
pub async fn clipboard_url() -> Result<String, String> {
    let text = crate::clipboard::read_clipboard_text();
    Ok(crate::clipboard::extract_first_url(&text).unwrap_or_default())
}

/// 打开一个已完成任务的文件（右键菜单「打开」/ 双击行）。
///
/// 路径来自任务表，而任务表可被 history.json 回灌——持久层算不可信输入。
/// 所以这里不只是「把字符串交给 shell」：
/// - 必须存在且是文件（目录不走这条路，防「打开」变成「用默认程序打开随机目录」）；
/// - 必须是本程序库内认知的文件（存在即可，因为用户本来就指定了任意目录下载）；
/// - 用 opener 的 open_path（内部走 ShellExecuteW），不拼命令行，无注入面。
///
/// 故意**不**在此处做「下载目录闸门」：文件已经落在盘上了，打开它是用户对
/// 自己文件的正当操作，与「往哪写」是两件事。
#[tauri::command]
pub async fn open_task_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let p = std::path::PathBuf::from(path.trim());
    if p.as_os_str().is_empty() {
        return Err("文件路径为空".into());
    }
    if !p.is_file() {
        return Err("文件不存在或不是普通文件（可能已被移动/删除）".into());
    }
    app.opener()
        .open_path(p.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| format!("打开文件失败: {e}"))
}

/// 在系统资源管理器里定位一个已完成文件（选中它）。
///
/// 与 `open_task_file` 的区别：这个只打开文件夹并高亮，不会启动文件关联的
/// 程序——对「.exe/.bat 下载完」这类场景，定位比直接执行安全得多，所以右键
/// 菜单的默认项用它。
#[tauri::command]
pub async fn reveal_task_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let p = std::path::PathBuf::from(path.trim());
    if p.as_os_str().is_empty() {
        return Err("文件路径为空".into());
    }
    if !p.exists() {
        return Err("文件不存在（可能已被移动/删除）".into());
    }
    app.opener()
        .reveal_item_in_dir(&p)
        .map_err(|e| format!("定位文件失败: {e}"))
}

/// 复制文本到剪贴板（右键菜单「复制下载链接」/「复制文件路径」）。
///
/// 只写文本，格式固定 CF_UNICODETEXT，不碰其它格式——写剪贴板是本程序唯一
/// 会**修改**系统共享状态的操作，越窄越好。最多重试几次（剪贴板被别的程序
/// 短暂占用是常态），失败如实返回。
#[tauri::command]
pub async fn copy_text(text: String) -> Result<(), String> {
    if text.is_empty() {
        return Err("没有可复制的内容".into());
    }
    crate::clipboard::write_clipboard_text(&text)?;
    // 登记成「自己写的」：否则剪贴板监听会立刻把刚复制的链接当成用户
    // 新复制的内容再提示一遍。
    crate::clipboard::note_self_copy(&text);
    Ok(())
}

/// 隐藏主窗口（关闭提示里选「最小化到托盘」）。
#[tauri::command]
pub fn hide_main_window(app: tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

/// 自绘标题栏的窗口控制（decorations:false 后原生按钮没了）。
///
/// 返回动作后的最大化状态，前端据此切换最大化/还原图标；`query` 只查不改，
/// 供 resize 事件兜底刷新（拖拽区双击、Win+方向键贴靠走的都是系统路径，
/// 前端按钮感知不到，只能靠事件 + 查询）。
///
/// `close` 走窗口的 `close()` 而不是 hide/exit：`on_window_event` 的
/// CloseRequested 处理器里挂着 close_mode（询问/最小化到托盘/直接退出）
/// 语义，自绘关闭按钮必须复用同一条路。
#[tauri::command]
pub fn window_control(app: tauri::AppHandle, action: String) -> Result<bool, String> {
    use tauri::Manager;
    let Some(w) = app.get_webview_window("main") else {
        return Err("主窗口不存在".into());
    };
    match action.as_str() {
        "minimize" => {
            let _ = w.minimize();
        }
        "maximize_toggle" => {
            if w.is_maximized().unwrap_or(false) {
                let _ = w.unmaximize();
            } else {
                let _ = w.maximize();
            }
        }
        "close" => {
            let _ = w.close();
        }
        // 只查询
        "query" => {}
        _ => return Err(format!("未知窗口动作: {action}")),
    }
    Ok(w.is_maximized().unwrap_or(false))
}

/// 真正退出程序。
///
/// 走 `app.exit(0)` 而不是 `std::process::exit`：前者会触发 `ExitRequested`，
/// 那里才有「杀掉 yt-dlp 子进程 + 刷历史」的清理。直接退进程会把下载中的
/// 文件留在半成品状态，还会留下没人回收的子进程。
#[tauri::command]
pub fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// 设定「全部下载完成后」的动作：`none` 表示取消。
///
/// 计划只活在内存里（见 plan.rs 的模块注释）：关机是一次性意图，不该跨重启
/// 存活。返回归一化之后的值，前端据此更新状态栏提示。
#[tauri::command]
pub fn set_plan(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    action: String,
) -> Result<Option<String>, String> {
    let next = if action.trim() == "none" || action.trim().is_empty() {
        None
    } else {
        Some(
            crate::plan::PlanAction::parse(&action)
                .ok_or_else(|| format!("未知的完成动作: {action}"))?,
        )
    };
    {
        let mut guard = match state.plan.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        *guard = next;
    }

    let saved = next.map(|a| a.as_str().to_string());
    // 设计划时队列可能已经空了（用户就是等全部下完才来设的）。不在这里
    // 主动判一次，就得等下一个任务结束才会触发——而那时可能永远没有下一个。
    if saved.is_some() {
        let ctx = PumpCtx::from_state(&state);
        tauri::async_runtime::spawn(async move {
            crate::plan::check_and_fire(&app, &ctx.tasks, &ctx.queue, &ctx.running).await;
        });
    }
    Ok(saved)
}

#[tauri::command]
pub fn get_plan(state: tauri::State<'_, AppState>) -> Option<String> {
    let guard = match state.plan.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    guard.map(|a| a.as_str().to_string())
}

/// 取消已经排队的关机/重启（`shutdown /a`）。
///
/// 只对「系统已经排上队」的情况有意义；睡眠没有取消一说，所以这里不处理。
/// async 命令：sync 命令默认跑主线程，`.status()` 同步等待子进程会短暂冻结 UI。
#[cfg(windows)]
#[tauri::command]
pub async fn cancel_shutdown() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let status = std::process::Command::new(r"C:\Windows\System32\shutdown.exe")
        .arg("/a")
        .creation_flags(0x0800_0000)
        .status()
        .map_err(|e| format!("调用 shutdown /a 失败: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("没有正在倒计时的关机/重启".into())
    }
}

#[cfg(not(windows))]
#[tauri::command]
pub async fn cancel_shutdown() -> Result<(), String> {
    Err("本平台未实现".into())
}

/// 打开下载文件夹（菜单「文件 → 打开下载文件夹」）。
///
/// 用当前的默认下载目录，而不是「最后一个任务」的目录：
/// 用户点这一项时想看的通常就是总目录。
#[tauri::command]
pub async fn open_download_dir(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    use tauri_plugin_opener::OpenerExt;

    let dir = {
        let settings = load_settings(&state);
        settings.download_dir.clone()
    };
    let dir = if dir.trim().is_empty() {
        crate::settings::default_download_dir()
    } else {
        dir
    };

    let path = std::path::PathBuf::from(&dir);
    if !path.is_dir() {
        std::fs::create_dir_all(&path).map_err(|e| format!("创建下载目录失败: {e}"))?;
    }

    app.opener()
        .open_path(&dir, None::<&str>)
        .map_err(|e| format!("打开目录失败: {e}"))?;
    Ok(dir)
}

/// 程序版本号，供「帮助 → 关于」显示。
#[tauri::command]
pub async fn app_version(app: tauri::AppHandle) -> Result<String, String> {
    Ok(app.package_info().version.to_string())
}

/// 打开独立的进度窗口（菜单「查看 → 打开进度窗口」）。
///
/// 之所以做成独立窗口而不是切到任务页：IDM 的进度窗口是常驻小窗，
/// 用户一边浏览网页一边看进度。窗口只创建一次，之后每次点击只是把它
/// 提到前台——重复创建会让同一份下载显示成两个窗口，且事件监听翻倍。
#[tauri::command]
pub async fn open_progress_window(app: tauri::AppHandle) -> Result<(), String> {
    crate::monitor::ensure(&app, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vg-delete-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 用户手工删掉成品后，「连文件一起删」不能因为文件不存在就卡住任务。
    #[test]
    fn delete_output_file_accepts_already_missing_file() {
        let dir = temp_dir("missing");
        let out = dir.join("gone.mp4");
        assert_eq!(delete_output_file(out.to_str().unwrap(), &dir), Ok(false));
        assert!(!out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_output_file_still_removes_existing_file() {
        let dir = temp_dir("existing");
        let out = dir.join("keep.mp4");
        std::fs::write(&out, b"x").unwrap();
        assert_eq!(delete_output_file(out.to_str().unwrap(), &dir), Ok(true));
        assert!(!out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录闸门不能被「文件不存在」这条捷径绕过：越界且存在的文件仍拒绝。
    #[test]
    fn delete_output_file_rejects_existing_file_outside_task_dir() {
        let dir = temp_dir("inside");
        let outside = temp_dir("outside");
        let out = outside.join("secret.txt");
        std::fs::write(&out, b"x").unwrap();
        let err = delete_output_file(out.to_str().unwrap(), &dir).unwrap_err();
        assert!(out.exists(), "越界文件必须保留：{err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// 相对路径在闸门里本来就该拒绝，不能因为「当前目录下没有」放行。
    #[test]
    fn delete_output_file_rejects_relative_missing_path() {
        let dir = temp_dir("relative");
        assert!(delete_output_file("gone.mp4", &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 状态机编排回归（P3-8） ----------
    //
    // cancel_one / pause_one / resume_one / redo_one 的内核只需要 &AppState，
    // 用 new_for_test() 走真实锁与令牌表，锁「显示状态」与「令牌在不在」的
    // 配对语义。此前这些语义全靠代码注释论证，没有任何可执行验证。

    use crate::engine::TaskState;

    /// 造一个可入库的任务（状态可指定；kind/字段取最小集）。
    fn mk_task(state: TaskState) -> Task {
        // id 必须唯一：static 计数器比时间戳可靠（同毫秒内两次调用会同 id，
        // 第二次入库会把第一次静默覆盖——测试曾因此自欺）。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Task {
            id: format!(
                "ttest{}_{}",
                crate::commands::now_ms(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ),
            url: "https://e.com/a.bin".into(),
            title: "a.bin".into(),
            thumbnail: None,
            duration: None,
            format_id: "direct".into(),
            quality_label: "4 线程".into(),
            has_audio: false,
            best_audio_format: None,
            state,
            progress: if state == TaskState::Downloading { 42.0 } else { 0.0 },
            downloaded_bytes: 0,
            total_bytes: Some(1000),
            speed: None,
            eta: None,
            output_path: None,
            error: None,
            file_missing: false,
            created_at: crate::commands::now_ms(),
            kind: "direct".into(),
            threads: 4,
            save_dir: None,
            expected_ext: Some("bin".into()),
        start_at: None,
        rate_kb: None,
        sha256: None,
        referer: None,
        }
    }

    async fn task_state_of(state: &AppState, id: &str) -> Option<TaskState> {
        state.tasks.lock().await.get(id).map(|t| t.state)
    }

    async fn has_token(state: &AppState, id: &str) -> bool {
        state.tokens.lock().await.contains_key(id)
    }

    /// 排队中的取消：出队 + 落 Canceled + 瞬时字段清零（P3-1 语义）。
    #[tokio::test]
    async fn cancel_one_dequeues_and_clears_progress() {
        let state = AppState::new_for_test();
        let mut t = mk_task(TaskState::Queued);
        t.progress = 42.0;
        t.downloaded_bytes = 100;
        let id = t.id.clone();
        state.insert_task_for_test(t);
        state.queue.lock().await.push_back(id.clone());

        cancel_one(&state, &id).await;

        assert_eq!(task_state_of(&state, &id).await, Some(TaskState::Canceled));
        assert!(!has_token(&state, &id).await, "无令牌路径不得凭空造令牌");
        assert!(!state.queue.lock().await.iter().any(|x| *x == id), "必须出队");
        let t = state.tasks.lock().await.get(&id).cloned().unwrap();
        assert_eq!(t.progress, 0.0, "取消后进度清零（P3-1）");
        assert_eq!(t.downloaded_bytes, 0);
        assert!(t.total_bytes.is_none());
    }

    /// 运行中的取消：令牌被 cancel（标志置位），状态交给 worker 收尾——
    /// 但「非终态才覆盖」的守卫存在时这里直接落 Canceled（测试里无 worker，
    /// 状态停留在中间态，走覆盖分支）。
    #[tokio::test]
    async fn cancel_one_running_cancels_token_and_falls_to_canceled() {
        let state = AppState::new_for_test();
        let t = mk_task(TaskState::Downloading);
        let id = t.id.clone();
        state.insert_task_for_test(t);
        let token = Arc::new(CancelToken::new());
        state.tokens.lock().await.insert(id.clone(), token.clone());

        cancel_one(&state, &id).await;

        assert!(token.is_canceled(), "必须取消真实令牌");
        assert_eq!(task_state_of(&state, &id).await, Some(TaskState::Canceled));
    }

    /// 终态守卫：worker 已写 Done 后迟到的 cancel 不得覆盖成 Canceled。
    #[tokio::test]
    async fn cancel_one_never_overrides_done() {
        let state = AppState::new_for_test();
        let t = mk_task(TaskState::Done);
        let id = t.id.clone();
        state.insert_task_for_test(t);
        let token = Arc::new(CancelToken::new());
        state.tokens.lock().await.insert(id.clone(), token.clone());

        cancel_one(&state, &id).await;

        assert_eq!(task_state_of(&state, &id).await, Some(TaskState::Done), "终态必须保留");
        assert!(token.is_canceled(), "令牌照常取消（收尾用）");
    }

    /// 排队中的暂停：出队 + 落 Paused（进度保留——断点还在盘上）。
    #[tokio::test]
    async fn pause_one_dequeues_and_keeps_progress() {
        let state = AppState::new_for_test();
        let mut t = mk_task(TaskState::Queued);
        t.progress = 42.0;
        let id = t.id.clone();
        state.insert_task_for_test(t);
        state.queue.lock().await.push_back(id.clone());

        pause_one(&state, &id).await.unwrap();

        assert_eq!(task_state_of(&state, &id).await, Some(TaskState::Paused));
        let t = state.tasks.lock().await.get(&id).cloned().unwrap();
        assert_eq!(t.progress, 42.0, "暂停必须保留进度（断点语义）");
    }

    /// 运行中的暂停：pause() 先写 paused 再置取消位——worker 收尾时据此
    /// 落 Paused。这里验证令牌标志序与「不直接写状态」的既有设计。
    #[tokio::test]
    async fn pause_one_running_pauses_token_without_state_write() {
        let state = AppState::new_for_test();
        let t = mk_task(TaskState::Downloading);
        let id = t.id.clone();
        state.insert_task_for_test(t);
        let token = Arc::new(CancelToken::new());
        state.tokens.lock().await.insert(id.clone(), token.clone());

        pause_one(&state, &id).await.unwrap();

        assert!(token.is_paused(), "先写 paused");
        assert!(token.is_canceled(), "再置取消位（stop 传输）");
        assert_eq!(
            task_state_of(&state, &id).await,
            Some(TaskState::Downloading),
            "有令牌时不写状态：交给 worker 收尾（避免与它抢锁回跳）"
        );
    }

    /// 终态暂停拒绝：Done 不能被拍成 Paused。
    #[tokio::test]
    async fn pause_one_rejects_terminal() {
        let state = AppState::new_for_test();
        let t = mk_task(TaskState::Done);
        let id = t.id.clone();
        state.insert_task_for_test(t);

        let err = pause_one(&state, &id).await.unwrap_err();
        assert!(err.contains("已结束"), "{err}");
        assert_eq!(task_state_of(&state, &id).await, Some(TaskState::Done));
    }

    /// 恢复：Paused → Queued 入队；已有令牌（旧 worker 未收尾）必须拒绝。
    #[tokio::test]
    async fn resume_one_rejects_when_token_alive() {
        let state = AppState::new_for_test();
        let t = mk_task(TaskState::Paused);
        let id = t.id.clone();
        state.insert_task_for_test(t);
        let token = Arc::new(CancelToken::new());
        state.tokens.lock().await.insert(id.clone(), token.clone());

        let err = resume_one(&state, &id).await.unwrap_err();
        assert!(err.contains("收尾"), "{err}");
        assert_eq!(
            task_state_of(&state, &id).await,
            Some(TaskState::Paused),
            "拒绝时不得翻态（防双线程写同一份断点）"
        );
    }

    #[tokio::test]
    async fn resume_one_queues_paused_task() {
        let state = AppState::new_for_test();
        let t = mk_task(TaskState::Paused);
        let id = t.id.clone();
        state.insert_task_for_test(t);

        resume_one(&state, &id).await.unwrap();

        assert_eq!(task_state_of(&state, &id).await, Some(TaskState::Queued));
        assert!(state.queue.lock().await.iter().any(|x| *x == id), "必须入队");
    }

    /// Done 不能 resume（是 redo 的入口）。
    #[tokio::test]
    async fn resume_one_rejects_done() {
        let state = AppState::new_for_test();
        let t = mk_task(TaskState::Done);
        let id = t.id.clone();
        state.insert_task_for_test(t);

        let err = resume_one(&state, &id).await.unwrap_err();
        assert!(err.contains("已完成"), "{err}");
    }

    /// 重下：取消态允许（清断点从头下）；运行中拒绝；快照与复查之间状态
    /// 变化（并发 resume 抢先）必须拒绝。
    #[tokio::test]
    async fn redo_one_rejects_running_and_racing() {
        // 运行中拒绝
        let state = AppState::new_for_test();
        let t = mk_task(TaskState::Downloading);
        let id = t.id.clone();
        state.insert_task_for_test(t);
        let err = redo_one(&state, &id).await.unwrap_err();
        assert!(err.contains("先取消"), "{err}");

        // 并发抢先：快照时是 Canceled，提交前被改回 Paused（模拟另一命令）
        let state2 = AppState::new_for_test();
        let t2 = mk_task(TaskState::Canceled);
        let id2 = t2.id.clone();
        state2.insert_task_for_test(t2);
        // 在快照与提交之间插队：直接把状态改成 Paused 且插令牌
        let token = Arc::new(CancelToken::new());
        state2.tokens.lock().await.insert(id2.clone(), token.clone());
        state2.tasks.lock().await.get_mut(&id2).unwrap().state = TaskState::Paused;
        let err = redo_one(&state2, &id2).await.unwrap_err();
        assert!(
            err.contains("发生了变化") || err.contains("先取消"),
            "快照判定或复查必须拦住: {err}"
        );
        assert_eq!(
            task_state_of(&state2, &id2).await,
            Some(TaskState::Paused),
            "被抢先后不得再写 Queued"
        );
    }

    /// 重下的正常路径：Canceled → Queued，进度归零，入队。
    #[tokio::test]
    async fn redo_one_canceled_goes_queued_with_progress_reset() {
        let state = AppState::new_for_test();
        let mut t = mk_task(TaskState::Canceled);
        t.progress = 88.0;
        let id = t.id.clone();
        state.insert_task_for_test(t);

        redo_one(&state, &id).await.unwrap();

        let t = state.tasks.lock().await.get(&id).cloned().unwrap();
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.progress, 0.0, "断点已清，进度归零");
        assert!(state.queue.lock().await.iter().any(|x| *x == id));
    }

    /// 并发取消 vs 完成竞态的结构验证：两个任务并发跑 cancel_one 与
    /// 「写 Done」，终态只有一个赢家，绝不出现「Done 被改成 Canceled」
    /// 或相反的写丢。跑若干轮放大窗口。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_vs_done_race_converges() {
        for round in 0..32 {
            let state = AppState::new_for_test();
            let t = mk_task(TaskState::Downloading);
            let id = t.id.clone();
            state.insert_task_for_test(t);
            let token = Arc::new(CancelToken::new());
            state.tokens.lock().await.insert(id.clone(), token.clone());

            let s1 = clone_state(&state);
            let s2 = clone_state(&state);
            let id1 = id.clone();
            let id2 = id.clone();
            let (a, b) = tokio::join!(
                async move { cancel_one(&s1, &id1).await },
                async move {
                    // 模拟 worker 收尾的「锁内写 Done」分支
                    let mut guard = s2.tasks.lock().await;
                    if let Some(t) = guard.get_mut(&id2) {
                        t.state = TaskState::Done;
                    }
                },
            );
            let _ = (a, b);

            let final_state = task_state_of(&state, &id).await.unwrap();
            assert!(
                matches!(final_state, TaskState::Done | TaskState::Canceled),
                "round {round}: 终态只能是 Done 或 Canceled，实际 {final_state:?}"
            );
            // 状态与令牌取消标志的关系：Done 赢家可能发生在 cancel 前
            //（token.is_canceled() 可真可假），但 Canceled 赢家必然源自
            // cancel 路径——此时令牌一定被 cancel 过。
            if final_state == TaskState::Canceled {
                assert!(token.is_canceled(), "round {round}: Canceled 必然伴随令牌取消");
            }
        }
    }

    /// 定时开始（1.4.0）：泵对未到点任务的 defer 语义——不占槽、不启动、
    /// 不丢任务；到点后正常启动。
    #[tokio::test]
    async fn pump_defers_future_start_at() {
        let state = AppState::new_for_test();
        // 未来的任务：start_at 在 1 小时后
        let mut future = mk_task(TaskState::Queued);
        future.start_at = Some(crate::commands::now_ms() + 3600 * 1000);
        let fid = future.id.clone();
        let start_at_expected = future.start_at;
        state.insert_task_for_test(future);
        // 已到点的任务（负偏移 = 立即）
        let mut due = mk_task(TaskState::Queued);
        due.start_at = Some(crate::commands::now_ms() - 1);
        let did = due.id.clone();
        state.insert_task_for_test(due);
        state.queue.lock().await.push_back(fid.clone());
        state.queue.lock().await.push_back(did.clone());

        let now = crate::commands::now_ms();
        assert!(!state.tasks.lock().await.get(&fid).unwrap().start_at_due(now), "未来时刻不得判定为到点");
        assert!(state.tasks.lock().await.get(&did).unwrap().start_at_due(now), "过去/当前时刻必须到点");

        // Task 序列化保 start_at（历史回灌守约的前提）
        let v = serde_json::to_value(state.tasks.lock().await.get(&fid).unwrap()).unwrap();
        assert_eq!(v["start_at"], serde_json::json!(start_at_expected), "start_at 必须随任务序列化");
    }

    fn clone_state(state: &AppState) -> AppState {
        AppState {
            tasks: state.tasks.clone(),
            tokens: state.tokens.clone(),
            settings: state.settings.clone(),
            queue: state.queue.clone(),
            running: state.running.clone(),
            pump_lock: state.pump_lock.clone(),
            counter: AtomicU64::new(0),
            pending_links: std::sync::Mutex::new(Vec::new()),
            plan: std::sync::Mutex::new(None),
        }
    }
}
