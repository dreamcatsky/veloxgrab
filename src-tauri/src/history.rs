//! 下载历史持久化。
//!
//! 之前任务表只在内存里，程序一关全部记录清零：用户找不到昨天下的文件在哪、
//! 失败的任务是什么原因、点了「移除」的记录也彻底消失。
//!
//! 这里把每个进入终态**或活跃/暂停**的任务单独 upsert 成 JSON：
//! - 入队、状态变化（完成/失败/取消/暂停）时同步一条；活跃态统一记成 paused，
//!   保证下载中途退出软件任务也不会丢；
//! - 进度事件（10Hz）**不**触发写盘——否则 3 个并发就是每秒 30 次磁盘 IO。
//!   下载中途的进度靠断点文件本身续传，不依赖历史里的快照值。
//!
//! 文件采用「写临时文件 + rename」的原子替换：直接 write 在崩溃时会留下
//! 截断的 JSON，下次启动整份历史都读不回来。
//!
//! ## 写入模型：单写线程（为什么不是每个调用者各自读-改-写）
//!
//! 旧模型里 24 个调用点都「持有全局 tasks 锁时同步读整个 history.json、合并、
//! 全量写回」。三宗罪：① 磁盘一卡（杀软扫描、U 盘掉线）整张任务表冻结，所有
//! 命令和进度上报排队；② upsert 是 load-modify-write 且无串行化，引擎线程与
//! 命令线程并发时后写者覆盖前写者——丢更新；③ 并发写共用同一个 `history.json.tmp`，
//! rename 抢走半截内容会产出坏 JSON，触发整库备份转空。
//!
//! 新模型：调用方只把 `Op`（Upsert/Remove/Clear/Flush）塞进 mpsc（微秒级，
//! 无磁盘 IO，可以在 tasks 临界区里调用）；唯一的写线程把操作按到达顺序重放到
//! 内存权威副本上，攒一个合并窗口落盘一次。读-改-写从此不存在第二个作者，
//! 批量操作的 N 次全量写盘自动合并成 1 次。进程退出前经 `flush_blocking` 强制
//! 落盘；异常崩溃最多丢合并窗口（~200ms）内的状态变化——断点文件不依赖历史，
//! 丢的只是列表显示与 paused 回灌字段，且下一次状态变化会补全。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::engine::Task;

/// 单个历史条目。
///
/// 不复用 `Task`：Task 带 `#[serde(default)]` 的历史包袱，历史只需要终态信息
/// 和暂停任务的续传字段，速度/ETA 这类瞬时值不落盘。
/// 新字段一律 `#[serde(default)]`：旧版本写的 history.json 必须原样读得回来。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub url: String,
    pub title: String,
    pub kind: String,
    /// "done" / "failed" / "canceled" / "paused"
    pub state: String,
    pub quality_label: String,
    pub output_path: Option<String>,
    pub save_dir: Option<String>,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
    pub created_at: i64,
    /// 任务结束（完成/失败/取消）的时间戳；paused 是中间态，恒为 None
    pub finished_at: Option<i64>,
    /// 以下字段只为 paused 条目服务：重启回灌后要能原样续传。
    /// 视频任务的 format_id 若不存，回灌会退化成 "direct"，恢复后
    /// yt-dlp 拿到 `-f direct` 必失败——「失败可续传」放开后这条坑必然踩中。
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub downloaded_bytes: u64,
    #[serde(default)]
    pub threads: usize,
    #[serde(default)]
    pub format_id: String,
    #[serde(default)]
    pub has_audio: bool,
    #[serde(default)]
    pub best_audio_format: Option<String>,
}

