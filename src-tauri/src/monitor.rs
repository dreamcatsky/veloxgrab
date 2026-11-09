//! 下载监视框：有活动下载时自动浮出的置顶列表窗。
//!
//! 行为（对齐 Ghost-Downloader-3 的浏览器插件监视面板）：
//! - 活跃（Downloading/Merging）任务数 > 0 → 自动显示（不抢焦点）
//! - 活跃数归零、且没有暂停/失败/刚完成的任务 → 自动隐藏
//! - 用户手动关闭（×）→ 本次下载批次内不再自动浮出，直到全部结束归零
//!
//! 内容改为**逐任务列表**：每行一个任务（图标、名称、状态、进度条、速度、
//! 行内暂停/继续/打开），不再是把所有任务聚合成一个大数字。
//! 尺寸不再用 s/m/l 三档：窗口可自由缩放，大小与位置都由 progress.js
//! 存进 localStorage（用户拖一次就记住）。
//!
//! 置顶让它能挂在别的窗口旁边盯进度；监视窗只是「看」，真正的操作入口
//! 仍然是主窗口——但列表里给了最常用的两个（暂停/继续、打开文件）。

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::settings::Settings;

/// 用户手动关掉监视框 = 本批次不再打扰。空闲归零时复位。
static DISMISSED: AtomicBool = AtomicBool::new(false);

/// 默认尺寸（逻辑像素）：能舒服放下 3~4 行任务。
const DEFAULT_SIZE: (f64, f64) = (320.0, 240.0);

/// 最小尺寸：一行任务的完整信息 + 标题栏。
const MIN_SIZE: (f64, f64) = (240.0, 96.0);

/// 本批次刚完成的任务要保持窗口可见的时长。
///
/// 与 progress.js 的 RETIRE_MS 一致：用户需要时间在窗里看到「已完成」并点
/// 「打开」。Rust 侧这里只负责「别在任务刚完成时立刻隐藏窗口」。
const RECENT_DONE_KEEP: std::time::Duration = std::time::Duration::from_secs(60);

/// 本批次首个任务的开始时间（用于区分「本次下载」与历史回灌）。
///
/// 监视窗是「这次下载」的视图：历史里的 done 任务不该让它一直浮着。
/// 进程启动时刻作为基准——早于它完成的任务都算历史。
static SESSION_START: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

fn session_start_ms() -> i64 {
    *SESSION_START.get_or_init(crate::commands::now_ms)
}
///
/// 尺寸不再由设置决定（窗口可自由缩放，大小由 progress.js 持久化）；
/// 这里只负责**首次创建**时给默认尺寸，以及把「始终置顶」之类的窗口
/// 属性对齐到设置。保留函数是为了让 `ensure` 的调用点不用改。
fn apply_prefs(win: &tauri::WebviewWindow, s: &Settings) {
    // 用户说过「不要置顶」时跟随；默认保持置顶（监视窗的语义）。
    let _ = win.set_always_on_top(s.monitor_always_on);
    // 透明度：低到能透出背景但不影响读数
    let opacity = s.monitor_opacity.clamp(0.5, 1.0);
    let js = format!("document.body.style.setProperty('--m-opacity','{opacity}');");
    if let Err(e) = win.eval(&js) {
        eprintln!("[monitor] 应用监视窗偏好失败: {e}");
    }
}

/// 依据任务状态同步监视框的可见性。泵的每次收尾都会调用——所有
/// 「开始下载」与「下载结束」路径最终都汇到泵。
///
/// 判据不再是「只有活跃任务才显示」：列表里还要展示暂停/失败/刚完成的
/// 任务（用户需要在窗里点「继续」或「打开」）。只有在**完全没有可看内容**
/// 时才隐藏——即没有活跃、没有暂停/失败、也没有本批次刚完成的任务。
/// 一条任务是否让监视窗「值得显示」。
///
/// 纯函数（不碰 AppHandle），便于单测这段判据——它是「窗口该不该浮出」的
/// 唯一真相，写错的表现是「下载中却没有监视窗」或「窗口永远不走」，
/// 两种都很难在实机上定位。
fn task_keeps_monitor(
    state: crate::engine::TaskState,
    created_at: i64,
    session_start: i64,
    recent_cutoff: i64,
) -> bool {
    use crate::engine::TaskState;
    match state {
        // 正在下载/合并：当然显示
        TaskState::Downloading | TaskState::Merging => true,
        // 暂停/失败：用户要在窗里点继续；排队：马上开始
        TaskState::Paused | TaskState::Failed | TaskState::Queued => true,
        // 已完成：只有本批次内刚完成的才留一会儿（让用户点「打开」）。
        // 历史回灌的 done 不该让监视窗常驻。
        TaskState::Done => created_at >= session_start && created_at >= recent_cutoff,
        TaskState::Canceled => false,
    }
}

