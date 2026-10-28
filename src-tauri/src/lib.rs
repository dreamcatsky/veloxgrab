pub mod bridge;
pub mod clipboard;
pub mod commands;
pub mod decode;
pub mod deeplink;
pub mod dpapi;
pub mod direct;
pub mod engine;
pub mod history;
pub mod save_dir;
pub mod settings;
pub mod tray;
pub mod update;
pub mod util;
pub mod ytdlp;

use commands::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
// Emitter 提供 app.emit(...)。不引入它，派发外部链接时编译不过。
use tauri::{Emitter, Manager, RunEvent};

/// 标记「退出清理已经做过」。
///
/// `AppHandle::exit()` 会再发一次 `RunEvent::ExitRequested`。如果没有这个标记，
/// 处理函数会在「prevent_exit → exit → 再次 ExitRequested → prevent_exit」之间死循环。
static EXIT_CLEANED: AtomicBool = AtomicBool::new(false);

/// 从进程参数里找出第一个 `veloxgrab://` 链接。
///
/// Windows 上协议处理程序的启动方式是 `veloxgrab.exe "%1"`，用户点的 URL
/// 会作为**第一个**命令行参数出现。热启动时（已有实例在跑）这个参数
/// 由 `single_instance` 回调拿到，冷启动时只能自己从 `args()` 里翻。
///
/// 用泛型而不是 `impl IntoIterator`：`std::env::args()` 与 `Vec<String>`
/// 的迭代器 Item 都是 `String`，泛型版本两种都能直接传。
fn deep_link_from_args<I>(argv: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    let prefix = format!("{}://", deeplink::SCHEME);
    argv.into_iter()
        .skip(1)
        .map(|a| a.trim().trim_matches('"').to_string())
        .find(|a| a.to_ascii_lowercase().starts_with(&prefix))
}

