//! 系统托盘图标。
//!
//! 关窗口 = 缩到托盘，程序继续在后台下载；真正退出要从托盘菜单选「退出」。
//! 这是下载器的常规行为——用户关掉窗口往往只是不想看见它，不是想停掉下载。

use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

pub const SHOW_ID: &str = "tray_show";
pub const QUIT_ID: &str = "tray_quit";

/// 建托盘图标。失败只打日志：托盘不是核心功能，不能因为它让程序起不来。
pub fn setup(app: &tauri::AppHandle) {
    let show = match MenuItemBuilder::with_id(SHOW_ID, "显示窗口").build(app) {
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

    let menu = match MenuBuilder::new(app).item(&show).item(&quit).build() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[tray] 建菜单失败: {e}");
            return;
        }
    };

    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("VGrab")
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
