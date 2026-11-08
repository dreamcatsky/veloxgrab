//! 下载监视框：有活动下载时自动浮出的置顶进度窗。
//!
//! 行为（IDM 的浮动监视框语义）：
//! - 活跃（Downloading/Merging）任务数 > 0 → 自动显示（不抢焦点）
//! - 活跃数归零 → 自动隐藏
//! - 用户手动关闭（×）→ 本次下载批次内不再自动浮出，直到全部结束归零
//!
//! 尺寸与内容行由设置驱动（monitor_size: s|m|l + monitor_show_* 三开关）：
//! 每次浮出前同步一次设置，用户改完保存立即生效，不需要重启。
//! 复用进度窗口：它本来就是「只看进行中」的视图，监视框只是让它有了
//! 自动出现/消失的生命周期；置顶让它能挂在别的窗口旁边盯进度。

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{LogicalSize, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::settings::Settings;

/// 用户手动关掉监视框 = 本批次不再打扰。空闲归零时复位。
static DISMISSED: AtomicBool = AtomicBool::new(false);

/// 尺寸档位 → 逻辑像素（宽, 高）。
///
/// s = 只看大数字（关名称/副行时的推荐档）；m = 升级前的默认尺寸；
/// l = 三行全开时的舒适档。收敛到三档：无框小窗做自由缩放的手柄成本
/// （与拖拽区打架）配不上收益。
fn size_of(preset: &str) -> (f64, f64) {
    match preset.trim() {
        "s" => (196.0, 92.0),
        "l" => (288.0, 168.0),
        _ => (236.0, 128.0),
    }
}

/// 把设置的监视窗偏好应用到窗口：尺寸 + 内容行可见性。
///
/// 内容行用 body 上的 data 属性交给 CSS（display:none），而不是逐元素
/// remove——用户在设置里反复切换时元素结构保持稳定，progress.js 不用
/// 关心「节点在不在」。
fn apply_prefs(win: &tauri::WebviewWindow, s: &Settings) {
    let (w, h) = size_of(&s.monitor_size);
    let _ = win.set_size(LogicalSize::new(w, h));
    let js = format!(
        "document.body.dataset.monName='{}';document.body.dataset.monSub='{}';document.body.dataset.monBar='{}';",
        s.monitor_show_name as u8, s.monitor_show_sub as u8, s.monitor_show_bar as u8,
    );
    if let Err(e) = win.eval(&js) {
        eprintln!("[monitor] 应用监视窗偏好失败: {e}");
    }
}

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
///
/// 每次显示都重放一遍设置里的尺寸/内容行偏好：用户改完保存立即生效，
/// 不依赖窗口重建。
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

    let (w, h) = prefs
        .as_ref()
        .map(|s| size_of(&s.monitor_size))
        .unwrap_or((236.0, 128.0));
    let win = WebviewWindowBuilder::new(app, "progress", WebviewUrl::App("progress.html".into()))
        .title("VeloxGrab 下载监视")
        .inner_size(w, h)
        .min_inner_size(120.0, 64.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
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
