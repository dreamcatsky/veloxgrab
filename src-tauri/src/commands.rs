use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::direct::{self, DirectProbe};
use crate::engine::{self, CancelToken, Task, TaskState};
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
    /// 启动阶段收到的 `vgrab://` 链接，等前端来取。
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
        // 回灌出来的都是终态任务，不进队列、不会自己开始下载。
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
/// Tauri 在 Windows 上给出的 resource_dir 带 `\\?\` verbatim 前缀（`\\?\D:\Tools\VGrab`）。
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
    direct::probe_direct(&url, None, &crate::settings::NetCfg::from_settings(&settings)).await
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
                let net = crate::settings::NetCfg::from_settings(&settings);
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
    // 先校验保存目录可写，避免任务排进去才失败。
    // 用任务自己的目录：用户指定的盘符不存在时，这里就该把话说清楚。
    {
        let settings = load_settings(&state);
        let dir = args
            .save_dir
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(&settings.download_dir));
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
    }

    let n = state.counter.fetch_add(1, Ordering::SeqCst);
    let id = format!("t{}_{}", now_ms(), n);

    let kind = args.kind.unwrap_or_else(|| "video".into());

    let task = Task {
        id: id.clone(),
        url: args.url,
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
        // 空串与 None 一视同仁，都表示「用全局默认目录」
        save_dir: args
            .save_dir
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty()),
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

            // 从取出到启动之间，cancel_task 可能刚好把这个任务标成已取消
            // （它这时已经不在队列里，走的是 token 分支）。这里做最后一次确认，
            // 顺手把状态推到 Downloading——否则取消掉的任务还会被拉起来跑一遍。
            let runnable = {
                let mut guard = ctx.tasks.lock().await;
                match guard.get_mut(&next_id) {
                    Some(t) if !t.state.is_terminal() && t.state != TaskState::Paused => {
                        t.state = TaskState::Downloading;
                        true
                    }
                    _ => false,
                }
            };
            if !runnable {
                continue; // 槽位随作用域释放
            }

            let token = Arc::new(CancelToken::new());
            {
                let mut t = ctx.tokens.lock().await;
                t.insert(next_id.clone(), token.clone());
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

                    let mut t = ctx_c.tokens.lock().await;
                    t.remove(&task_c.id);
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
    // 排队中的任务：直接从队列移除并标记取消
    {
        let mut q = state.queue.lock().await;
        if let Some(pos) = q.iter().position(|x| x == &id) {
            let _ = q.remove(pos);
            let mut guard = state.tasks.lock().await;
            if let Some(t) = guard.get_mut(&id) {
                t.state = TaskState::Canceled;
                t.error = Some("已取消".into());
                // 入队即落盘的取消态也要记进历史
                crate::history::upsert_entry(t);
            }
            return Ok(());
        }
    }

    // 运行中的任务：置取消标志并杀掉进程树
    let token = {
        let t = state.tokens.lock().await;
        t.get(&id).cloned()
    };
    if let Some(token) = token {
        token.cancel();
        engine::kill_process_tree(token.pid());
    }

    let mut guard = state.tasks.lock().await;
    if let Some(t) = guard.get_mut(&id) {
        if !t.state.is_terminal() {
            t.state = TaskState::Canceled;
            t.error = Some("已取消".into());
        }
        crate::history::upsert_entry(t);
    }
    Ok(())
}

/// 暂停：停止传输但保留断点。
///
/// 与 `cancel_task` 几乎一样，关键差别只有两处：
/// 1. 用 `token.pause()` 而不是 `cancel()`——它先写 paused 标志再置取消位，
///    工作线程收尾时据此落成 `Paused` 而不是 `Canceled`。
/// 2. 队列里的任务只出队不销毁，状态落成 Paused，留着等恢复。
#[tauri::command]
pub async fn pause_task(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    // 排队中的任务：直接出队，状态落成暂停
    {
        let mut q = state.queue.lock().await;
        if let Some(pos) = q.iter().position(|x| x == &id) {
            let _ = q.remove(pos);
            let mut guard = state.tasks.lock().await;
            if let Some(t) = guard.get_mut(&id) {
                t.state = TaskState::Paused;
                t.error = None;
            }
            return Ok(());
        }
    }

    // 运行中的任务：置暂停标志并杀掉进程树。
    // 这里必须先调 pause() 再 kill：pause() 会把 paused 置位，工作线程在
    // `child.wait()` 返回后会读到它，从而把状态写成 Paused。
    // 如果先 kill 再 pause，中间那段窗口足够让工作线程落成 Canceled，
    // 用户点暂停却得到一条已取消的任务，断点还在但再也恢复不了。
    let token = {
        let t = state.tokens.lock().await;
        t.get(&id).cloned()
    };
    if let Some(token) = token {
        token.pause();
        engine::kill_process_tree(token.pid());
    } else {
        // 没有 token 说明它既不在队列也没在跑（比如已经是终态）。
        // 直接把状态写成暂停，让用户至少能看到意图落库。
        let mut guard = state.tasks.lock().await;
        if let Some(t) = guard.get_mut(&id) {
            if !t.state.is_terminal() {
                t.state = TaskState::Paused;
                t.error = None;
            }
        }
        return Ok(());
    }

    // 状态由工作线程收尾时写；这里不改，避免和它抢同一把锁造成状态回跳。
    Ok(())
}

