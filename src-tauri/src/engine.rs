use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::settings::Settings;
use crate::ytdlp::{push_common_args, resolve_ytdlp};

/// 按行读取，并按系统代码页解码（见 decode.rs）。
///
/// 不能用 `BufReader::lines()`：它遇到非法 UTF-8 会返回 `Err(InvalidData)`，
/// 而 Windows 上 yt-dlp 输出的路径/标题未必是 UTF-8。读取一旦中断，子进程的
/// 管道没人消费会被写满，`child.wait()` 就会永远卡住。
///
/// 也不能只用 `from_utf8_lossy`：实测 yt-dlp.exe 在简中系统吐 cp936 字节，
/// lossy 会把标题的每个汉字变成 `?`——`--print after_move:filepath` 的输出
/// 因此存成坏路径，之后打开/定位/删除全部 `os error 2`。解码细节与实测证据
/// 见 decode.rs 头注释。
async fn next_line_lossy<R>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    buf.clear();
    let n = reader.read_until(b'\n', buf).await?;
    if n == 0 {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
    Ok(Some(crate::decode::decode_output(buf)))
}

/// 任务状态机
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Downloading,
    Merging,
    Done,
    Failed,
    Canceled,
    /// 用户主动暂停。
    ///
    /// 与 `Canceled` 的区别是「能不能接着跑」：取消是终态，任务再也回不来；
    /// 暂停保留断点（直链的 `.veloxgrab-part` / yt-dlp 的 `.part`），恢复时从原位置续传。
    /// 所以它**不能**算终态——否则「清除已结束」会把用户刚暂停的任务一起扫掉，
    /// 而且队列泵会把它当成已死任务跳过。
    Paused,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Failed | TaskState::Canceled
        )
    }

    /// 是否处于「可以让它继续跑」的状态：排队、暂停、失败都可以重启。
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            TaskState::Queued | TaskState::Paused | TaskState::Failed
        )
    }
}

/// 单个下载任务
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub url: String,
    pub title: String,
    pub thumbnail: Option<String>,
    pub duration: Option<f64>,
    pub format_id: String,
    pub quality_label: String,
    /// 该 video 格式是否自带音轨；false 时下载需拼接独立音轨
    pub has_audio: bool,
    /// 探测阶段选出的最佳纯音频流 format_id，用于拼接
    pub best_audio_format: Option<String>,
    pub state: TaskState,
    pub progress: f64,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed: Option<String>,
    pub eta: Option<String>,
    pub output_path: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    /// "video" = yt-dlp 解析下载；"direct" = HTTP Range 多线程直链
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 直链下载的并发分块数（0 表示自动）
    #[serde(default)]
    pub threads: usize,
    /// 本任务的保存目录；None 表示用设置里的全局默认目录。
    ///
    /// IDM 的核心体验之一就是「每个任务可以单独指定存哪」。之前只有全局
    /// 默认目录，用户想把这个视频丢到 D 盘、那个压缩包丢到桌面，只能手动挪。
    #[serde(default)]
    pub save_dir: Option<String>,
}

fn default_kind() -> String {
    "video".into()
}

/// 进度事件负载（emit 到前端）
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub id: String,
    pub state: TaskState,
    pub progress: f64,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed: Option<String>,
    pub eta: Option<String>,
    pub error: Option<String>,
    pub output_path: Option<String>,
}

/// 取消句柄：每个运行中的任务持有一个
pub struct CancelToken {
    flag: AtomicBool,
    /// 本次停止是「暂停」还是「取消」。
    ///
    /// 两者都要立刻停下传输，但落库的状态不同：暂停要保留断点等恢复，
    /// 取消是终态。用一个独立的标志而不是两个 token，是因为停止的传播路径
    /// （杀掉 yt-dlp 进程树 / 置直链的共享 AtomicBool）完全一样，
    /// 只有事后写什么状态不同——再复制一套停止逻辑只会让两边慢慢分叉。
    paused: AtomicBool,
    /// yt-dlp 子进程 pid，用于强杀（取消时 Windows 上终止子进程树）
    pid: AtomicU64,
    /// 直链下载用的共享取消标志（与 direct 模块共享同一个 AtomicBool）
    shared: std::sync::Mutex<Option<Arc<AtomicBool>>>,
}

/// 默认构造等价于 `new()`：几个原子变量清零，没有副作用。
impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            pid: AtomicU64::new(0),
            shared: std::sync::Mutex::new(None),
        }
    }
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Ok(g) = self.shared.lock() {
            if let Some(f) = g.as_ref() {
                f.store(true, Ordering::SeqCst);
            }
        }
    }
    /// 暂停：停止传输但把意图记成「可恢复」。
    ///
    /// 先写 paused 再置 flag：停止是个异步过程，工作线程随时可能在中途读到
    /// flag 并立刻落库，那时代码会去问 `is_paused()`。顺序反过来的话，
    /// 就有窗口把「暂停」误记成「取消」，断点信息虽然还在，任务却再也回不来。
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.cancel();
    }
    pub fn is_canceled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
    pub fn set_pid(&self, pid: u32) {
        self.pid.store(pid as u64, Ordering::SeqCst);
    }
    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::SeqCst) as u32
    }
    /// 注册一个外部共享的取消标志，使 cancel() 能同时通知它。
    ///
    /// 必须在锁内检查本 token 的 flag：如果取消发生在 attach 之前，`cancel()` 那时
    /// 还没有可通知的对象，只置了本 token 的 flag。不补这一步，直链下载会永远
    /// 看不到这次取消，界面显示「已取消」但下载照跑。
    pub fn attach_flag(&self, flag: Arc<AtomicBool>) {
        if let Ok(mut g) = self.shared.lock() {
            if self.flag.load(Ordering::SeqCst) {
                flag.store(true, Ordering::SeqCst);
            }
            *g = Some(flag);
        }
    }
}