pub async fn sync(app: &tauri::AppHandle, tasks: &crate::engine::SharedTasks) {
    let now = crate::commands::now_ms();
    let recent_cutoff = now - RECENT_DONE_KEEP.as_millis() as i64;
    let session = session_start_ms();
    let keep = {
        let guard = tasks.lock().await;
        guard
            .values()
            .any(|t| task_keeps_monitor(t.state, t.created_at, session, recent_cutoff))
    };

    if keep {
        if DISMISSED.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = ensure(app, false).await {
            eprintln!("[monitor] 监视框显示失败: {e}");
        }
        return;
    }

    // 空闲：隐藏 + 复位 dismissed，下一批下载重新自动浮出
    DISMISSED.store(false, Ordering::Relaxed);
    if let Some(w) = app.get_webview_window("progress") {
        let _ = w.hide();
    }
}

/// 确保进度窗口存在并可见。`focus` = 是否抢焦点（手动打开要，自动浮出不要——
/// 用户正在打字时弹窗抢焦点是最招人烦的一类交互）。
///
/// 每次显示都重放一遍置顶/透明度偏好：用户改完保存立即生效，不依赖窗口重建。
pub async fn ensure(app: &tauri::AppHandle, focus: bool) -> Result<(), String> {
    let prefs = app
        .try_state::<crate::AppState>()
        .map(|st| crate::settings::load_settings(st.inner()));

    if let Some(win) = app.get_webview_window("progress") {
        let _ = win.show();
        let _ = win.unminimize();
        if let Some(s) = &prefs {
            apply_prefs(&win, s);
        }
        if focus {
            let _ = win.set_focus();
        }
        return Ok(());
    }

    let win = WebviewWindowBuilder::new(app, "progress", WebviewUrl::App("progress.html".into()))
        .title("VeloxGrab 下载监视")
        .inner_size(DEFAULT_SIZE.0, DEFAULT_SIZE.1)
        .min_inner_size(MIN_SIZE.0, MIN_SIZE.1)
        // 列表要能放下任意多任务：窗口可缩放，尺寸由 progress.js 持久化
        .resizable(true)
        .decorations(false)
        .transparent(true)
        .always_on_top(prefs.as_ref().map(|s| s.monitor_always_on).unwrap_or(true))
        .skip_taskbar(true)
        .build()
        .map_err(|e| format!("打开监视窗口失败: {e}"))?;
    if let Some(s) = &prefs {
        apply_prefs(&win, s);
    }
    // 用户手动关闭 = 本批次不再自动浮出（自动隐藏走 hide()，不触发这里）。
    // 关闭监听挂在构建后的窗口实例上：这个 tauri 版本的 builder 没有
    // on_window_event 链式方法。
    win.on_window_event(|e| {
        if let tauri::WindowEvent::CloseRequested { .. } = e {
            DISMISSED.store(true, Ordering::Relaxed);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::task_keeps_monitor;
    use crate::engine::TaskState;

    /// 判据的核心语义（写反会让监视窗「该出不出」或「永远不走」）：
    /// 活跃/暂停/失败/排队一律显示；已完成只在**本批次**内留一会儿；
    /// 历史回灌的 done / canceled 一律不显示。
    #[test]
    fn monitor_visibility_predicate() {
        let session = 1_000_000i64;
        let now = session + 10_000;
        let cutoff = now - 60_000; // 60 秒内的完成算「刚完成」

        // 活跃三类：总是显示
        for st in [
            TaskState::Downloading,
            TaskState::Merging,
            TaskState::Paused,
            TaskState::Failed,
            TaskState::Queued,
        ] {
            assert!(task_keeps_monitor(st, session + 1, session, cutoff), "{st:?} 应显示");
        }

        // 本批次内刚完成：显示
        assert!(task_keeps_monitor(TaskState::Done, now - 5_000, session, cutoff));
        // 本批次内、但完成太久（created_at 早于 cutoff）：不显示
        assert!(!task_keeps_monitor(TaskState::Done, cutoff - 1, session, cutoff));
        // 历史回灌（创建早于本进程启动）：不显示，哪怕时间上很近
        assert!(!task_keeps_monitor(TaskState::Done, session - 1, session, cutoff));
        // 取消是终态，从不显示
        assert!(!task_keeps_monitor(TaskState::Canceled, now, session, cutoff));
    }
}
