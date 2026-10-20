// 解析一批 vgrab:// 链接并打印结果。
// 运行: cargo run --example link_check -- <cases.txt>
// 每行一条链接。用途：改动 deeplink.rs 后确认各种浏览器形态都能正确解析。

use std::io::BufRead;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "cases.txt".to_string());
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            println!("!! 打不开 {path}: {e}");
            std::process::exit(1);
        }
    };
    let mut ok = 0usize;
    let mut err = 0usize;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let link = line.trim();
        if link.is_empty() {
            continue;
        }
        match vgrab_lib::deeplink::parse_deep_link(link) {
            Ok(r) => {
                println!("OK   {link}");
                println!(
                    "     -> url={:?} name={:?} dir={:?} threads={:?}",
                    r.url, r.filename, r.save_dir, r.threads
                );
                ok += 1;
            }
            Err(e) => {
                println!("ERR  {link}");
                println!("     -> {e}");
                err += 1;
            }
        }
        println!();
    }
    println!("共 {ok} 条 OK / {err} 条 ERR");
}
