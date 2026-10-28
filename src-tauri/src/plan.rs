//! 「全部下载完成后」的一次性动作（关机 / 重启 / 睡眠 / 打开文件）。
//!
//! ## 为什么放在内存里，不落盘
//!
//! 关机是一次性意图，只对「这一次挂机」有意义。落盘的话，用户第二天打开
//! 程序、随便下个小文件，就会被昨天忘了取消的计划关一次机——这种惊吓
//! 远比「重启后要重新设一次」严重。所以计划只活在本次进程里。
//!
//! ## 什么时候算「全部完成」
//!
//! 队列空 + 没有占用中的并发槽 + 没有排队/下载/合并/暂停的任务 + 至少有一个
//! 成功的任务。**有失败就不触发**：下挂了三个文件还照常关机，是比不关机
//! 更糟的体验；此时计划保持待命，用户可以重试失败的任务，成功后仍会执行。
//!
//! ## 执行前留 60 秒
//!
//! 关机和重启都用 `shutdown /s|/r /t 60`，并给前端发一条带「取消」按钮的
//! 提示（取消走 `shutdown /a`）。用户挂机时最怕的就是「来不及反应」。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::engine::{SharedTasks, TaskState};

/// 关机/重启的延迟秒数。够用户看到提示并点「取消」。
const DELAY_SECS: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Shutdown,
    Restart,
    Sleep,
    OpenFile,
}

impl PlanAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "shutdown" => Some(Self::Shutdown),
            "restart" => Some(Self::Restart),
            "sleep" => Some(Self::Sleep),
            "open_file" => Some(Self::OpenFile),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Restart => "restart",
            Self::Sleep => "sleep",
            Self::OpenFile => "open_file",
        }
    }

    /// 用户可见的动作名，用于提示文案。
    pub fn label(self) -> &'static str {
        match self {
            Self::Shutdown => "关机",
            Self::Restart => "重启",
            Self::Sleep => "睡眠",
            Self::OpenFile => "打开文件",
        }
    }
}

/// 每个 worker 收尾、并且续泵跑完之后调一次。
///
/// 位置很关键：必须在 `pump_queue` **之后**。泵还没跑时，上一个任务刚释放
/// 的槽位可能立刻被下一个排队任务占走；在泵之前判断「没有活跃任务」会
/// 在并发队列里误判成「全下完了」。
pub async fn check_and_fire(
    app: &tauri::AppHandle,
    tasks: &SharedTasks,
    queue: &Arc<Mutex<VecDeque<String>>>,
    running: &Arc<AtomicU64>,
) {
    use tauri::Manager;
    let Some(state) = app.try_state::<crate::AppState>() else {
        return;
    };
    // 没设计划时这是最热的路径（每次任务结束都会走一遍），先做一次廉价判断。
    let armed = match state.plan.lock() {
        Ok(g) => g.is_some(),
        Err(e) => e.into_inner().is_some(),
    };
    if !armed {
        return;
    }
    if running.load(Ordering::SeqCst) > 0 {
        return;
    }
    if !queue.lock().await.is_empty() {
        return;
    }

    // 锁内只看状态，不做任何 IO：判断完立刻放锁，别让任务表卡在这里。
    let last_path = {
        let guard = tasks.lock().await;
        let mut any_done = false;
        let mut last: Option<(i64, String)> = None;
        for t in guard.values() {
            match t.state {
                TaskState::Done => {
                    any_done = true;
                    if let Some(p) = t.output_path.as_ref() {
                        // 取「最近创建」的那个成品，作为「打开文件」的目标
                        if last.as_ref().map(|(ts, _)| t.created_at > *ts).unwrap_or(true) {
                            last = Some((t.created_at, p.clone()));
                        }
                    }
                }
                // 还有活着的、排队的、或失败的 → 这轮不算完成
                TaskState::Queued
                | TaskState::Downloading
                | TaskState::Merging
                | TaskState::Paused
                | TaskState::Failed => return,
                TaskState::Canceled => {}
            }
        }
        if !any_done {
            return;
        }
        last.map(|(_, p)| p)
    };

    // take() 保证一次性：即使两个 worker 同时走到这里，也只有一个能拿到动作。
    let action = {
        let mut g = match state.plan.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.take()
    };
    let Some(action) = action else {
        return;
    };

    match execute(app, action, last_path.as_deref()) {
        Ok(()) => {
            use tauri::Emitter;
            let _ = app.emit(
                "plan-fired",
                serde_json::json!({
                    "action": action.as_str(),
                    "label": action.label(),
                    "delay": DELAY_SECS,
                }),
            );
        }
        Err(e) => {
            // 执行失败就如实说，并把计划清掉（已经 take 走了）：留着一个
            // 「显示已设定但永远不会执行」的标记只会让人困惑。
            use tauri::Emitter;
            let _ = app.emit(
                "plan-failed",
                serde_json::json!({
                    "action": action.as_str(),
                    "label": action.label(),
                    "error": e,
                }),
            );
        }
    }
}

