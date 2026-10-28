use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

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
    pub settings: std::sync::Mutex<Settings>,
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
            settings: std::sync::Mutex::new(settings),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            running: Arc::new(AtomicU64::new(0)),
            pump_lock: Arc::new(Mutex::new(())),
            counter: AtomicU64::new(1),
            pending_links: std::sync::Mutex::new(Vec::new()),
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
             确定要下载到这个位置吗？拒绝后将使用默认下载目录。"
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
    let net = crate::settings::NetCfg::from_settings(&settings);
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
    {
        let settings = load_settings(&state);
        let dir = save_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(&settings.download_dir));
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
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
        created_at: now_ms(),
        kind: kind.clone(),
        // 0 表示交给设置里的默认值；非 0 也夹到 64（与 direct::MAX_THREADS 一致），
        // 防止 IPC 传入离谱的并发数
        threads: args.threads.unwrap_or(0).min(64),
        // 闸门的定稿路径（None = 用全局默认目录）。原始 args.save_dir 不得
        // 再往下传——那是未经折叠/校验的原始输入。
        save_dir,
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
}

impl PumpCtx {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            tasks: state.tasks.clone(),
            tokens: state.tokens.clone(),
            queue: state.queue.clone(),
            running: state.running.clone(),
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

        // 每次续泵都重读一次设置，用户改了并发数能立刻生效
        let max_concurrent = crate::settings::read_settings_from_disk()
            .max_concurrent
            .max(1);

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
                            true
                        }
                    }
                    _ => false,
                }
            };
            if !started {
                continue; // 槽位随作用域释放；失败分支没插过令牌，无需回收
            }

            let settings = crate::settings::read_settings_from_disk();
            let app_c = app.clone();
            let ctx_c = ctx.clone();
            let task_c = task.clone();

            tauri::async_runtime::spawn(async move {
                {
                    // 槽位在作用域结束时归还，panic 也不会泄漏
                    let _slot = slot;

                    if task_c.kind == "direct" {
                        let _ = engine::run_direct_download(
                            app_c.clone(),
                            task_c.clone(),
                            settings,
                            token.clone(),
                            ctx_c.tasks.clone(),
                        )
                        .await;
                    } else {
                        let _ = engine::run_download(
                            app_c.clone(),
                            task_c.clone(),
                            settings,
                            token.clone(),
                            ctx_c.tasks.clone(),
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

                // 关键：真正把后续排队任务泵起来。
                // 少了这一步，超过并发数的任务会永远停在「排队中」。
                pump_queue(app_c, ctx_c).await;
            });
        }
    })
}