pub type SharedTasks = Arc<Mutex<HashMap<String, Task>>>;
pub type SharedTokens = Arc<Mutex<HashMap<String, Arc<CancelToken>>>>;

/// 一行进度快照。
///
/// 用具名字段而不是五元组：`speed` 和 `eta` 都是 `Option<String>`，
/// 位置写反了编译器不会报错，界面上就会把速度显示成剩余时间。
#[derive(Debug, Default)]
struct ProgressLine {
    progress: f64,
    downloaded: u64,
    total: Option<u64>,
    speed: Option<String>,
    eta: Option<String>,
}

/// 停止之后该落什么状态。
///
/// 三处停止点（spawn 前的竞态、yt-dlp 等待结束、直链 Err 分支）都必须问这一个函数：
/// 暂停要继续保留断点、取消是终态，任何一处自己判断 `is_canceled()` 就会把
/// 暂停写成取消，任务再也恢复不了。
pub fn stopped_state(token: &CancelToken) -> (TaskState, String) {
    if token.is_paused() {
        (TaskState::Paused, "已暂停".into())
    } else {
        (TaskState::Canceled, "已取消".into())
    }
}

/// 停止任务时该推给界面的进度快照。
///
/// 暂停必须保留「已经下到哪」：清零会让进度条瞬间归零，用户以为一个字节都没下；
/// 恢复后进度又从 0 跳到原位置，看起来像是偷偷重下了一遍。
/// 取消才清零——那条任务已经废了，留着半截进度只会让人误会它还能接着跑。
fn stop_snapshot(t: &Task, state: TaskState) -> (f64, u64, Option<u64>) {
    if state == TaskState::Paused {
        (t.progress, t.downloaded_bytes, t.total_bytes)
    } else {
        (0.0, 0, None)
    }
}

