fn main() {
    // 图标改动必须触发资源重编：tauri-build 自己的 rerun-if-changed 清单
    // 不覆盖 bundle.icon 这些文件，而它一旦发过任何 rerun-if-changed，
    // cargo 的「包内任意文件变更即重跑」默认行为就被关闭——只改图标不改
    // Rust 源码时，exe 会带着**旧图标**静默重编（实测踩过：任务栏永远
    // 是旧图，重启系统也没用）。
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/32x32.png");
    println!("cargo:rerun-if-changed=icons/128x128.png");
    tauri_build::build()
}