impl HistoryEntry {
    /// 从任务构造历史条目。
    ///
    /// 落盘语义按「重启后该看到什么」定：
    /// - 终态（done/failed/canceled）按原样记录；
    /// - Paused 以及一切活跃态（queued/downloading/merging）一律记成 "paused"：
    ///   历史是任务表唯一的持久化层，活跃任务不落盘的话，用户在下载中退出软件
    ///   就会连任务带「继续」入口一起丢。记成 paused 回灌后是带断点的暂停任务，
    ///   点继续即续传；回灌出的任务不进队列，也保持了「重启不自动开下」的设计。
    ///   断点文件（.veloxgrab-part / yt-dlp .part）本来就在盘上，语义完全吻合。
    pub fn from_task(task: &Task) -> Option<Self> {
        use crate::engine::TaskState;
        let (state, paused_like) = match task.state {
            TaskState::Done => ("done", false),
            TaskState::Failed => ("failed", false),
            TaskState::Canceled => ("canceled", false),
            TaskState::Paused => ("paused", true),
            TaskState::Queued | TaskState::Downloading | TaskState::Merging => ("paused", true),
        };
        Some(Self {
            id: task.id.clone(),
            url: task.url.clone(),
            title: task.title.clone(),
            kind: task.kind.clone(),
            state: state.to_string(),
            quality_label: task.quality_label.clone(),
            output_path: task.output_path.clone(),
            save_dir: task.save_dir.clone(),
            total_bytes: task.total_bytes,
            error: task.error.clone(),
            created_at: task.created_at,
            // 暂停/活跃都不是"结束"：finished_at 留 None，记录页不显示完成时间
            finished_at: if paused_like { None } else { Some(crate::commands::now_ms()) },
            progress: if paused_like { task.progress } else { 0.0 },
            downloaded_bytes: if paused_like { task.downloaded_bytes } else { 0 },
            threads: task.threads,
            format_id: task.format_id.clone(),
            has_audio: task.has_audio,
            best_audio_format: task.best_audio_format.clone(),
        })
    }

    /// 把历史条目还原成任务，用于启动时回灌任务表。
    ///
    /// 「下载记录」页面已经删掉了（它的内容和「所有下载 / 正在下载 / 已完成」
    /// 三个筛选完全重复），但历史文件本身还在承担持久化职责：任务表只在内存里，
    /// 不回灌的话用户每次重启都会看到空列表，昨天下的文件一条都找不到。
    ///
    /// 终态条目还原成不进队列、不自动下载的记录；paused 条目还原成可「继续」的
    /// 暂停任务（断点文件仍在盘上，进度从历史里拿）。
    pub fn to_task(&self) -> Task {
        use crate::engine::TaskState;
        let state = match self.state.as_str() {
            "done" => TaskState::Done,
            "failed" => TaskState::Failed,
            "paused" => TaskState::Paused,
            _ => TaskState::Canceled,
        };
        // history.json 在威胁模型里算不可信输入（手改/半截写/恶意本机进程都可能
        // 碰它）：回灌前把数值字段钳回合法域。progress 的 NaN/负数/>100 会直接
        // 画进进度条；downloaded > total 会让「已下载 1.2 GB / 共 800 MB」上屏。
        // 钳而不弃：条目仍可用，续传偏移另有 .veloxgrab-part 兜底，不依赖这里的值。
        let progress = if self.progress.is_finite() {
            self.progress.clamp(0.0, 100.0)
        } else {
            0.0
        };
        Task {
            id: self.id.clone(),
            url: self.url.clone(),
            title: self.title.clone(),
            thumbnail: None,
            duration: None,
            // 旧条目没有 format_id（默认空串）：回退 "direct" 与现状一致；
            // 新条目带原值，视频任务恢复后仍能拼出正确的 -f 表达式。
            format_id: if self.format_id.is_empty() {
                "direct".into()
            } else {
                self.format_id.clone()
            },
            quality_label: self.quality_label.clone(),
            has_audio: self.has_audio || self.format_id.is_empty(),
            best_audio_format: self.best_audio_format.clone(),
            state,
            progress: match state {
                TaskState::Done => 100.0,
                TaskState::Paused => progress,
                _ => 0.0,
            },
            downloaded_bytes: if state == TaskState::Paused {
                self.downloaded_bytes
                    .min(self.total_bytes.unwrap_or(u64::MAX))
            } else {
                self.total_bytes.unwrap_or(0)
            },
            total_bytes: self.total_bytes,
            speed: None,
            eta: None,
            output_path: self.output_path.clone(),
            error: self.error.clone(),
            created_at: self.created_at,
            kind: if self.kind.is_empty() {
                "direct".into()
            } else {
                self.kind.clone()
            },
            threads: self.threads,
            save_dir: self.save_dir.clone(),
        }
    }
}