/// 恢复：把暂停/失败/排队中的任务重新送进队列。
///
/// 断点续传靠的是各下载器自己的临时文件（直链 `.vgrab-part` + `.vgrab-meta`、
/// yt-dlp 的 `.part`），所以恢复不需要我们搬数据，只要重新跑一遍同一个任务。
#[tauri::command]
pub async fn resume_task(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    use tauri::Emitter;

    // 先把它从队列里摘掉（如果还在），避免同一个 id 出现两次
    {
        let mut q = state.queue.lock().await;
        q.retain(|x| x != &id);
    }

    {
        let mut guard = state.tasks.lock().await;
        match guard.get_mut(&id) {
            Some(t) => {
                if t.state.is_terminal() {
                    return Err("该任务已结束，无法恢复".into());
                }
                // 先把状态改回排队，队列泵只认这个状态
                t.state = TaskState::Queued;
                t.error = None;
            }
            None => return Err("任务不存在".into()),
        }
    }

    // 暂停期间它的取消令牌仍然带着 flag=true（甚至 paused=true），
    // 直接复用会让新的一轮下载刚启动就立刻停止。摘掉，真正开跑时泵会新建。
    {
        let mut tokens = state.tokens.lock().await;
        tokens.remove(&id);
    }

    {
        let mut q = state.queue.lock().await;
        q.push_back(id.clone());
    }

    let _ = app.emit("tasks-changed", ());

    // 不 await：泵只负责在限额内启动任务
    let ctx = PumpCtx::from_state(&state);
    tauri::async_runtime::spawn(pump_queue(app, ctx));
    Ok(())
}

#[tauri::command]
pub async fn remove_task(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    // 先留一份快照：从任务表删掉之后就没有 title / kind / url，没法再清理临时文件了
    let snapshot = {
        let guard = state.tasks.lock().await;
        guard.get(&id).cloned()
    };

    // 先取消再删。只从 map 里移除的话，正在跑的下载既停不下来，
    // 界面上又已经没有这个任务，用户再也找不到取消入口——子进程会一直跑到结束。
    {
        let mut q = state.queue.lock().await;
        q.retain(|x| x != &id);
    }
    let token = {
        let t = state.tokens.lock().await;
        t.get(&id).cloned()
    };
    if let Some(token) = token {
        token.cancel();
        engine::kill_process_tree(token.pid());
    }
    {
        let mut guard = state.tasks.lock().await;
        guard.remove(&id);
    }
    // 历史里也删掉：启动时历史会回灌进任务表，只清内存的话下次启动它又回来了。
    let _ = crate::history::remove_entry(&id);

    // 只清理已经结束的任务：运行中的任务还在写文件，删掉也会被重新创建。
    // 目录要用任务自己的 save_dir，不是全局默认目录——否则给任务单独设了目录
    // 的用户，移除任务时半成品会留在原地没人管。
    if let Some(t) = snapshot.filter(|t| t.state.is_terminal()) {
        let settings = load_settings(&state);
        let dir = engine::resolve_task_dir(&settings, &t)
            .unwrap_or_else(|_| std::path::PathBuf::from(&settings.download_dir));
        engine::cleanup_task_artifacts(&dir, &t);
    }
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

/// `new_download` 与 `vgrab://` 深链共用的建任务逻辑。
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

    // 目录校验前置：盘符不存在时立刻报错，而不是等下载失败
    let save_dir = save_dir
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    {
        let settings = load_settings(state);
        let dir = save_dir
            .clone()
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
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string());
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

/// 取消所有任务（用于退出前清理）
pub async fn cancel_all(state: &AppState) {
    let tokens = {
        let t = state.tokens.lock().await;
        t.values().cloned().collect::<Vec<_>>()
    };
    for token in tokens {
        token.cancel();
        engine::kill_process_tree(token.pid());
    }
}

/// 取走启动阶段收到的 `vgrab://` 链接（取走即清空）。
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

    {
        let mut q = state.queue.lock().await;
        for id in &ids {
            // 去重：已经在队列里的不要再塞一次，否则同一个任务会被启动两遍
            if !q.iter().any(|x| x == id) {
                q.push_back(id.clone());
            }
        }
    }
    {
        let mut guard = state.tasks.lock().await;
        for id in &ids {
            if let Some(t) = guard.get_mut(id) {
                t.state = TaskState::Queued;
                t.error = None;
            }
        }
    }
    // 暂停时留下的令牌带着 flag=true，不清掉会让重启的任务立刻停下
    {
        let mut tokens = state.tokens.lock().await;
        for id in &ids {
            tokens.remove(id);
        }
    }

    let _ = app.emit("tasks-changed", ());
    let ctx = PumpCtx::from_state(&state);
    tauri::async_runtime::spawn(pump_queue(app, ctx));
    Ok(ids.len())
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

    let tokens: Vec<(String, Arc<CancelToken>)> = {
        let t = state.tokens.lock().await;
        ids.iter()
            .filter_map(|id| t.get(id).map(|tok| (id.clone(), tok.clone())))
            .collect()
    };
    for (_, token) in &tokens {
        token.pause();
        engine::kill_process_tree(token.pid());
    }

    // 排队中（没有令牌）的任务不会被工作线程收尾，这里直接落状态；
    // 运行中的交给工作线程写，避免两边抢锁导致状态回跳。
    {
        let mut guard = state.tasks.lock().await;
        for id in &ids {
            if tokens.iter().any(|(tid, _)| tid == id) {
                continue;
            }
            if let Some(t) = guard.get_mut(id) {
                if !t.state.is_terminal() {
                    t.state = TaskState::Paused;
                    t.error = None;
                }
            }
        }
    }

    let _ = app.emit("tasks-changed", ());
    Ok(ids.len())
}

