//! 任务完成提醒：闪任务栏 + 更新托盘提示。
//!
//! ## 为什么不做系统 toast
//!
//! Windows 的 toast 需要一个注册过的 AppUserModelID（通常靠开始菜单里的
//! 快捷方式建立）。便携版、绿色版、以及「直接双击 exe」的场景经常拿不到，
//! 结果就是代码不报错、通知就是不出现——这种「静默失效」比没有通知更糟。
//!
//! 任务栏闪烁（`request_user_attention`）和托盘 tooltip 都是进程内就能保证
//! 生效的：前者让用户知道「有结果了」，后者鼠标一悬停就能看到「几个已完成」。
//! 对下载器来说这已经够用，而且零依赖。
//!
//! ## 为什么窗口在前台时不闪
//!
//! 用户正盯着任务列表，进度条自己会变成「已完成」，再闪一下任务栏纯属噪音。

use tauri::{Manager, UserAttentionType};

use crate::engine::{SharedTasks, TaskState};

/// worker 收尾时调用。`done` 表示这次收尾是成功还是失败/取消。
pub async fn after_worker(
    app: &tauri::AppHandle,
    tasks: &SharedTasks,
    done: bool,
) {
    let enabled = app
        .try_state::<crate::AppState>()
        .map(|s| crate::settings::load_settings(s.inner()).notify_on_complete)
        .unwrap_or(false);

    // 托盘提示是静态信息，即使关掉提醒也照常更新——它不打扰任何人。
    update_tooltip(app, tasks).await;

    if !enabled || !done {
        return;
    }

    if let Some(win) = app.get_webview_window("main") {
        if !win.is_focused().unwrap_or(false) {
            // 只在后台时闪：前台用户已经能看到列表里的状态变化。
            let _ = win.request_user_attention(Some(UserAttentionType::Informational));
        }
    }
}

/// 托盘 tooltip 跟着队列状态走。
///
/// 不显示百分比：tooltip 只有一行、刷新频率又高，跳数字反而看不清。
/// 用户要细节就点开窗口。
async fn update_tooltip(app: &tauri::AppHandle, tasks: &SharedTasks) {
    let (total, running, done) = {
        let guard = tasks.lock().await;
        let mut total = 0usize;
        let mut running = 0usize;
        let mut done = 0usize;
        for t in guard.values() {
            total += 1;
            match t.state {
                TaskState::Downloading | TaskState::Merging => running += 1,
                TaskState::Done => done += 1,
                _ => {}
            }
        }
        (total, running, done)
    };

    let text = if running > 0 {
        format!("VeloxGrab · 正在下载 {running} 个")
    } else if done > 0 && done == total {
        format!("VeloxGrab · {done} 个任务已完成")
    } else {
        "VeloxGrab".to_string()
    };

    if let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) {
        let _ = tray.set_tooltip(Some(text));
    }
}