/// 解析 yt-dlp 的进度行。
/// 我们让 yt-dlp 使用 `--newline --progress` 使每行都是一个进度快照。
fn parse_progress_line(line: &str) -> Option<ProgressLine> {
    // 形如: [download]  42.3% of  120.45MiB at    3.21MiB/s ETA 00:28
    if !line.starts_with("[download]") {
        return None;
    }
    let body = line.trim_start_matches("[download]").trim();

    // 百分比
    let pct_idx = body.find('%')?;
    let num_part = body[..pct_idx].trim();
    let progress: f64 = num_part.split_whitespace().last()?.parse().ok()?;

    // 已下载 / 总量
    let mut total: Option<u64> = None;
    let mut downloaded: u64 = 0;
    if let Some(of_idx) = body.find(" of ") {
        let after = &body[of_idx + 4..];
        let size_token = after.split_whitespace().next().unwrap_or("");
        total = parse_size(size_token);
        if let Some(t) = total {
            downloaded = ((progress / 100.0) * t as f64) as u64;
        }
    }

    // 速度
    let speed = body
        .find(" at ")
        .map(|i| {
            let rest = &body[i + 4..];
            rest.split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty() && s != "Unknown");

    // ETA
    let eta = body
        .find("ETA ")
        .map(|i| {
            let rest = &body[i + 4..];
            rest.split_whitespace()
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty() && s != "Unknown");

    Some(ProgressLine {
        progress,
        downloaded,
        total,
        speed,
        eta,
    })
}

fn parse_size(token: &str) -> Option<u64> {
    // yt-dlp 对「估算值」会加 `~` 前缀（如 `~120.45MiB`）。不剥掉的话
    // strip_suffix 得到 `"~120.45"`，parse 失败返回 None，total 变成 None，
    // 进度条就只剩百分比、下载量恒为 0。
    let t = token.trim().trim_start_matches('~').trim();
    let units = [
        ("GiB", 1024u64 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
        ("B", 1),
    ];
    for (suffix, mult) in units {
        if let Some(num) = t.strip_suffix(suffix) {
            let v: f64 = num.trim().parse().ok()?;
            if !v.is_finite() || v < 0.0 {
                return None;
            }
            return Some((v * mult as f64) as u64);
        }
    }
    None
}

/// yt-dlp 的临时目录（`.part` / 分片 / 合并中间文件）：
/// `<下载目录>/.veloxgrab-tmp/<format_id>-<url 哈希>`。
fn temp_dir_for(dir: &std::path::Path, task: &Task) -> std::path::PathBuf {
    let fmt_key = crate::util::sanitize_filename(&task.format_id.replace('+', "_"));
    dir.join(".veloxgrab-tmp")
        .join(format!("{}-{}", fmt_key, crate::util::short_hash(&task.url)))
}

/// 清理任务留下的临时产物（任务被移除时调用）。
pub fn cleanup_task_artifacts(dir: &std::path::Path, task: &Task) {
    if task.kind == "direct" {
        crate::direct::cleanup_artifacts(dir, &task.title);
    } else {
        // 只删我们自己命名空间下的目录，不会碰下载目录里的其他内容
        let _ = std::fs::remove_dir_all(temp_dir_for(dir, task));
    }
}

/// 启动时清理陈旧的临时产物。
///
/// `cleanup_task_artifacts` 只在用户手动「移除任务」时触发，但下面这些情况
/// 根本不会产生「移除」动作，产物就永久留在下载目录里：
///
/// - 下载中被取消、进程崩溃、直接关程序 → `<name>.veloxgrab-downloading` + `.veloxgrab-part`
/// - yt-dlp 把成品移走后 → 空的 `.veloxgrab-tmp/<format_id>-<hash>/` 目录
/// - 任务失败后用户没点移除就关了窗口
///
/// 只删超过 `max_age` 的，避免误伤另一个实例正在写的文件（单实例插件已经防了
/// 多开，但崩溃重启后一段时间内用户可能还想续传，7 天是足够宽裕的窗口）。
pub fn cleanup_stale_artifacts(dir: &std::path::Path, max_age: std::time::Duration) {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(max_age) else {
        return;
    };

    // 时间戳读不到时一律当成「不陈旧」，宁可留垃圾也不误删
    let is_stale = |p: &std::path::Path| -> bool {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(false)
    };

    // 1) 下载目录顶层的半成品
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let is_partial = name.ends_with(".veloxgrab-downloading")
                || name.ends_with(".veloxgrab-part")
                || name.ends_with(".veloxgrab-part.tmp");
            if is_partial && is_stale(&path) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    // 2) `.veloxgrab-tmp` 下的子目录：空目录，或内容全部陈旧，就整个删掉。
    //    正在下载的目录里必然有活跃的 `.part`，mtime 是新的，不会被误删。
    let tmp_root = dir.join(".veloxgrab-tmp");
    if let Ok(rd) = std::fs::read_dir(&tmp_root) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            let entries: Vec<_> = inner.flatten().collect();
            if entries.is_empty() || entries.iter().all(|e| is_stale(&e.path())) {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    // 顺手把已经空掉的 `.veloxgrab-tmp` 本身删掉（非空时 remove_dir 会失败，无副作用）
    let _ = std::fs::remove_dir(&tmp_root);
}

/// 兜底查找：下载目录里最近 10 分钟内生成、以标题开头的成品文件。
///
/// 只在 `--print after_move:filepath` 没有输出时使用。标题退化成 "download"
/// 时匹配面太宽，宁可返回 None 也不要随便挑一个用户的文件。
fn find_recent_output(dir: &std::path::Path, title: &str) -> Option<String> {
    let stem = crate::util::sanitize_filename(title);
    if stem.is_empty() || stem == "download" {
        return None;
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(600))?;

    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&stem)
            || name.ends_with(".part")
            || name.ends_with(".veloxgrab-part")
            || name.ends_with(".veloxgrab-downloading")
        {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        if md.len() == 0 {
            continue;
        }
        let Ok(modified) = md.modified() else { continue };
        if modified < cutoff {
            continue;
        }
        if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, p)| p.to_string_lossy().to_string())
}

/// 组装输出模板
fn output_template(dir: &str, task: &Task) -> String {
    // 标题为空时用任务 id：util 的兜底名是 "download"，多个任务会撞名
    let name = if task.title.trim().is_empty() {
        format!("video_{}", task.id)
    } else {
        crate::util::sanitize_filename(&task.title)
    };
    std::path::Path::new(dir)
        .join(format!("{name}.%(ext)s"))
        .to_string_lossy()
        .to_string()
}

/// 解析任务实际使用的下载目录。
///
/// 优先用任务自带的 `save_dir`（用户在新建下载时指定的），为空时退回全局默认。
/// 目录不存在时创建：用户可能选了可移动磁盘/U 盘，当时在、重启后不在，
/// 直接 create_dir_all 比报错更符合预期。
///
/// save_dir 的**可信性由建任务入口负责**（高-1 的 admit_save_dir：入口做过
/// 词法折叠 + 根内/canonicalize/信任集判定，回填的是定稿绝对路径；弹框只
/// 发生在入口那一次，本函数不弹）。但本函数还会在 redo/清理/进度收尾等
/// 内部路径被反复调用，任务可能来自 history 回灌——持久层在威胁模型里算
/// 不可信输入，所以这里做最后一次词法兜底：
/// - 绝对且折叠后不含逃逸 → 使用折叠结果（正常路径，与入口定稿一致）；
/// - 相对写法 / 折叠仍想上跳（脏数据）→ 落回默认根，绝不把带 `..` 的路径
///   喂给 create_dir_all / 清理逻辑。位置退到安全根比让整个任务卡死好。
pub fn resolve_task_dir(settings: &Settings, task: &Task) -> Result<std::path::PathBuf, String> {
    let fallback = || std::path::PathBuf::from(&settings.download_dir);
    let dir = match task.save_dir.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => {
            let p = std::path::Path::new(d);
            let (folded, esc) = crate::util::fold_path_lexically(p);
            if p.is_absolute() && !esc {
                folded
            } else {
                fallback()
            }
        }
        _ => fallback(),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
    Ok(dir)
}

/// 构造格式选择表达式。
/// 若用户选的格式无音轨，则自动拼上最佳音轨（yt-dlp 会调用 ffmpeg 合并）。
pub fn build_format_expr(format_id: &str, has_audio: bool, best_audio: Option<&str>, merge_mp4: bool) -> String {
    let mut expr = if has_audio {
        format_id.to_string()
    } else if let Some(audio) = best_audio {
        format!("{format_id}+{audio}")
    } else {
        // 没有可用音轨时退化为 best
        format_id.to_string()
    };
    if merge_mp4 && expr.contains('+') {
        expr.push_str("/best");
    }
    expr
}

/// 实际执行下载。返回最终输出路径。
///
/// 默认先不带 Cookie 下载；失败且用户配了 Cookie 时自动带 Cookie 重试一次。
/// 与 probe 同一套降级思路——浏览器 App-Bound Encryption 让 yt-dlp 读不到
/// Cookie 库时，强行带 Cookie 只会让本来能下的视频也失败。
pub async fn run_download(
    app: tauri::AppHandle,
    task: Task,
    settings: Settings,
    token: Arc<CancelToken>,
    tasks: SharedTasks,
) -> Result<Option<String>, String> {
    use tauri::Emitter;

    let attempt = run_download_inner(
        app.clone(),
        task.clone(),
        settings.clone(),
        token.clone(),
        tasks.clone(),
        false,
    )
    .await;

    match attempt {
        Ok(p) => Ok(p),
        Err(plain_err) => {
            // 用户取消了就别再重试，否则取消后还会偷偷跑一遍
            if token.is_canceled() {
                return Err(plain_err);
            }
            if !crate::ytdlp::has_cookie_source(&settings) {
                return Err(plain_err);
            }
            // 404 / 410 / 磁盘满 / URL 非法，再跑一遍带 Cookie 也不会变好，纯属浪费时间
            if is_hopeless_error(&plain_err) {
                return Err(plain_err);
            }

            // 重置进度和状态，避免重试时界面停在旧错误上。
            // 光改内存不够，得 emit，否则界面会一直显示上一轮的 Failed。
            {
                let mut guard = tasks.lock().await;
                if let Some(t) = guard.get_mut(&task.id) {
                    t.state = TaskState::Queued;
                    t.error = None;
                    t.progress = 0.0;
                    t.downloaded_bytes = 0;
                    t.speed = None;
                    t.eta = None;
                }
            }
            let _ = app.emit(
                "task-progress",
                &ProgressEvent {
                    id: task.id.clone(),
                    state: TaskState::Queued,
                    progress: 0.0,
                    downloaded_bytes: 0,
                    total_bytes: None,
                    speed: None,
                    eta: None,
                    error: None,
                    output_path: None,
                },
            );

            match run_download_inner(
                app.clone(),
                task.clone(),
                settings,
                token,
                tasks.clone(),
                true,
            )
            .await
            {
                Ok(p) => Ok(p),
                Err(cookie_err) => {
                    // 两轮的错误必须一起交给用户：只显示 Cookie 那一轮，
                    // 会让人误以为问题出在 Cookie 上（README 里专门记过这个坑）。
                    //
                    // 注意这里的返回值调用方是丢弃的（`let _ = run_download(...)`），
                    // 所以必须自己把合并后的消息写回任务表并 emit，否则用户永远
                    // 只能看到第二轮的错误。
                    let msg = format!(
                        "下载失败（不带 Cookie）：\n{plain_err}\n\n重试失败（带 Cookie）：\n{cookie_err}"
                    );
                    {
                        let mut guard = tasks.lock().await;
                        if let Some(t) = guard.get_mut(&task.id) {
                            t.state = TaskState::Failed;
                            t.error = Some(msg.clone());
                            crate::history::upsert_entry(t);
                        }
                    }
                    let _ = app.emit(
                        "task-progress",
                        &ProgressEvent {
                            id: task.id.clone(),
                            state: TaskState::Failed,
                            progress: 0.0,
                            downloaded_bytes: 0,
                            total_bytes: None,
                            speed: None,
                            eta: None,
                            error: Some(msg.clone()),
                            output_path: None,
                        },
                    );
                    Err(msg)
                }
            }
        }
    }
}

/// 这些错误换一种 Cookie 策略也不可能成功，没必要再跑一遍完整下载。
fn is_hopeless_error(msg: &str) -> bool {
    const KEYS: &[&str] = &[
        "HTTP Error 404",
        "HTTP Error 410",
        "Not Found",
        "Unsupported URL",
        "is not a valid URL",
        "No space left",
        "os error 112", // Windows: ERROR_DISK_FULL
        "磁盘空间",
        // 退出码 0 但拿不到输出路径：换 Cookie 重试也不会变出路径来
        "没有返回输出路径",
    ];
    KEYS.iter().any(|k| msg.contains(k))
}

async fn run_download_inner(
    app: tauri::AppHandle,
    task: Task,
    settings: Settings,
    token: Arc<CancelToken>,
    tasks: SharedTasks,
    with_cookies: bool,
) -> Result<Option<String>, String> {
    use tauri::Emitter;

    let ytdlp = resolve_ytdlp()?;
    // 每个任务的保存目录可能不同（见 Task::save_dir）
    let dir = resolve_task_dir(&settings, &task)?;
    let dir_str = dir.to_string_lossy().to_string();

    let template = output_template(&dir_str, &task);
    let expr = build_format_expr(
        &task.format_id,
        task.has_audio,
        task.best_audio_format.as_deref(),
        settings.merge_to_mp4,
    );

    let mut cmd = Command::new(&ytdlp);
    // 每行都是完整进度快照
    cmd.arg("--newline").arg("--progress");
    // 下载不带 Cookie 起步，失败后由 run_download 带 Cookie 重试。
    // 与 probe 保持一致：浏览器加密导致读不到 Cookie 时，
    // 不带 Cookie 反而更容易成功。
    push_common_args(&mut cmd, &settings, with_cookies, &task.url);
    // 不传 --no-part：中断时保留 `.part`，不会留下一个「看起来完整、实际残缺」
    // 的最终文件。临时文件按「清晰度 + URL 哈希」分目录存放：
    // 换清晰度不会把新流续写到旧文件尾部；不同 URL 即使标题相同也互不干扰。
    let temp_dir = temp_dir_for(&dir, &task);
    cmd.arg("--paths")
        .arg(format!("temp:{}", temp_dir.to_string_lossy()));
    cmd.arg("--continue");
    cmd.arg("-f").arg(&expr);
    if settings.merge_to_mp4 {
        cmd.arg("--merge-output-format").arg("mp4");
    }
    cmd.arg("--concurrent-fragments")
        .arg(settings.concurrent_fragments.to_string());
    // 输出到用户指定目录，并打印最终文件名
    cmd.arg("-o").arg(&template);
    cmd.arg("--print").arg("after_move:filepath");
    cmd.arg("--no-simulate");
    cmd.arg("--").arg(&task.url);

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 万一 run_download_inner 提前返回（panic / 上层 ? 传播），别把子进程留成孤儿
    cmd.kill_on_drop(true);

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 yt-dlp 失败: {e}"))?;

    if let Some(pid) = child.id() {
        token.set_pid(pid);
    }

    // 取消可能恰好落在 spawn 之前：那一刻 token 里还没有 pid，cancel() 调用的
    // kill_process_tree(0) 是空操作，只置了取消标志。pid 直到上面这行才写进去，
    // 所以必须在这里补一次——否则子进程会变成孤儿，界面显示「已取消」，
    // yt-dlp 却在后台继续把整条下载跑完。
    if token.is_canceled() {
        kill_process_tree(child.id().unwrap_or(0));
        let _ = child.wait().await;
        let (state, msg) = stopped_state(&token);
        let (prog, bytes, total) = {
            let mut guard = tasks.lock().await;
            match guard.get_mut(&task.id) {
                Some(t) => {
                    t.state = state;
                    t.error = Some(msg.clone());
                    stop_snapshot(t, state)
                }
                None => (0.0, 0, None),
            }
        };
        let _ = app.emit(
            "task-progress",
            &ProgressEvent {
                id: task.id.clone(),
                state,
                progress: prog,
                downloaded_bytes: bytes,
                total_bytes: total,
                speed: None,
                eta: None,
                error: Some(msg),
                output_path: None,
            },
        );
        return Ok(None);
    }

    let stdout = child.stdout.take().ok_or("无法读取 yt-dlp 输出")?;
    let stderr = child.stderr.take().ok_or("无法读取 yt-dlp 错误")?;

    let app2 = app.clone();
    let id2 = task.id.clone();
    let tasks2 = tasks.clone();

    // stdout: 进度 + 最终文件路径
    let stdout_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf: Vec<u8> = Vec::new();
        let mut last_path: Option<String> = None;
        let mut last_err_line: Option<String> = None;
        let mut read_err: Option<String> = None;
        loop {
            let line = match next_line_lossy(&mut reader, &mut buf).await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    // 不能像以前那样 `while let Ok(Some(..))` 静默退出：
                    // 读取一旦出错，后面的 [Merger] / ERROR 行和最终路径
                    // 全部丢失，退出码为 0 时任务会被误判为成功。
                    read_err = Some(format!("读取 yt-dlp 输出失败: {e}"));
                    break;
                }
            };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            // 最终文件路径（--print after_move:filepath 的输出）
            if !line.starts_with('[') {
                if std::path::Path::new(&line).is_absolute() {
                    last_path = Some(line.clone());
                    continue;
                }
                last_err_line = Some(line);
                continue;
            }
            if line.starts_with("[download]") {
                if let Some(p) = parse_progress_line(&line) {
                    let ev = ProgressEvent {
                        id: id2.clone(),
                        state: TaskState::Downloading,
                        progress: p.progress,
                        downloaded_bytes: p.downloaded,
                        total_bytes: p.total,
                        speed: p.speed,
                        eta: p.eta,
                        error: None,
                        output_path: None,
                    };
                    {
                        let mut guard = tasks2.lock().await;
                        if let Some(t) = guard.get_mut(&id2) {
                            t.progress = p.progress;
                            t.downloaded_bytes = p.downloaded;
                            t.total_bytes = p.total;
                            t.speed = ev.speed.clone();
                            t.eta = ev.eta.clone();
                            t.state = TaskState::Downloading;
                        }
                    }
                    let _ = app2.emit("task-progress", &ev);
                }
            } else if line.starts_with("[Merger]") || line.starts_with("[VideoConvertor]") {
                let ev = ProgressEvent {
                    id: id2.clone(),
                    state: TaskState::Merging,
                    progress: 100.0,
                    downloaded_bytes: 0,
                    total_bytes: None,
                    speed: None,
                    eta: None,
                    error: None,
                    output_path: None,
                };
                {
                    let mut guard = tasks2.lock().await;
                    if let Some(t) = guard.get_mut(&id2) {
                        t.state = TaskState::Merging;
                    }
                }
                let _ = app2.emit("task-progress", &ev);
            } else if line.starts_with("ERROR:") {
                last_err_line = Some(line.clone());
            }
        }
        (last_path, last_err_line, read_err)
    });

    // stderr: 收集错误（只保留最后 20 行，用 VecDeque 避免 remove(0) 的搬移）
    let stderr_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf: Vec<u8> = Vec::new();
        let mut lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        let mut read_err: Option<String> = None;
        loop {
            let line = match next_line_lossy(&mut reader, &mut buf).await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    read_err = Some(format!("读取 yt-dlp 错误输出失败: {e}"));
                    break;
                }
            };
            let l = line.trim().to_string();
            if l.is_empty() || l.starts_with("WARNING:") {
                continue;
            }
            lines.push_back(l);
            if lines.len() > 20 {
                lines.pop_front();
            }
        }
        (lines.into_iter().collect::<Vec<_>>(), read_err)
    });

    let status = child.wait().await.map_err(|e| format!("等待进程失败: {e}"))?;
    let (final_path, stdout_err, stdout_read_err) =
        stdout_handle.await.unwrap_or((None, None, None));
    let (stderr_lines, stderr_read_err) = stderr_handle.await.unwrap_or_default();

    // 被取消 / 被暂停
    if token.is_canceled() {
        let (state, msg) = stopped_state(&token);
        let (prog, bytes, total) = {
            let mut guard = tasks.lock().await;
            match guard.get_mut(&task.id) {
                Some(t) => {
                    t.state = state;
                    t.error = Some(msg.clone());
                    // 暂停也要落历史：历史是任务表唯一的持久化层，
                    // 不写的话用户暂停后退出软件，任务连同「继续」入口一起消失。
                    crate::history::upsert_entry(t);
                    stop_snapshot(t, state)
                }
                None => (0.0, 0, None),
            }
        };
        let _ = app.emit(
            "task-progress",
            &ProgressEvent {
                id: task.id.clone(),
                state,
                progress: prog,
                downloaded_bytes: bytes,
                total_bytes: total,
                speed: None,
                eta: None,
                error: Some(msg),
                output_path: None,
            },
        );
        return Ok(None);
    }

    // stdout 读失败会丢掉后续全部输出（最终路径 + 错误行）。
    // 这种「退出码 0 但什么都没读到」的情况不能算成功。
    let untrusted_success =
        status.success() && final_path.is_none() && stdout_read_err.is_some();

    // `--print after_move:filepath` 偶尔没有输出（版本差异、输出被过滤）。
    // 这时从下载目录里找刚生成的文件兜底，别让用户拿到一个没有路径的「成功」任务。
    let resolved_path = match final_path {
        Some(p) => Some(p),
        None if status.success() && !untrusted_success => find_recent_output(&dir, &task.title),
        None => None,
    };
    let missing_output = status.success() && !untrusted_success && resolved_path.is_none();

    if status.success() && !untrusted_success && !missing_output {
        // 同 run_direct_download：yt-dlp 的终态事件也要带上真实大小，
        // 否则「大小」列对完成的视频任务恒为「—」。
        let final_size = resolved_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len());
        let mut guard = tasks.lock().await;
        if let Some(t) = guard.get_mut(&task.id) {
            t.state = TaskState::Done;
            t.progress = 100.0;
            t.output_path = resolved_path.clone();
            t.error = None;
            if let Some(sz) = final_size {
                t.downloaded_bytes = sz;
                t.total_bytes = Some(sz);
            }
            crate::history::upsert_entry(t);
        }
        drop(guard);
        let _ = app.emit(
            "task-progress",
            &ProgressEvent {
                id: task.id.clone(),
                state: TaskState::Done,
                progress: 100.0,
                downloaded_bytes: final_size.unwrap_or(0),
                total_bytes: final_size,
                speed: None,
                eta: None,
                error: None,
                output_path: resolved_path.clone(),
            },
        );
        Ok(resolved_path)
    } else {
        let mut parts: Vec<String> = Vec::new();
        if missing_output {
            parts.push("yt-dlp 报告成功，但没有返回输出路径，也没在下载目录找到成品文件".into());
        }
        if !stderr_lines.is_empty() {
            parts.push(stderr_lines.join("\n"));
        }
        if let Some(e) = stderr_read_err {
            parts.push(e);
        }
        if let Some(e) = stdout_read_err {
            parts.push(e);
        }
        let mut msg = parts.join("\n");
        if msg.is_empty() {
            msg = stdout_err.unwrap_or_else(|| "下载失败，未知错误".into());
        }
        if msg.chars().count() > 1200 {
            msg = msg.chars().take(1200).collect();
        }
        let mut guard = tasks.lock().await;
        if let Some(t) = guard.get_mut(&task.id) {
            t.state = TaskState::Failed;
            t.error = Some(msg.clone());
            crate::history::upsert_entry(t);
        }
        drop(guard);
        let _ = app.emit(
            "task-progress",
            &ProgressEvent {
                id: task.id.clone(),
                state: TaskState::Failed,
                progress: 0.0,
                downloaded_bytes: 0,
                total_bytes: None,
                speed: None,
                eta: None,
                error: Some(msg.clone()),
                output_path: None,
            },
        );
        Err(msg)
    }
}