/// 恢复选中 / 暂停选中 / 删除选中 的公共入参。
#[derive(Deserialize)]
pub struct IdsArgs {
    pub ids: Vec<String>,
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
            }
            return Ok(());
        }
    }

    let token = {
        let t = state.tokens.lock().await;
        t.get(id).cloned()
    };
    match token {
        Some(token) => {
            token.pause();
            engine::kill_process_tree(token.pid());
            Ok(())
        }
        None => {
            // 既不在队列也没令牌：可能已经结束，也可能还没启动。后者也允许暂停。
            let mut guard = state.tasks.lock().await;
            match guard.get_mut(id) {
                Some(t) if !t.state.is_terminal() => {
                    t.state = TaskState::Paused;
                    t.error = None;
                    Ok(())
                }
                Some(_) => Err("该任务已结束".into()),
                None => Err("任务不存在".into()),
            }
        }
    }
}

/// 单个任务的恢复内核（`resume_task` 与 `resume_tasks` 共用）。
async fn resume_one(state: &AppState, id: &str) -> Result<(), String> {
    {
        let mut q = state.queue.lock().await;
        q.retain(|x| x != id);
    }
    {
        let mut guard = state.tasks.lock().await;
        match guard.get_mut(id) {
            Some(t) => {
                if t.state.is_terminal() {
                    return Err("该任务已结束，无法恢复".into());
                }
                t.state = TaskState::Queued;
                t.error = None;
            }
            None => return Err("任务不存在".into()),
        }
    }
    // 暂停期间留下的令牌仍有 flag=true，复用会让新一轮立刻停下
    {
        let mut tokens = state.tokens.lock().await;
        tokens.remove(id);
    }
    {
        let mut q = state.queue.lock().await;
        q.push_back(id.to_string());
    }
    Ok(())
}

/// 批量删除：先杀进程、清临时文件，再从任务表移除。
#[tauri::command]
pub async fn remove_tasks(
    state: tauri::State<'_, AppState>,
    args: IdsArgs,
) -> Result<usize, String> {
    let mut ok = 0;
    for id in args.ids {
        if remove_one(&state, &id).await {
            ok += 1;
        }
    }
    Ok(ok)
}

/// 删除单个任务的完整流程，返回是否真的删掉了。
///
/// 与 `remove_task` 命令分开是为了让批量版不必重复实现「避免泄漏临时文件」
/// 的那段逻辑——那段逻辑少一句就会在下载目录里留下半个 GB 的残片。
async fn remove_one(state: &AppState, id: &str) -> bool {
    // 还在跑就先停：不然删了任务表，进程还在写文件
    if let Some(token) = {
        let t = state.tokens.lock().await;
        t.get(id).cloned()
    } {
        token.cancel();
        engine::kill_process_tree(token.pid());
    }

    {
        let mut q = state.queue.lock().await;
        q.retain(|x| x != id);
    }

    let snapshot = {
        let guard = state.tasks.lock().await;
        guard.get(id).cloned()
    };

    if let Some(task) = &snapshot {
        // 删除时清临时文件：用户删掉任务就是要它彻底消失，
        // 留着 .vgrab-part 下次同名下载会莫名其妙地续上一个旧进度。
        // 目录必须用任务自己的 save_dir：单独设过目录的任务，
        // 用全局默认目录去找会扑空，残片就永远留在那儿。
        let settings = load_settings(state);
        let dir = engine::resolve_task_dir(&settings, task)
            .unwrap_or_else(|_| std::path::PathBuf::from(&settings.download_dir));
        engine::cleanup_task_artifacts(&dir, task);
    }

    let mut guard = state.tasks.lock().await;
    let gone = guard.remove(id).is_some();
    // 历史也得删：启动时历史会回灌进任务表，只清内存的话下次启动它又回来了。
    let _ = crate::history::remove_entry(id);
    gone
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
        .title("VGrab 进度")
        .inner_size(560.0, 420.0)
        .min_inner_size(420.0, 260.0)
        .resizable(true)
        .always_on_top(false)
        .build()
        .map_err(|e| format!("打开进度窗口失败: {e}"))?;

    Ok(())
}
