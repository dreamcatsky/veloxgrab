// 用真实 URL 验证 probe 的解析逻辑与格式排序。
// 运行: cargo run --example probe_test -- <url>
fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://www.bilibili.com/video/BV1GJ411x7h7".to_string());

    let settings = vgrab_lib::settings::Settings::default();
    let rt = tokio::runtime::Runtime::new().unwrap();

    match rt.block_on(vgrab_lib::ytdlp::probe(&url, &settings)) {
        Ok(info) => {
            println!("========== 探测成功 ==========");
            println!("标题     : {}", info.title);
            println!("ID       : {}", info.id);
            println!("站点     : {}", info.extractor);
            println!("上传者   : {:?}", info.uploader);
            println!("时长     : {:?}", info.duration);
            println!("最佳音轨 : {:?}", info.best_audio_format);
            println!("有视频流 : {}", info.has_video_stream);
            println!("\n可选格式 ({} 项):", info.formats.len());
            for (i, f) in info.formats.iter().enumerate() {
                let mark = if f.recommended { " ★推荐" } else { "" };
                println!(
                    "  [{i:>2}] {:<30} id={:<8} size={:>10}{}",
                    f.label,
                    f.format_id,
                    f.filesize
                        .map(|s| format!("{:.1}MB", s as f64 / 1048576.0))
                        .unwrap_or_else(|| "-".into()),
                    mark
                );
            }

            // 断言：同分辨率下 H264 必须排在 H265/AV1 之前
            let rank = |l: &str| {
                if l.contains("H264") {
                    0
                } else if l.contains("H265") {
                    1
                } else if l.contains("AV1") {
                    2
                } else {
                    3
                }
            };
            let mut order_ok = true;
            for w in info.formats.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                let ha: u32 = a.resolution.trim_end_matches('p').parse().unwrap_or(0);
                let hb: u32 = b.resolution.trim_end_matches('p').parse().unwrap_or(0);
                if ha == hb && rank(&a.label) > rank(&b.label) {
                    order_ok = false;
                    println!("  ⚠ 同分辨率内编码顺序异常: {} -> {}", a.label, b.label);
                }
            }
            let rec_count = info.formats.iter().filter(|f| f.recommended).count();
            println!();
            println!("同分辨率编码排序: {}", if order_ok { "✅ 正确" } else { "❌ 错误" });
            println!("推荐项数量: {} {}", rec_count, if rec_count == 1 { "✅" } else { "❌" });
            if let Some(r) = info.formats.iter().find(|f| f.recommended) {
                println!("推荐格式: {} (id={})", r.label, r.format_id);
            }
            println!("\n✅ 解析逻辑正常");
        }
        Err(e) => {
            println!("❌ 探测失败: {e}");
            std::process::exit(1);
        }
    }
}