/// 历史文件路径：`<config_dir>/VeloxGrab/history.json`
fn history_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("VeloxGrab").join("history.json")
}

/// 历史文件大小上限（字节）。
///
/// 每条记录约 400 字节，2000 条约 800KB，读取与全量渲染都还能接受。
/// 超出后从最旧的开始截断，保证文件不会无限膨胀。
const MAX_ENTRIES: usize = 2000;

/// 攒写窗口：这段时间内到达的操作合并成一次落盘。
///
/// 批量操作（清空全部 / 全部开始 / 退出时 cancel_all 逐条拍 Paused）会产生
/// N 条操作；合并窗口把它们压成 1 次全量写盘。状态变化本身是低频事件
/// （每个任务每个状态迁移一次，不是 10Hz 的进度），200ms 足够凑齐一批，
/// 又不会让「刚暂停就断电」丢太多。
const COALESCE: Duration = Duration::from_millis(200);

/// 写线程收到的操作。
///
/// HistoryEntry 有十来个字段（内含多个 String），直接进枚举会让每个 Remove /
/// Clear 都按最大变体装箱；Box 一下，通道里流过的每个 op 就是指针大小。
enum Op {
    Upsert(Box<HistoryEntry>),
    Remove(String),
    Clear,
    /// 强制落盘并回执（进程退出前用）。
    Flush(mpsc::Sender<()>),
}

/// 把一批操作重放到权威副本上。纯函数：便于单测，不碰文件系统。
fn apply_ops(list: &mut Vec<HistoryEntry>, ops: Vec<Op>) {
    for op in ops {
        match op {
            Op::Upsert(entry) => match list.iter_mut().find(|h| h.id == entry.id) {
                Some(slot) => *slot = *entry,
                None => list.push(*entry),
            },
            Op::Remove(id) => list.retain(|h| h.id != id),
            Op::Clear => list.clear(),
            // Flush 只做攒批边界：写与回执在调用方处理
            Op::Flush(_) => {}
        }
    }
}

/// 落盘前的整形：新→旧排序 + 截断。纯函数。
fn prepare_for_write(list: &mut Vec<HistoryEntry>) {
    list.sort_by_key(|h| std::cmp::Reverse(h.created_at));
    list.truncate(MAX_ENTRIES);
}

/// 读取全部历史（新的在前）。文件不存在或损坏时返回空列表。
///
/// 只有两个读者：进程启动时 AppState::new 回灌任务表、写线程首次启动取初值。
/// 运行中的读一律走写线程的权威副本，不再有人对同一个文件做 read-modify-write。
pub fn load_history() -> Vec<HistoryEntry> {
    let path = history_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<HistoryEntry>>(&text) {
        Ok(list) => list,
        Err(e) => {
            // 与 settings 一致：坏文件改名备份，不要让它永远挡住后续写入。
            // 备份名带时间戳而不是固定 .json.bad：连续两次损坏（断电 + 半截写
            // 是真实场景）时，固定名会让第二次覆盖掉第一次的唯一遗骸。
            let stamp = crate::commands::now_ms();
            let bad = history_path().with_extension(format!("json.bad-{stamp}"));
            let _ = std::fs::rename(&path, &bad);
            eprintln!(
                "history.json 解析失败（{e}），已备份到 {}，本次历史为空",
                bad.display()
            );
            Vec::new()
        }
    }
}

/// 原子写入历史文件。只有写线程调用它，所以临时文件名不会与人争抢。
fn write_history(list: &[HistoryEntry]) -> Result<(), String> {
    let path = history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_string(list).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &text).map_err(|e| format!("写入历史失败: {e}"))?;
    // fsync 到盘再 rename：崩溃时不留下「目录项已更新、内容还在页缓存」的空文件。
    // rename + flush 的组合才是「要么旧内容要么新内容」的原子性来源。
    // 句柄必须可写：Windows 上 FlushFileBuffers 对只读句柄会失败，
    // 原来用 File::open + sync_data 在 Windows 上其实是静默空操作。
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&tmp) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("保存历史失败: {e}"))?;
    Ok(())
}

