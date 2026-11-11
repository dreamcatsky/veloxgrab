// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Native Messaging 宿主形态：浏览器按注册表里的 manifest 拉起本进程，
    // stdin/stdout 走 NM 帧协议。这条路绝不能初始化 Tauri（无窗口、无事件
    // 循环、生命周期由浏览器管理），必须在 run() 之前分流。
    // 判据见 `nm_host::is_nm_host`（浏览器传的是扩展 origin，不是 `--nm-host`）。
    let argv: Vec<String> = std::env::args().collect();
    if veloxgrab_lib::nm_host::is_nm_host(&argv) {
        veloxgrab_lib::nm_host::run_nm_host();
    }
    veloxgrab_lib::run()
}
