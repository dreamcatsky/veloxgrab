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

use std::sync::atomic::{AtomicBool, Ordering};
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

/// 托盘图标的 id。`AppHandle::tray_by_id` 靠它找到实例——任务完成时要改
/// tooltip（「3 个任务已完成」），没有 id 就只能拿到一个「某个托盘图标」。
pub const TRAY_ID: &str = "veloxgrab_tray";

/// 托盘菜单项触发的「前端命令」事件。
///
/// 载荷是 runMenuCommand 认得的命令名——这样托盘菜单、菜单栏、快捷键
/// 三条入口共用同一个执行器，不会出现「托盘里的暂停全部和菜单里的行为不一样」。
pub const TRAY_CMD_EVENT: &str = "tray-command";

/// 托盘是否成功建立。lib.rs 的关窗处理据此降级：「最小化到托盘」的前提是
/// 托盘真的在——托盘失败 × close_mode=background 的组合会让窗口永久消失
/// 而进程还在跑，用户只能靠「再启动一个实例」救回。
static TRAY_OK: AtomicBool = AtomicBool::new(false);

/// 托盘当前是否可用（含后台重试成功的情况）。
pub fn is_available() -> bool {
    TRAY_OK.load(Ordering::SeqCst)
}

/// 建托盘图标。首次失败不放弃（开机自启时 Explorer/任务栏可能尚未就绪，
/// Shell_NotifyIcon 会瞬时失败），后台线程重试三次。
pub fn setup(app: &tauri::AppHandle) {
    if try_setup(app) {
        TRAY_OK.store(true, Ordering::SeqCst);
        return;
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if try_setup(&app2) {
                TRAY_OK.store(true, Ordering::SeqCst);
                eprintln!("[tray] 重试后托盘建立成功");
                return;
            }
        }
        eprintln!("[tray] 重试后仍失败：本次会话「最小化到托盘」不可用，关窗将走「询问」");
    });
}

/// 单次建立尝试。任一步失败返回 false（托盘不是核心功能，不能让它把程序
/// 拖垮；降级策略见 is_available 的注释）。
fn try_setup(app: &tauri::AppHandle) -> bool {
    let show = match MenuItemBuilder::with_id(SHOW_ID, "显示窗口").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return false;
        }
    };
    // 常用动作：不需要看见任务表就能做的事
    let open_dir = match MenuItemBuilder::with_id(OPEN_DIR_ID, "打开下载文件夹").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return false;
        }
    };
    let paste = match MenuItemBuilder::with_id(PASTE_ID, "从剪贴板添加链接").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return false;
        }
    };
    let pause_all = match MenuItemBuilder::with_id(PAUSE_ALL_ID, "全部暂停").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return false;
        }
    };
    let resume_all = match MenuItemBuilder::with_id(RESUME_ALL_ID, "全部开始").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return false;
        }
    };
    let quit = match MenuItemBuilder::with_id(QUIT_ID, "退出").build(app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[tray] 建菜单项失败: {e}");
            return false;
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
            return false;
        }
    };

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
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

    builder.build(app).is_ok()
}