/// 写线程句柄：全局唯一，首次使用时惰性起线程。
static WRITER: OnceLock<Mutex<Sender<Op>>> = OnceLock::new();

fn sender() -> &'static Mutex<Sender<Op>> {
    WRITER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Op>();
        std::thread::Builder::new()
            .name("history-writer".into())
            .spawn(move || writer_loop(rx))
            .expect("起历史写线程失败");
        Mutex::new(tx)
    })
}

/// 提交一个操作。非阻塞（通道发送微秒级），因此可以在持有 tasks 锁时调用。
fn submit(op: Op) {
    let lock = sender();
    // 中毒说明写线程 panic 过（磁盘 IO 路径本身不 panic，多半是未来改动引入）：
    // 取内层继续用，而不是让整个应用的落盘从此沉默。
    let tx = lock.lock().unwrap_or_else(|e| e.into_inner());
    let _ = tx.send(op);
}

/// 唯一的落盘者：重放操作 → 攒批 → 合并窗口到期才写。
fn writer_loop(rx: mpsc::Receiver<Op>) {
    let mut list = load_history();
    let mut pending: Vec<Op> = Vec::new();
    let mut dirty = false;
    let mut deadline: Option<Instant> = None;

    loop {
        let wait = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => COALESCE,
        };
        match rx.recv_timeout(wait) {
            Ok(Op::Upsert(e)) => {
                pending.push(Op::Upsert(e));
                dirty = true;
                deadline.get_or_insert_with(Instant::now);
            }
            Ok(Op::Remove(id)) => {
                pending.push(Op::Remove(id));
                dirty = true;
                deadline.get_or_insert_with(Instant::now);
            }
            Ok(Op::Clear) => {
                // Clear 是「抹掉一切」的显式意图，不等攒批：立即重放并落盘，
                // 否则清空后 200ms 内崩溃/重启，旧历史会「复活」。
                let mut ops = std::mem::take(&mut pending);
                ops.push(Op::Clear);
                apply_ops(&mut list, ops);
                prepare_for_write(&mut list);
                if let Err(e) = write_history(&list) {
                    eprintln!("{e}");
                }
                dirty = false;
                deadline = None;
            }
            Ok(Op::Flush(reply)) => {
                // reply 不塞进 ops（apply_ops 拿走整个 Vec，之后再想取就是
                // use-after-move）；它只当攒批边界：重放、落盘、回执。
                let ops = std::mem::take(&mut pending);
                apply_ops(&mut list, ops);
                if dirty {
                    prepare_for_write(&mut list);
                    if let Err(e) = write_history(&list) {
                        eprintln!("{e}");
                    }
                    dirty = false;
                    deadline = None;
                }
                // 接收端可能已经超时离开，回执失败不影响正确性
                let _ = reply.send(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if dirty {
                    let ops = std::mem::take(&mut pending);
                    apply_ops(&mut list, ops);
                    prepare_for_write(&mut list);
                    if let Err(e) = write_history(&list) {
                        eprintln!("{e}");
                    }
                    dirty = false;
                    deadline = None;
                }
            }
            // 所有发送端 drop = 进程正在结束：做最后一次落盘再退。
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let ops = std::mem::take(&mut pending);
                let had_ops = !ops.is_empty();
                apply_ops(&mut list, ops);
                if dirty || had_ops {
                    prepare_for_write(&mut list);
                    if let Err(e) = write_history(&list) {
                        eprintln!("{e}");
                    }
                }
                return;
            }
        }
    }
}

/// 单条历史 Upsert（任务状态变化时调用）。
///
/// 这里只投递操作，不做磁盘 IO：调用点大多在持有全局 tasks 锁的临界区里，
/// 同步写盘会让「磁盘一卡」放大成「全部任务停摆」。
pub fn upsert_entry(task: &Task) {
    let Some(entry) = HistoryEntry::from_task(task) else {
        return;
    };
    submit(Op::Upsert(Box::new(entry)));
}