#[tauri::command]
pub async fn list_tasks(state: tauri::State<'_, AppState>) -> Result<Vec<Task>, String> {
    let guard = state.tasks.lock().await;
    let mut v: Vec<Task> = Vec::with_capacity(guard.len());
    v.extend(guard.values().cloned());
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
                    crate::history::upsert_entry(t);
                }
            }
            Some(tok.clone())
        } else {
            if let Some(t) = guard.get_mut(id) {
                if !t.state.is_terminal() {
                    t.state = TaskState::Canceled;
                    t.error = Some("已取消".into());
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
async fn redo_one(state: &AppState, id: &str) -> Result<(), String> {
    // 「查令牌 + 查状态 + 置 Queued + 入队」合并进一个 tasks 临界区（锁序
    // tasks→tokens 与泵一致）。旧写法先单独查令牌再单独查状态，两次加锁之间
    // 泵可以完成原子提交——拿着「还没启动」的结论却遇到已经在跑的 worker，
    // 清断点与写盘打架；末尾那次无条件摘令牌又正好把泵刚插的新轮令牌拆掉。
    // 合并后这些组合都不存在：要么看到真令牌拒绝，要么状态转换与令牌表一致。
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
        // Done 拒绝的理由已经消失一半：finish_download 现在撞名递增避让，
        // 不再静默覆盖成品。但重下同一个成品仍然会白跑一遍网络再原地多出一份
        // 重复文件——对用户还是意外，维持拒绝。
        TaskState::Done => return Err("任务已完成，如需再下请先移除后新建".into()),
        TaskState::Canceled => {
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
                let was_canceled = t.state == TaskState::Canceled;
                t.state = TaskState::Queued;
                t.error = None;
                if was_canceled {
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
}

#[tauri::command]
pub async fn new_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: NewDownloadArgs,
) -> Result<Task, String> {
    create_direct_task(
        &app,
        &state,
        args.url,
        args.filename,
        args.threads,
        args.save_dir,
    )
    .await
}

/// `new_download` 与 `veloxgrab://` 深链共用的建任务逻辑。
///
/// 单独抽出来是因为深链进来时没有「前端表单」可依赖：扩展只传一个 URL，
/// 其余全靠这里兜。两条入口必须走同一条路径，否则会出现「网页点能下、
/// 手动粘同一个地址反而失败」这种最难排查的分裂。
pub(crate) async fn create_direct_task(
    app: &tauri::AppHandle,
    state: &AppState,
    url: String,
    filename: Option<String>,
    threads: Option<usize>,
    save_dir: Option<String>,
) -> Result<Task, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("请输入下载地址".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("地址必须以 http:// 或 https:// 开头".into());
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
        created_at: now_ms(),
        kind: "direct".into(),
        threads: threads.unwrap_or(0).min(64),
        save_dir: save_dir.clone(),
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

/// 清空已结束的任务。
///
/// 同时把对应条目从历史里删掉：启动时历史会回灌进任务表，只清内存的话
/// 用户以为清掉的条目下次启动又回来了，等于这个按钮在骗人。
#[tauri::command]
pub async fn clear_finished(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let removed: Vec<String> = {
        let mut guard = state.tasks.lock().await;
        let before = guard.len();
        let mut ids = Vec::new();
        guard.retain(|id, t| {
            if t.state.is_terminal() {
                ids.push(id.clone());
                false
            } else {
                true
            }
        });
        if before == guard.len() {
            return Ok(0);
        }
        ids
    };

    for id in &removed {
        let _ = crate::history::remove_entry(id);
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
                        Ok(()) => outcome.file_deleted = true,
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
/// 三层处理：精确路径 → 编码损坏的兜底查找 → 明确报错。
/// 单独抽出来是因为它同时要照顾「路径合法但文件被占用」和
/// 「路径本身是坏的（历史遗留 cp936 编码）」两类情况。
fn delete_output_file(output_path: &str, task_dir: &std::path::Path) -> Result<(), String> {
    let target = match save_dir::check_deletable_file(output_path, task_dir) {
        Ok(p) => p,
        Err(why) => {
            // 精确路径不通时，先试「编码损坏的历史记录」救援路径。
            // 老版本把 yt-dlp 的 cp936 输出当 UTF-8 解，中文名存成了问号，
            // 这些任务的 output_path 指向一个不存在的文件。
            match save_dir::find_file_for_broken_path(output_path, task_dir) {
                Some(p) => p,
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
        Ok(()) => Ok(()),
        // 已经不在了：用户要的结果已经达成，不算失败
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "文件".into());
            Err(format!("{name}：{e}（可能正被其他程序占用）"))
        }
    }
}

/// 清空所有任务（不分状态），同时删掉各自的临时文件。
#[tauri::command]
pub async fn clear_all_tasks(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    use tauri::Emitter;

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
    crate::clipboard::write_clipboard_text(&text)
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
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

    if let Some(win) = app.get_webview_window("progress") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return Ok(());
    }

    WebviewWindowBuilder::new(&app, "progress", WebviewUrl::App("progress.html".into()))
        .title("VeloxGrab 进度")
        .inner_size(560.0, 420.0)
        .min_inner_size(420.0, 260.0)
        .resizable(true)
        .always_on_top(false)
        .build()
        .map_err(|e| format!("打开进度窗口失败: {e}"))?;

    Ok(())
}
