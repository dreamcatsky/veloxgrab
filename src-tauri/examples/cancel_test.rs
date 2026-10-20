// 取消功能验证 + 诊断。
//
// 用法: cargo run --example cancel_test -- <url> [取消延迟毫秒] [线程数]
//
// 关键观察点：
// 1. 取消后 download_direct 多久返回
// 2. 取消后网络流是否真的停止（服务器端应收到连接关闭）
// 3. 续传元数据保留情况

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:18780/bigtest.bin".to_string());
    let delay_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1500);
    let threads: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(4);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let out_dir = std::env::temp_dir().join("veloxgrab_cancel_test");
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    println!("========== 取消测试 ==========");
    println!("URL      : {url}");
    println!("取消延迟 : {delay_ms} ms");
    println!("分块线程 : {threads}");

    let cancel = Arc::new(AtomicBool::new(false));

    // 记录每秒进度，便于观察取消前后是否还在收数据
    let last_bytes = Arc::new(Mutex::new(0u64));
    let lb = last_bytes.clone();
    let t0 = Instant::now();

    // 触发取消
    let cancel_trigger = cancel.clone();
    let spawned = tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        println!("[{}] → 触发取消标志", t0.elapsed().as_millis());
        cancel_trigger.store(true, Ordering::SeqCst);
    });

    let started = Instant::now();
    let result = rt.block_on(veloxgrab_lib::direct::download_direct(
        &url,
        &out_dir,
        None,
        threads,
        cancel.clone(),
        move |p| {
            if let Ok(mut g) = lb.lock() {
                *g = p.downloaded;
            }
        },
        &veloxgrab_lib::settings::NetCfg::default(),
    ));
    let elapsed = started.elapsed();
    // 取消已经触发过了，这个句柄没别的用途；显式 drop 而不是 `let _ =`，
    // 后者不会消费 future（clippy::let_underscore_future）。
    drop(spawned);

    let downloaded = *last_bytes.lock().unwrap_or_else(|e| e.into_inner());
    println!(
        "[{}] 下载调用返回，已收 {} 字节",
        elapsed.as_millis(),
        downloaded
    );

    let mut ok = true;
    match result {
        Err(e) if e.contains("已取消") => {
            println!("✅ 返回取消错误: {e}");
            println!("   取消触发到返回耗时: {:.2}s", elapsed.as_secs_f64());
            if elapsed > Duration::from_secs(5) {
                println!("❌ 取消响应过慢（>5s）—— 说明 reqwest 的数据流没有及时中断");
                ok = false;
            }
        }
        Ok(p) => {
            println!("❌ 期望被取消，实际完成了: {}", p.display());
            ok = false;
        }
        Err(e) => {
            println!("❌ 非取消错误: {e}");
            ok = false;
        }
    }

    let part = out_dir.join("bigtest.bin.veloxgrab-part");
    if part.exists() {
        let text = std::fs::read_to_string(&part).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let done: u64 = v["segments_done"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_u64()).sum())
            .unwrap_or(0);
        println!("✅ 续传元数据保留，记录已完成 {done} 字节");
        if done == 0 {
            println!("❌ 已完成字节为 0 —— 续传将从头开始，取消等于全废");
            ok = false;
        }
    } else {
        println!("❌ 续传元数据不存在");
        ok = false;
    }

    println!();
    if ok {
        println!("✅✅ 取消测试通过");
    } else {
        println!("❌ 取消测试失败");
        std::process::exit(1);
    }
}