/// 从历史里删除一条（用户点「删除记录」/移除任务）。
pub fn remove_entry(id: &str) -> Result<(), String> {
    submit(Op::Remove(id.to_string()));
    Ok(())
}

/// 清空全部历史。
pub fn clear_all() -> Result<(), String> {
    submit(Op::Clear);
    Ok(())
}

/// 强制把攒着的变化写盘并等待完成。进程退出前（含 2 秒上限的清理路径）调用。
///
/// 等待有超时：写线程卡在磁盘上时，宁可留下攒批窗口内那点损失，
/// 也不能把「退出」本身卡死——那是比丢历史更差的结果。
pub fn flush_blocking(timeout: Duration) {
    let (tx, rx) = mpsc::channel::<()>();
    submit(Op::Flush(tx));
    let _ = rx.recv_timeout(timeout);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::TaskState;

    fn task_with_state(id: &str, state: TaskState) -> Task {
        Task {
            id: id.to_string(),
            url: "https://example.com/a.bin".into(),
            title: "a.bin".into(),
            thumbnail: None,
            duration: None,
            format_id: "direct".into(),
            quality_label: "4 线程".into(),
            has_audio: false,
            best_audio_format: None,
            state,
            progress: 100.0,
            downloaded_bytes: 10,
            total_bytes: Some(10),
            speed: None,
            eta: None,
            output_path: Some("C:\\dl\\a.bin".into()),
            error: None,
            created_at: 1,
            kind: "direct".into(),
            threads: 4,
            save_dir: Some("D:\\custom".into()),
        }
    }

    #[test]
    fn active_tasks_are_persisted_as_paused() {
        // 历史是任务表唯一的持久化层：活跃态不落盘的话，下载中退出软件
        // 任务就永远消失了。统一记成 paused，重启回灌成可「继续」的暂停任务。
        for st in [
            TaskState::Downloading,
            TaskState::Queued,
            TaskState::Merging,
            TaskState::Paused,
        ] {
            let e = HistoryEntry::from_task(&task_with_state("a", st)).unwrap();
            assert_eq!(e.state, "paused", "{st:?} 必须记成 paused");
            assert!(e.finished_at.is_none(), "中间态不算结束");
            assert_eq!(e.progress, 100.0);
            assert_eq!(e.downloaded_bytes, 10);
            // 续传三件套：回灌后 run_download 才能拼出正确的 -f
            assert_eq!(e.format_id, "direct");
            assert_eq!(e.threads, 4);
        }

        let e = HistoryEntry::from_task(&task_with_state("a", TaskState::Done)).unwrap();
        assert_eq!(e.state, "done");
        assert_eq!(e.save_dir.as_deref(), Some("D:\\custom"));
        assert!(e.finished_at.is_some());

        assert_eq!(
            HistoryEntry::from_task(&task_with_state("a", TaskState::Failed))
                .unwrap()
                .state,
            "failed"
        );
        assert_eq!(
            HistoryEntry::from_task(&task_with_state("a", TaskState::Canceled))
                .unwrap()
                .state,
            "canceled"
        );
    }

    /// 旧版 history.json（没有 paused/format 等新字段）必须原样读回来：
    /// serde(default) 兼容是这条持久化层的底线，否则升级即清空用户记录。
    #[test]
    fn legacy_entry_json_still_deserializes() {
        let legacy = r#"{
            "id": "t1", "url": "https://e.com/a", "title": "a.bin",
            "kind": "video", "state": "failed", "quality_label": "1080p",
            "output_path": null, "save_dir": null, "total_bytes": null,
            "error": "boom", "created_at": 5, "finished_at": 9
        }"#;
        let entry: HistoryEntry = serde_json::from_str(legacy).unwrap();
        let task = entry.to_task();
        assert_eq!(task.state, TaskState::Failed);
        // 旧条目没有 format_id：回退 direct，与升级前行为一致
        assert_eq!(task.format_id, "direct");
    }

    /// paused 条目回灌必须是「可继续」的暂停任务，且续传字段原样带回。
    #[test]
    fn paused_roundtrip_keeps_resume_fields() {
        let mut t = task_with_state("a", TaskState::Paused);
        t.kind = "video".into();
        t.format_id = "137".into();
        t.has_audio = false;
        t.best_audio_format = Some("140".into());
        t.progress = 42.0;
        let entry = HistoryEntry::from_task(&t).unwrap();
        let back = entry.to_task();
        assert_eq!(back.state, TaskState::Paused);
        assert!(back.state.is_resumable(), "回灌出来的暂停任务必须能点继续");
        assert_eq!(back.format_id, "137");
        assert_eq!(back.best_audio_format.as_deref(), Some("140"));
        assert_eq!(back.threads, 4);
        assert_eq!(back.progress, 42.0);
        assert_eq!(back.downloaded_bytes, 10);
    }

    /// 状态迁移必须能覆盖旧记录：失败 → 重试成功，历史里不能还留着 failed。
    #[test]
    fn upsert_replaces_existing_entry() {
        let mut list: Vec<HistoryEntry> = Vec::new();
        let failed = HistoryEntry::from_task(&task_with_state("a", TaskState::Failed)).unwrap();
        list.push(failed);
        let done = HistoryEntry::from_task(&task_with_state("a", TaskState::Done)).unwrap();
        // 模拟 upsert 的 replace 分支
        if let Some(slot) = list.iter_mut().find(|h| h.id == done.id) {
            *slot = done;
        }
        assert_eq!(list.len(), 1, "同一任务不能产生两条历史");
        assert_eq!(list[0].state, "done");
    }

    /// 写线程的重放模型：Upsert/Remove/Clear 按到达顺序作用在同一份权威副本上。
    /// 旧模型的「每个调用者各自 load-modify-write」丢更新，就丢在这个顺序性上。
    #[test]
    fn apply_ops_replays_in_order() {
        let mut list: Vec<HistoryEntry> = Vec::new();
        let a = HistoryEntry::from_task(&task_with_state("a", TaskState::Done)).unwrap();
        let b = HistoryEntry::from_task(&task_with_state("b", TaskState::Failed)).unwrap();
        apply_ops(
            &mut list,
            vec![Op::Upsert(Box::new(a.clone())), Op::Upsert(Box::new(b.clone()))],
        );
        assert_eq!(list.len(), 2);

        // 同名 upsert 覆盖，不产生第二条
        let a2 = HistoryEntry::from_task(&task_with_state("a", TaskState::Canceled)).unwrap();
        apply_ops(&mut list, vec![Op::Upsert(Box::new(a2))]);
        assert_eq!(list.len(), 2);
        assert_eq!(list.iter().find(|h| h.id == "a").unwrap().state, "canceled");

        apply_ops(&mut list, vec![Op::Remove("b".into())]);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "a");

        // 交错批次也要保持先后语义：b 先回来、又被 remove、最后 upsert
        apply_ops(
            &mut list,
            vec![
                Op::Upsert(Box::new(b)),
                Op::Remove("b".into()),
                Op::Upsert(Box::new(a)),
            ],
        );
        assert_eq!(list.len(), 1, "remove 之后的 upsert 会再插回来，按顺序算");

        apply_ops(&mut list, vec![Op::Clear]);
        assert!(list.is_empty());
    }

    #[test]
    fn prepare_for_write_sorts_newest_first_and_truncates() {
        let mut list: Vec<HistoryEntry> = (0..(MAX_ENTRIES + 50))
            .map(|i| {
                let mut t = task_with_state(&format!("t{i}"), TaskState::Done);
                t.created_at = i as i64;
                HistoryEntry::from_task(&t).unwrap()
            })
            .collect();
        prepare_for_write(&mut list);
        assert_eq!(list.len(), MAX_ENTRIES, "超出上限要截掉最旧的");
        assert_eq!(list[0].created_at, (MAX_ENTRIES + 49) as i64, "新的在前");
        // 被截掉的是 created_at 最小的那 50 条
        assert!(list.iter().all(|h| h.created_at >= 50));
    }
}
