// Cookie 降级验证：在浏览器 Cookie 不可读时，probe 仍应解析成功
//
// 用法: cargo run --example cookie_test -- [视频URL]
//
// 背景：Chrome/Edge 127+ 的 App-Bound Encryption 让 yt-dlp 读不到 Cookie 库，
// 强行带 Cookie 会让本来能解析的公开视频也失败。修复后的 probe 应做到：
//   1. 不带 Cookie 先试 → 成功则完全不碰浏览器 Cookie
//   2. 失败才带 Cookie 重试
//
// 本测试把 cookies_from_browser 设成 chrome（正是会报错的那个），
// 然后验证 probe 依然成功。

use std::time::Instant;

#[tokio::main]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://www.youtube.com/watch?v=UXEjocWEfiM".to_string());

    println!("========== Cookie 降级测试 ==========");
    println!("URL: {url}");

    // 故意配置一个读不出来的浏览器 Cookie 来源
    let settings = vgrab_lib::settings::Settings {
        cookies_from_browser: "chrome".to_string(),
        cookies_file: String::new(),
        ..Default::default()
    };
    println!(
        "配置: cookies_from_browser={:?}",
        settings.cookies_from_browser
    );
    println!("（Chrome 127+ 启用了 App-Bound Encryption，直接带这个 Cookie 会报错）\n");

    let t = Instant::now();
    let result = vgrab_lib::ytdlp::probe(&url, &settings).await;
    let elapsed = t.elapsed();

    match &result {
        Ok(info) => {
            println!("[{} ms] 解析成功", elapsed.as_millis());
            println!("  标题  : {}", info.title);
            println!("  格式数: {}", info.formats.len());
            if let Some(d) = info.duration {
                println!("  时长  : {d:.1} 秒");
            }
            let rec = info.formats.iter().find(|f| f.recommended);
            if let Some(r) = rec {
                println!("  推荐  : {} ({})", r.label, r.format_id);
            }
            println!("\n✅ Cookie 降级生效：浏览器 Cookie 不可读也不影响公开视频解析");
        }
        Err(e) => {
            println!("[{} ms] 解析失败", elapsed.as_millis());
            println!("  {e}");
            println!("\n❌ 降级未生效");
            std::process::exit(1);
        }
    }
}
