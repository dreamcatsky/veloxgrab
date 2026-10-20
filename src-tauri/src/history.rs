//! 下载历史持久化。
//!
//! 之前任务表只在内存里，程序一关全部记录清零：用户找不到昨天下的文件在哪、
//! 失败的任务是什么原因、点了「移除」的记录也彻底消失。
//!
//! 这里把每个进入终态的任务单独 upsert 成 JSON：
//! - 入队、状态变化（完成/失败/取消）时同步一条；
//! - 进度事件（10Hz）**不**触发写盘——否则 3 个并发就是每秒 30 次磁盘 IO。
//!
//! 文件采用「写临时文件 + rename」的原子替换：直接 write 在崩溃时会留下
//! 截断的 JSON，下次启动整份历史都读不回来。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::engine::Task;

/// 单个历史条目。
///
/// 不复用 `Task`：Task 带 `#[serde(default)]` 的历史包袱，且历史只需要终态信息，
/// 没有进度/速度这种瞬时字段。字段保持兼容读取（旧版本只有这些字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub url: String,
    pub title: String,
    pub kind: String,
    /// "done" / "failed" / "canceled"
    pub state: String,
    pub quality_label: String,
    pub output_path: Option<String>,
    pub save_dir: Option<String>,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
    pub created_at: i64,
    /// 任务结束（完成/失败/取消）的时间戳
    pub finished_at: Option<i64>,
}

impl HistoryEntry {
    /// 从终态任务构造历史条目。非终态任务返回 None。
    pub fn from_task(task: &Task) -> Option<Self> {
        let state = match task.state {
            crate::engine::TaskState::Done => "done",
            crate::engine::TaskState::Failed => "failed",
            crate::engine::TaskState::Canceled => "canceled",
            _ => return None,
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
            finished_at: Some(crate::commands::now_ms()),
        })
    }

    /// 把历史条目还原成一个**终态**任务，用于启动时回灌任务表。
    ///
    /// 「下载记录」页面已经删掉了（它的内容和「所有下载 / 正在下载 / 已完成」
    /// 三个筛选完全重复），但历史文件本身还在承担持久化职责：任务表只在内存里，
    /// 不回灌的话用户每次重启都会看到空列表，昨天下的文件一条都找不到。
    ///
    /// 还原出来的任务一律是终态——不进队列、不启动下载，只供界面筛选与
    /// 「打开所在文件夹」用。瞬时字段（进度/速度/ETA）没有意义，全部清零或留空。
    pub fn to_task(&self) -> Task {
        let state = match self.state.as_str() {
            "done" => crate::engine::TaskState::Done,
            "failed" => crate::engine::TaskState::Failed,
            _ => crate::engine::TaskState::Canceled,
        };
        Task {
            id: self.id.clone(),
            url: self.url.clone(),
            title: self.title.clone(),
            thumbnail: None,
            duration: None,
            format_id: "direct".into(),
            quality_label: self.quality_label.clone(),
            has_audio: false,
            best_audio_format: None,
            state,
            progress: if state == crate::engine::TaskState::Done {
                100.0
            } else {
                0.0
            },
            downloaded_bytes: self.total_bytes.unwrap_or(0),
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
            threads: 0,
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

/// 读取全部历史（新的在前）。文件不存在或损坏时返回空列表。
pub fn load_history() -> Vec<HistoryEntry> {
    let path = history_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<HistoryEntry>>(&text) {
        Ok(list) => list,
        Err(e) => {
            // 与 settings 一致：坏文件改名备份，不要让它永远挡住后续写入
            let bad = path.with_extension("json.bad");
            let _ = std::fs::rename(&path, &bad);
            eprintln!(
                "history.json 解析失败（{e}），已备份到 {}，本次历史为空",
                bad.display()
            );
            Vec::new()
        }
    }
}

/// 原子写入历史文件。
fn write_history(list: &[HistoryEntry]) -> Result<(), String> {
    let path = history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_string(list).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入历史失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("保存历史失败: {e}"))?;
    Ok(())
}

/// 单条历史 Upsert（任务结束时调用，不必遍历整张任务表）。
pub fn upsert_entry(task: &Task) {
    let Some(entry) = HistoryEntry::from_task(task) else {
        return;
    };
    let mut list = load_history();
    match list.iter_mut().find(|h| h.id == entry.id) {
        Some(slot) => *slot = entry,
        None => list.push(entry),
    }
    list.sort_by_key(|h| std::cmp::Reverse(h.created_at));
    if list.len() > MAX_ENTRIES {
        list.truncate(MAX_ENTRIES);
    }
    if let Err(e) = write_history(&list) {
        eprintln!("{e}");
    }
}

/// 从历史里删除一条（用户点「删除记录」）。
pub fn remove_entry(id: &str) -> Result<(), String> {
    let mut list = load_history();
    let before = list.len();
    list.retain(|h| h.id != id);
    if list.len() == before {
        return Ok(());
    }
    write_history(&list)
}

/// 清空全部历史。
pub fn clear_all() -> Result<(), String> {
    write_history(&[])
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
    fn only_terminal_tasks_become_history() {
        // 进行中的任务不能进历史，否则历史里全是半成品
        assert!(HistoryEntry::from_task(&task_with_state("a", TaskState::Downloading)).is_none());
        assert!(HistoryEntry::from_task(&task_with_state("a", TaskState::Queued)).is_none());
        assert!(HistoryEntry::from_task(&task_with_state("a", TaskState::Merging)).is_none());

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
}
