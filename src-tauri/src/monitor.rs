//! 下载监视框：有活动下载时自动浮出的置顶进度窗。
//!
//! 行为（IDM 的浮动监视框语义）：
//! - 活跃（Downloading/Merging）任务数 > 0 → 自动显示（不抢焦点）
//! - 活跃数归零 → 自动隐藏
//! - 用户手动关闭（×）→ 本次下载批次内不再自动浮出，直到全部结束归零
//!
//! 为什么复用进度窗口：它本来就是「只看进行中」的视图，监视框只是让它有了
//! 自动出现/消失的生命周期，不再依赖手动打开；置顶让它能挂在别的窗口旁边盯进度。

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// 用户手动关掉监视框 = 本批次不再打扰。空闲归零时复位。
static DISMISSED: AtomicBool = AtomicBool::new(false);

/// 依据活跃任务数同步监视框的可见性。泵的每次收尾都会调用——所有
/// 「开始下载」与「下载结束」路径最终都汇到泵。
pub async fn sync(app: &tauri::AppHandle, tasks: &crate::engine::SharedTasks) {
    let active = {
        let guard = tasks.lock().await;
        guard
            .values()
            .filter(|t| {
                matches!(
                    t.state,
                    crate::engine::TaskState::Downloading | crate::engine::TaskState::Merging
                )
            })
            .count()
    };

    if active > 0 {
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
pub async fn ensure(app: &tauri::AppHandle, focus: bool) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("progress") {
        let _ = win.show();
        let _ = win.unminimize();
        if focus {
            let _ = win.set_focus();
        }
        return Ok(());
    }

    let win = WebviewWindowBuilder::new(app, "progress", WebviewUrl::App("progress.html".into()))
        .title("VeloxGrab 下载监视")
        .inner_size(420.0, 320.0)
        .min_inner_size(360.0, 240.0)
        .resizable(true)
        .always_on_top(true)
        .build()
        .map_err(|e| format!("打开监视窗口失败: {e}"))?;
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