/// 执行直链多线程下载。与 run_download 并列，由 kind 字段分流。
pub async fn run_direct_download(
    app: tauri::AppHandle,
    task: Task,
    settings: Settings,
    token: Arc<CancelToken>,
    tasks: SharedTasks,
) -> Result<Option<String>, String> {
    use tauri::Emitter;

    // 任务可能自带保存目录；为空时用全局默认目录
    let dir = resolve_task_dir(&settings, &task)?;
    let cancel_flag = Arc::new(AtomicBool::new(false));
    token.attach_flag(cancel_flag.clone());

    // attach 之前就被取消的话，别再去发探测请求了。
    // attach_flag 会把已有取消状态同步进来，这里只是省掉一次无谓的网络往返。
    if cancel_flag.load(Ordering::SeqCst) {
        let (state, msg) = stopped_state(&token);
        let (prog, bytes, total) = {
            let mut guard = tasks.lock().await;
            match guard.get_mut(&task.id) {
                Some(t) => {
                    t.state = state;
                    t.error = Some(msg.clone());
                    crate::history::upsert_entry(t);
                    stop_snapshot(t, state)
                }
                None => (0.0, 0, None),
            }
        };
        let _ = app.emit(
            "task-progress",
            &ProgressEvent {
                id: task.id.clone(),
                state,
                progress: prog,
                downloaded_bytes: bytes,
                total_bytes: total,
                speed: None,
                eta: None,
                error: Some(msg),
                output_path: None,
            },
        );
        return Ok(None);
    }

    let net = crate::settings::NetCfg::from_settings(&settings);
    let mut threads = if task.threads > 0 {
        task.threads
    } else {
        settings.default_threads.max(1)
    };
    // HuggingFace 匿名下载有限流（线程过多触发 403）：没配 token 时按设置压低线程数。
    if task.url.contains("huggingface.co")
        && settings.hf_token_trimmed().is_none()
        && settings.hf_thread_cap > 0
    {
        threads = threads.min(settings.hf_thread_cap);
    }

    let app2 = app.clone();
    let id2 = task.id.clone();
    let tasks2 = tasks.clone();

    // 进度回调来自下载器内部的非 async 闭包（多分块线程共用），
    // 而任务表是 tokio::sync::Mutex，不能在同步上下文里 await。
    // 这里用一个 std::sync::Mutex 直接写共享快照，再以非阻塞方式投递事件。
    // 实际上 tasks 用的是 tokio Mutex，故这里改为经由通道交给一个 async 消费者处理。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();

    // 事件消费者：把进度写入任务表并推给前端
    let app_c = app2.clone();
    let id_c = id2.clone();
    let tasks_c = tasks2.clone();
    let consumer = tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            {
                let mut guard = tasks_c.lock().await;
                if let Some(t) = guard.get_mut(&id_c) {
                    t.progress = ev.progress;
                    t.downloaded_bytes = ev.downloaded_bytes;
                    t.total_bytes = ev.total_bytes;
                    t.speed = ev.speed.clone();
                    t.eta = ev.eta.clone();
                    t.state = ev.state;
                }
            }
            let _ = app_c.emit("task-progress", &ev);
        }
    });

    let result = crate::direct::download_direct(
        &task.url,
        &dir,
        // 界面上的「保存文件名」就存在 task.title 里；直链任务没有别的来源
        Some(task.title.as_str()),
        threads,
        cancel_flag.clone(),
        move |p| {
            let _ = tx.send(ProgressEvent {
                id: id2.clone(),
                state: TaskState::Downloading,
                progress: p.progress,
                downloaded_bytes: p.downloaded,
                total_bytes: p.total,
                speed: p.speed.clone(),
                eta: p.eta.clone(),
                error: None,
                output_path: None,
            });
        },
        &net,
    )
    .await;

    // 进度消费者读完通道后自然退出
    let _ = consumer.await;

    match result {
        Ok(path) => {
            let path_str = path.to_string_lossy().to_string();
            // 完成时用磁盘上的真实文件长度兜底。
            //
            // 进度线程每 150ms 才上报一次，几 MB 的本地/内网下载常常在第一次
            // 采样之前就跑完了——这时前端只收到下面这一个终态事件，而它原先把
            // downloaded_bytes/total_bytes 写死成 0/None，表格「大小」列于是
            // 永远显示「—」。按磁盘长度回填，慢速和快速下载都能拿到正确大小。
            let final_size = std::fs::metadata(&path).map(|m| m.len()).ok();
            let (done_bytes, done_total) = {
                let guard = tasks.lock().await;
                match guard.get(&task.id) {
                    Some(t) => (
                        final_size.unwrap_or(t.downloaded_bytes),
                        final_size.or(t.total_bytes),
                    ),
                    None => (final_size.unwrap_or(0), final_size),
                }
            };
            {
                let mut guard = tasks.lock().await;
                if let Some(t) = guard.get_mut(&task.id) {
                    t.state = TaskState::Done;
                    t.progress = 100.0;
                    t.output_path = Some(path_str.clone());
                    t.error = None;
                    t.speed = None;
                    t.downloaded_bytes = done_bytes;
                    t.total_bytes = done_total;
                    // 记录历史：完成时间、最终输出路径（历史页点「打开所在文件夹」要用）
                    crate::history::upsert_entry(t);
                }
            }
            let _ = app.emit(
                "task-progress",
                &ProgressEvent {
                    id: task.id.clone(),
                    state: TaskState::Done,
                    progress: 100.0,
                    downloaded_bytes: done_bytes,
                    total_bytes: done_total,
                    speed: None,
                    eta: None,
                    error: None,
                    output_path: Some(path_str.clone()),
                },
            );
            Ok(Some(path_str))
        }
        Err(e) => {
            // 直链的取消/暂停在 download_direct 内部就返回 Err("已取消")，
            // 所以这里除了看 token，还要认那条消息。暂停优先：token 被 pause()
            // 过的话，即便消息文本是「已取消」也要落成 Paused。
            let stopped = token.is_canceled() || e.contains("已取消");
            let (state, msg) = if stopped {
                stopped_state(&token)
            } else {
                (TaskState::Failed, e)
            };
            let (prog, bytes, total) = {
                let mut guard = tasks.lock().await;
                match guard.get_mut(&task.id) {
                    Some(t) => {
                        t.state = state;
                        t.error = Some(msg.clone());
                        crate::history::upsert_entry(t);
                        stop_snapshot(t, state)
                    }
                    None => (0.0, 0, None),
                }
            };
            let _ = app.emit(
                "task-progress",
                &ProgressEvent {
                    id: task.id.clone(),
                    state,
                    progress: prog,
                    downloaded_bytes: bytes,
                    total_bytes: total,
                    speed: None,
                    eta: None,
                    error: Some(msg.clone()),
                    output_path: None,
                },
            );
            Ok(None)
        }
    }
}

