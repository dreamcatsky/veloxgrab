// 续传验证：先取消一次，再重新下载，确认从断点接续而不是从头开始
//
// 用法: cargo run --example resume_test -- <url> [取消延迟毫秒]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:18780/bigtest.bin".to_string());
    let delay_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3000);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let out_dir = std::env::temp_dir().join("vgrab_resume_test");
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();

    println!("========== 续传测试 ==========");
    println!("URL      : {url}");

    // ---------- 第一次：下到一半取消 ----------
    println!("\n[第一轮] {delay_ms}ms 后取消");
    let cancel = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicU64::new(0));
    {
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms));
            println!("  → 触发取消");
            c2.store(true, Ordering::SeqCst);
        });
    }
    let s1 = Arc::clone(&seen);
    let t1 = Instant::now();
    let r1 = rt.block_on(vgrab_lib::direct::download_direct(
        &url,
        &out_dir,
        None,
        4,
        cancel.clone(),
        move |p| {
            s1.store(p.downloaded, Ordering::Relaxed);
        },
        &vgrab_lib::settings::NetCfg::default(),
    ));
    match &r1 {
        Err(e) if e.contains("已取消") => {
            println!("  ✅ 已取消，耗时 {:.2}s", t1.elapsed().as_secs_f64());
        }
        Err(e) => {
            println!("  ❌ 意外错误: {e}");
            std::process::exit(1);
        }
        Ok(p) => {
            println!("  ⚠️ 取消太晚，第一轮已完成: {}", p.display());
            std::process::exit(0);
        }
    }

    let part = out_dir.join("bigtest.bin.vgrab-part");
    let meta_before: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&part).unwrap()).unwrap();
    let done_before: u64 = meta_before["segments_done"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64()).sum())
        .unwrap_or(0);
    let segs = meta_before["segments_done"].clone();
    println!("  断点记录: {done_before} 字节  分段明细: {segs}");

    // ---------- 第二轮：重新下载，应续传 ----------
    println!("\n[第二轮] 重新下载，期望续传而非从零开始");
    let cancel2 = Arc::new(AtomicBool::new(false));
    let first_report = Arc::new(AtomicU64::new(u64::MAX));
    let fr = Arc::clone(&first_report);
    let t2 = Instant::now();
    let r2 = rt.block_on(vgrab_lib::direct::download_direct(
        &url,
        &out_dir,
        None,
        4,
        cancel2.clone(),
        move |p| {
            // 记录首次回调的值：它就是续传起点
            let _ = fr.compare_exchange(
                u64::MAX,
                p.downloaded,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        },
        &vgrab_lib::settings::NetCfg::default(),
    ));
    let elapsed2 = t2.elapsed();
    let start_from = first_report.load(Ordering::Relaxed);
    let start_from = if start_from == u64::MAX { 0 } else { start_from };

    let mut ok = true;
    match r2 {
        Ok(p) => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            println!(
                "  ✅ 完成: {} ({} 字节) 耗时 {:.2}s",
                p.display(),
                size,
                elapsed2.as_secs_f64()
            );
            println!("  续传起点（首次进度回调）: {start_from} 字节；断点记录: {done_before} 字节");
            if done_before == 0 {
                println!("  ⚠️ 断点为 0（取消太早，未写入任何数据），本轮无法验证续传，仅验证全量下载");
            } else if start_from < done_before {
                println!("  ❌ 起点小于断点，说明没有续传");
                ok = false;
            } else {
                println!("  ✅ 起点已包含断点数据，续传生效");
            }
        }
        Err(e) => {
            println!("  ❌ 失败: {e}");
            ok = false;
        }
    }

    if part.exists() {
        println!("  ❌ 完成后 .vgrab-part 未清理");
        ok = false;
    } else {
        println!("  ✅ 临时元数据已清理");
    }

    println!();
    if ok {
        println!("✅✅ 续传测试通过");
    } else {
        println!("❌ 续传测试失败");
        std::process::exit(1);
    }
}
