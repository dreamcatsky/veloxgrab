//! 系统托盘图标。
//!
//! 关窗口 = 缩到托盘，程序继续在后台下载；真正退出要从托盘菜单选「退出」。
//! 这是下载器的常规行为——用户关掉窗口往往只是不想看见它，不是想停掉下载。
//!
//! ## 菜单为什么要「去重一层」
//!
//! 托盘菜单的用途是「窗口没开时也能做最常用的事」。所以这里只放**不需要
//! 看见任务表就能做**的动作（打开下载目录、从剪贴板添加、全部暂停/开始），
//! 加上窗口与退出。针对单条任务的操作（暂停某个任务、复制某条链接）需要
//! 知道选中的是哪个——那属于窗口内的右键菜单，塞进托盘只会让人无从选择。
//!
//! 菜单项动作走 app.emit 唤醒前端再执行，而不是在 Rust 里重写一遍业务：
//! 业务逻辑（含 toast 提示、确认框、状态刷新）全在前端一处，重写必然分叉。
//! 前端没开（窗口未创建）时这几项会静默无效果——这可以接受，因为窗口是
//! 常驻的（关闭只是隐藏），不存在「前端不可用」的稳定状态。

use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager,
};

pub const SHOW_ID: &str = "tray_show";
pub const QUIT_ID: &str = "tray_quit";
pub const OPEN_DIR_ID: &str = "tray_open_dir";
pub const PASTE_ID: &str = "tray_paste_url";
pub const PAUSE_ALL_ID: &str = "tray_pause_all";
pub const RESUME_ALL_ID: &str = "tray_resume_all";

/// 托盘菜单项触发的「前端命令」事件。
///
/// 载荷是 runMenuCommand 认得的命令名——这样托盘菜单、菜单栏、快捷键
/// 三条入口共用同一个执行器，不会出现「托盘里的暂停全部和菜单里的行为不一样」。
pub const TRAY_CMD_EVENT: &str = "tray-command";

/// 建托盘图标。失败只打日志：托盘不是核心功能，不能因为它让程序起不来。
pub fn setup(app: &tauri::AppHandle) {
    let show = match MenuItemBuilder::with_id(SHOW_ID, "显示窗口").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return;
        }
    };
    // 常用动作：不需要看见任务表就能做的事
    let open_dir = match MenuItemBuilder::with_id(OPEN_DIR_ID, "打开下载文件夹").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return;
        }
    };
    let paste = match MenuItemBuilder::with_id(PASTE_ID, "从剪贴板添加链接").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return;
        }
    };
    let pause_all = match MenuItemBuilder::with_id(PAUSE_ALL_ID, "全部暂停").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return;
        }
    };
    let resume_all = match MenuItemBuilder::with_id(RESUME_ALL_ID, "全部开始").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return;
        }
    };
    let quit = match MenuItemBuilder::with_id(QUIT_ID, "退出").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return;
        }
    };

    let menu = match MenuBuilder::new(app)
        .item(&show)
        .separator()
        .item(&open_dir)
        .item(&paste)
        .separator()
        .item(&resume_all)
        .item(&pause_all)
        .separator()
        .item(&quit)
        .build()
    {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[tray] 建菜单失败: {e}");
            return;
        }
    };

    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("VeloxGrab")
        // 右键弹菜单是 Windows 惯例；左键设为显示窗口，符合「点一下就回来」的直觉。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            SHOW_ID => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
            QUIT_ID => {
                app.exit(0);
            }
            // 以下动作统一交给前端执行器：先确保窗口可见（用户点托盘的这些
            // 项多半是想看到结果），再把命令名发过去。
            OPEN_DIR_ID | PASTE_ID | PAUSE_ALL_ID | RESUME_ALL_ID => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
                let cmd = match event.id().as_ref() {
                    OPEN_DIR_ID => "openDownloadDir",
                    PASTE_ID => "addFromClipboard",
                    PAUSE_ALL_ID => "pauseAll",
                    _ => "startAll",
                };
                let _ = app.emit(TRAY_CMD_EVENT, cmd);
            }
            _ => {}
        })
        .on_tray_icon_event(|icon, event| {
            // 左键单击托盘图标 = 显示窗口。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(w) = icon.app_handle().get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
        });

    // 用打包进去的应用图标，托盘和窗口、任务栏看起来是同一个程序。
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    match builder.build(app) {
        Ok(_) => {}
        Err(e) => eprintln!("[tray] 建托盘图标失败: {e}"),
    }
}