/// 强制终止子进程（Windows 用 taskkill /T 杀进程树，避免 ffmpeg 残留）
pub fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
}

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_handles_estimate_prefix() {
        // yt-dlp 对估算值输出 `~120.45MiB`，不剥 `~` 会导致 total 为 None
        assert_eq!(parse_size("~120.45MiB"), Some((120.45f64 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("120.45MiB"), Some((120.45f64 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("~1.5GiB"), Some((1.5f64 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("512.00KiB"), Some(512 * 1024));
        assert_eq!(parse_size("1024B"), Some(1024));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert_eq!(parse_size("Unknown"), None);
        assert_eq!(parse_size("N/A"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("~"), None);
    }

    #[test]
    fn parse_progress_line_extracts_named_fields() {
        let p = parse_progress_line("[download]  42.3% of ~120.45MiB at    3.21MiB/s ETA 00:28")
            .expect("进度行应能解析");

        assert!((p.progress - 42.3).abs() < 1e-9);
        let total = p.total.expect("带 ~ 的估算值也要能解析");
        assert_eq!(total, (120.45f64 * 1024.0 * 1024.0) as u64);
        assert_eq!(p.downloaded, ((42.3 / 100.0) * total as f64) as u64);
        assert_eq!(p.speed.as_deref(), Some("3.21MiB/s"));
        assert_eq!(p.eta.as_deref(), Some("00:28"));

        // 没有速度/ETA 时应该是 None，而不是空字符串
        let p2 = parse_progress_line("[download]  10.0% of 10.00MiB").unwrap();
        assert_eq!(p2.speed, None);
        assert_eq!(p2.eta, None);

        // 不是进度行
        assert!(parse_progress_line("[info] Downloading").is_none());
    }

    #[test]
    fn cleanup_removes_stale_artifacts_only() {
        let root = std::env::temp_dir().join(format!("veloxgrab_cleanup_{}", crate::util::short_hash(
            &format!("{:?}", std::time::SystemTime::now())
        )));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // 半成品：三个都算陈旧
        let stale_dl = root.join("movie.mp4.veloxgrab-downloading");
        let stale_part = root.join("movie.mp4.veloxgrab-part");
        let stale_ptmp = root.join("movie.mp4.veloxgrab-part.tmp");
        // 陈旧但正常的成品文件，绝不能删
        let real_file = root.join("movie.mp4");
        // 名字里带 veloxgrab 但不是我们的后缀，也不能删
        let lookalike = root.join("notes.veloxgrab-downloading.bak");
        for p in [&stale_dl, &stale_part, &stale_ptmp, &real_file, &lookalike] {
            std::fs::write(p, b"x").unwrap();
        }

        // 临时目录：一个空的、一个有陈旧内容的、一个带新鲜内容的
        let tmp_root = root.join(".veloxgrab-tmp");
        let empty_dir = tmp_root.join("137-aaaaaaaa");
        let stale_dir = tmp_root.join("140-bbbbbbbb");
        let fresh_dir = tmp_root.join("22-cccccccc");
        for d in [&empty_dir, &stale_dir, &fresh_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(stale_dir.join("video.f137.mp4.part"), b"x").unwrap();
        let fresh_part = fresh_dir.join("video.f22.mp4.part");
        std::fs::write(&fresh_part, b"x").unwrap();

        // 清理窗口是 7 天，所以「陈旧」必须比 7 天更老；用 10 天前区分。
        // 新鲜的（刚写入）自然落在窗口内，不用动。
        let ten_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 24 * 3600);
        for p in [&stale_dl, &stale_part, &stale_ptmp, &stale_dir.join("video.f137.mp4.part")] {
            let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_modified(ten_days_ago).unwrap();
        }

        cleanup_stale_artifacts(&root, std::time::Duration::from_secs(7 * 24 * 3600));

        assert!(!stale_dl.exists(), "陈旧半成品应被删除");
        assert!(!stale_part.exists(), "陈旧断点文件应被删除");
        assert!(!stale_ptmp.exists(), "陈旧断点临时文件应被删除");
        assert!(real_file.exists(), "正常成品文件不能被误删");
        assert!(lookalike.exists(), "后缀不匹配的文件不能被误删");
        assert!(!empty_dir.exists(), "空临时目录应被删除");
        assert!(!stale_dir.exists(), "内容全陈旧的临时目录应被删除");
        assert!(fresh_dir.exists(), "正在下载的临时目录必须保留");
        assert!(fresh_part.exists(), "活跃的 .part 文件必须保留");

        let _ = std::fs::remove_dir_all(&root);
    }
}