fn execute(app: &tauri::AppHandle, action: PlanAction, path: Option<&str>) -> Result<(), String> {
    match action {
        PlanAction::Shutdown => schedule_shutdown("/s"),
        PlanAction::Restart => schedule_shutdown("/r"),
        PlanAction::Sleep => sleep_now(),
        PlanAction::OpenFile => open_finished(app, path),
    }
}

/// `shutdown.exe` 走绝对路径：PATH 被污染时不该让「关机」变成执行别的程序。
fn schedule_shutdown(flag: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let comment = format!(
        "VeloxGrab：全部下载已完成，{} 秒后执行。要取消请运行 shutdown /a。",
        DELAY_SECS
    );
    let status = std::process::Command::new(r"C:\Windows\System32\shutdown.exe")
        .arg(flag)
        .arg("/t")
        .arg(DELAY_SECS.to_string())
        .arg("/c")
        .arg(comment)
        // CREATE_NO_WINDOW：别在任务栏闪一个黑色控制台窗口
        .creation_flags(0x0800_0000)
        .status()
        .map_err(|e| format!("调用 shutdown 失败: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("系统拒绝了关机请求（可能被组策略禁用）".into())
    }
}

/// 立即睡眠。
///
/// `SetSuspendState(FALSE, FALSE, FALSE)`：不休眠、不强制、不关闭唤醒事件。
/// 第一个参数传 TRUE 会变成「休眠」——那是另一件事，硬盘上要写整份内存镜像。
#[cfg(windows)]
fn sleep_now() -> Result<(), String> {
    use windows_sys::Win32::System::Power::SetSuspendState;
    let ok = unsafe { SetSuspendState(0, 0, 0) };
    if ok != 0 {
        Ok(())
    } else {
        Err("进入睡眠失败（可能被电源策略阻止）".into())
    }
}

#[cfg(not(windows))]
fn sleep_now() -> Result<(), String> {
    Err("本平台未实现睡眠".into())
}

/// 打开刚下完的文件。
///
/// 可执行文件不直接运行，改为在资源管理器里定位：挂机场景下用户往往不在
/// 电脑前，「下载完成 → 自动运行一个 exe」是最不该发生的事。
fn open_finished(app: &tauri::AppHandle, path: Option<&str>) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    let Some(path) = path else {
        return Err("没有可打开的成品文件".into());
    };
    let p = std::path::PathBuf::from(path);
    if !p.exists() {
        return Err("成品文件不存在（可能已被移动或删除）".into());
    }

    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    const RISKY: &[&str] = &[
        "exe", "msi", "bat", "cmd", "ps1", "vbs", "js", "scr", "com", // 可直接执行
        "lnk", "url", "hta", "reg", "inf", "jar", "wsf", "wsh", "cpl", "msc", // 可间接拉起程序
    ];
    if RISKY.contains(&ext.as_str()) {
        return app
            .opener()
            .reveal_item_in_dir(&p)
            .map_err(|e| format!("定位文件失败: {e}"));
    }
    app.opener()
        .open_path(p.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| format!("打开文件失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::PlanAction;

    #[test]
    fn parse_accepts_known_actions_only() {
        assert_eq!(PlanAction::parse("shutdown"), Some(PlanAction::Shutdown));
        assert_eq!(PlanAction::parse(" restart "), Some(PlanAction::Restart));
        assert_eq!(PlanAction::parse("sleep"), Some(PlanAction::Sleep));
        assert_eq!(PlanAction::parse("open_file"), Some(PlanAction::OpenFile));
        assert_eq!(PlanAction::parse("none"), None);
        assert_eq!(PlanAction::parse("rm -rf"), None);
    }

    #[test]
    fn roundtrip_str() {
        for a in [
            PlanAction::Shutdown,
            PlanAction::Restart,
            PlanAction::Sleep,
            PlanAction::OpenFile,
        ] {
            assert_eq!(PlanAction::parse(a.as_str()), Some(a));
        }
    }
}