/// 把一条 `veloxgrab://` 链接交给前端：先进待取队列，再发一个唤醒事件。
///
/// 解析失败的链接也照样入队，让前端 toast 出原因：用户点的是网页上的按钮，
/// 链接长什么样不受我们控制，静默丢弃会让用户以为按钮坏了。
///
/// 为什么是「入队 + 唤醒」而不是「直接把 payload 塞进事件」：
/// Tauri 的 `emit` 对还没挂 `listen` 的 webview 是**静默丢弃**，而冷启动时
/// webview2 首帧要一秒以上。原先只发事件的写法在冷启动路径上 100% 丢链接
/// （实测：进程命令行里明明带着 `veloxgrab://…`，任务列表却一条都没有）。
/// 现在事件退化成纯唤醒信号（空 payload），数据一律由前端主动
/// invoke `take_pending_deep_links` 拉取，取走即清空，天然不会重复处理。
pub(crate) fn dispatch_external_link(app: &tauri::AppHandle, link: String) {
    let payload = match deeplink::parse_deep_link(&link) {
        Ok(req) => match serde_json::to_value(&req) {
            Ok(v) => v,
            Err(e) => serde_json::json!({ "error": format!("序列化失败: {e}") }),
        },
        Err(e) => serde_json::json!({ "error": e }),
    };

    // state 可能还没 manage（single_instance 回调可能早于 setup 完成）。
    // 那时也没有 webview 可唤醒，链接直接丢弃是不可接受的，所以只在这种情况下
    // 打一条日志——正常路径上 state 一定在。
    if let Some(state) = app.try_state::<AppState>() {
        let mut guard = match state.pending_links.lock() {
            Ok(g) => g,
            Err(err) => err.into_inner(),
        };
        guard.push(payload);
        drop(guard);
    } else {
        eprintln!("[deeplink] AppState 尚未就绪，链接未能入队: {link}");
        return;
    }

    // 唤醒信号：前端已就绪时能立刻处理，不用等用户操作。
    let _ = app.emit(deeplink::EXTERNAL_LINK_EVENT, ());

    // 把主窗口拉到前台。浏览器点链接时窗口通常还在，但可能被别的程序盖住、
    // 最小化了，甚至停在托盘上——只发事件的话用户会觉得「点了没反应」。
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 第二个实例启动意味着用户又点了一个 veloxgrab:// 链接（或手动开了第二个窗口）。
            // 链接必须在这里消费掉：新进程马上就会退出，不处理就白点一次。
            if let Some(link) = deep_link_from_args(argv) {
                dispatch_external_link(app, link);
            }
            // 无论有没有链接，都把已有窗口拉到前台，给用户「点开了」的反馈
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .setup(|app| {
            // 记录 Tauri 资源目录，供 yt-dlp / ffmpeg 定位使用。
            // 安装后资源位于 $INSTDIR（resource_dir），与 exe 同级或位于其 bin/ 下。
            if let Ok(dir) = app.path().resource_dir() {
                ytdlp::set_resource_dir(dir);
            }

            // 接管 veloxgrab:// 协议。写 HKCU\Software\Classes\veloxgrab 指向本 exe。
            // 安装后首次启动时做一次：NSIS 只负责拷文件，协议关联要在程序侧补。
            // 失败不阻塞启动——只是网页点击会退化成「无法识别的链接」。
            // 放独立线程：reg.exe 是外部进程，同步跑会拖慢窗口显示。
            if let Some(exe) = deeplink::installed_exe() {
                std::thread::spawn(move || {
                    if let Err(e) = deeplink::ensure_protocol_registered(&exe) {
                        eprintln!("[deeplink] 注册协议失败: {e}");
                    }
                });
            }

            // 冷启动：用户直接点链接进来的，argv 里就带着 URL。
            // 派发要等 app.manage(state) 之后——前端收到事件会 invoke
            // new_download，那时 state 必须已经在。所以先取出链接，等会儿再发。
            let cold_link = deep_link_from_args(std::env::args().collect::<Vec<_>>());

            let state = AppState::new();

            // 启动时扫一遍下载目录，清掉陈旧的半成品与空临时目录。
            // 只清理 7 天前的：取消后立刻重开的场景还要能续传。
            // 放到独立线程里跑：目录可能很大，不能让扫描拖慢窗口显示。
            //
            // 范围必须覆盖「默认根 + 所有用户批准过的目录」：任务可以单独设
            // save_dir，只扫默认根的话，D 盘任务留下的残片永远没人收。
            let scan_dirs: Vec<std::path::PathBuf> = if let Ok(guard) = state.settings.lock() {
                let mut dirs: Vec<String> = Vec::new();
                let root = guard.download_dir.trim();
                dirs.push(if root.is_empty() {
                    crate::settings::default_download_dir()
                } else {
                    root.to_string()
                });
                dirs.extend(guard.trusted_dirs.iter().cloned());
                dirs.retain(|d| !d.trim().is_empty());
                dirs.sort();
                dirs.dedup();
                dirs.into_iter()
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .collect()
            } else {
                Vec::new()
            };
            if !scan_dirs.is_empty() {
                std::thread::spawn(move || {
                    for dir in scan_dirs {
                        engine::cleanup_stale_artifacts(
                            &dir,
                            std::time::Duration::from_secs(7 * 24 * 3600),
                        );
                    }
                });
            }

            app.manage(state);

            // 托盘图标：关窗口缩到托盘，后台继续下载。
            tray::setup(app.handle());

            // 浏览器扩展的传输桥（127.0.0.1:58943）。
            // 扩展拉不起 veloxgrab://（Chrome 静默拦截自定义协议导航），
            // 这条回环 HTTP 是真正能用的那条路。失败不阻断启动。
            bridge::spawn(app.handle().clone());

            // 启动后 6 秒查一次新版本（错开首帧渲染与扩展引导弹窗）。
            // 结果经 "update-available" 事件交给前端：有更新才弹窗，
            // 检查失败静默——网络不畅时不该为「查不到有没有新版」打扰用户。
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                    let current = handle.package_info().version.to_string();
                    let proxy = handle
                        .try_state::<AppState>()
                        .and_then(|s| crate::settings::load_settings(s.inner()).proxy_url());
                    let info = update::fetch_latest(proxy.as_deref(), &current).await;
                    if info.available {
                        let _ = handle.emit("update-available", &info);
                    }
                });
            }

            // 点窗口的「×」不退出，只隐藏——下载还在跑。真正退出的入口是
            // 托盘菜单里的「退出」，它会走 RunEvent::ExitRequested 的清理流程。
            if let Some(win) = app.get_webview_window("main") {
                let win2 = win.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = win2.hide();
                    }
                });
            }

            // 冷启动的链接在这里派发：state 已就绪，可以入队。
            // 不再需要「延迟 300ms 等前端挂 listen」——数据在队列里，
            // 前端 boot() 结束时会自己来取，事件发丢也不影响正确性。
            if let Some(link) = cold_link {
                dispatch_external_link(app.handle(), link);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::probe_url,
            commands::probe_direct_url,
            commands::probe_auto,
            commands::enqueue_download,
            commands::new_download,
            commands::list_tasks,
            commands::cancel_task,
            commands::remove_task,
            commands::clear_finished,
            commands::health_check,
            commands::open_extension_dir,
            commands::extension_installed,
            commands::extension_active,
            commands::detect_browsers,
            commands::open_extensions_page,
            commands::take_pending_deep_links,
            commands::pause_task,
            commands::resume_task,
            commands::pause_tasks,
            commands::resume_tasks,
            commands::redo_task,
            commands::redo_tasks,
            commands::remove_tasks,
            commands::start_all,
            commands::pause_all,
            commands::clear_all_tasks,
            commands::clipboard_url,
            commands::copy_text,
            commands::open_task_file,
            commands::reveal_task_file,
            commands::open_download_dir,
            commands::app_version,
            commands::open_progress_window,
            settings::get_settings,
            settings::save_settings,
            settings::set_theme,
            settings::default_settings,
            update::check_update,
            update::update_now,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { api, .. } = event {
                // 第二次进来是我们自己调 exit(0) 触发的：直接放行
                if EXIT_CLEANED.swap(true, Ordering::SeqCst) {
                    return;
                }

                // 默认退出会立刻销毁运行时，之前 spawn 出去的清理任务一帧都跑不到；
                // Windows 也不会因为父进程退出就杀掉子进程，结果就是关窗后 yt-dlp
                // 还在后台下载、文件被占用。这里先拦住退出，同步清理完再退。
                api.prevent_exit();

                if let Some(state) = app_handle.try_state::<AppState>() {
                    // 事件回调不在 tokio 运行时线程上，可以安全 block_on；
                    // 加 2 秒上限，避免某个子进程杀不掉把退出卡死。
                    let _ = tauri::async_runtime::block_on(async {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            commands::cancel_all(state.inner()),
                        )
                        .await
                    });
                }
                // cancel_all 的状态变化只投递给了历史写线程（攒批落盘）：
                // 退出前强制刷一次，否则「暂停任务重启后还在」依赖的那次写入
                // 可能停在 200ms 合并窗口里。给 1 秒——宁可丢攒批，
                // 也不能让磁盘卡住把「退出」本身卡死。
                commands::shutdown_history_writer();
                app_handle.exit(0);
            }
        });
}
