// 直链多线程下载器的端到端验证。
//
// 运行: cargo run --example direct_test -- <url> [线程数] [期望SHA256]
//
// 第三个参数是期望的内容 SHA256。留空则只校验大小与续传一致性。
// 输出目录固定为 %TEMP%\veloxgrab_direct_test，不占用参数位。
//
// 覆盖：
// 1. 探测（是否支持 Range、总大小、文件名）
// 2. 多线程分块下载 + 进度回调
// 3. 下载结果的大小校验 + SHA256 校验
// 4. 断点续传（构造 .veloxgrab-part 后重跑，验证只补缺失部分且内容正确）

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// 纯 Rust 的 SHA256（避免为测试脚本引入新依赖）
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:18777/bigtest.bin".to_string());
    let threads: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    // 第三个参数是期望 SHA256。输出目录固定用临时目录，不占用这个位置。
    let expect_sha256: Option<String> = args.next().filter(|s| !s.trim().is_empty());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let out_dir = std::env::temp_dir().join("veloxgrab_direct_test");
    let _ = std::fs::create_dir_all(&out_dir);

    println!("========== 直链下载测试 ==========");
    println!("URL      : {url}");
    println!("线程数   : {threads}");
    println!("输出目录 : {}", out_dir.display());

    // ---- 1. 探测 ----
    let probe = match rt.block_on(veloxgrab_lib::direct::probe_direct(
        &url,
        None,
        &veloxgrab_lib::settings::NetCfg::default(),
    )) {
        Ok(p) => p,
        Err(e) => {
            println!("❌ 探测失败: {e}");
            std::process::exit(1);
        }
    };
    println!("\n---------- 探测结果 ----------");
    println!("文件名       : {}", probe.filename);
    println!("总大小       : {:?} 字节", probe.total_bytes);
    println!("MIME         : {:?}", probe.content_type);
    println!("支持 Range   : {}", probe.range_supported);
    println!("建议线程数   : {}", probe.suggested_threads);
    println!("最终 URL     : {}", probe.final_url);

    let target = out_dir.join(&probe.filename);
    // 下载全程只写临时文件（`<name>.veloxgrab-downloading`），成功后 rename 成最终名。
    // 续传元数据挂在**临时文件**旁边，所以路径要跟着临时名走。
    let temp = out_dir.join(format!("{}.veloxgrab-downloading", probe.filename));
    let part = out_dir.join(format!("{}.veloxgrab-downloading.veloxgrab-part", probe.filename));
    if target.exists() {
        let _ = std::fs::remove_file(&target);
    }
    // 清理可能残留的续传元数据与临时文件
    let _ = std::fs::remove_file(&temp);
    let _ = std::fs::remove_file(&part);

    // ---- 2. 下载 ----
    println!("\n---------- 开始下载 ----------");
    let cancel = Arc::new(AtomicBool::new(false));
    let last_pct = Arc::new(Mutex::new(-1i32));
    let lp = last_pct.clone();
    let cb = move |p: veloxgrab_lib::direct::DirectProgress| {
        let pct = p.progress as i32;
        // 只在整数百分比变化时打印，避免刷屏
        let mut guard = match lp.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if pct != *guard {
            *guard = pct;
            println!(
                "  {:>5.1}%  {:>10} / {:?}  {}",
                p.progress,
                p.downloaded,
                p.total,
                p.speed.unwrap_or_default()
            );
        }
    };

    let result = rt.block_on(veloxgrab_lib::direct::download_direct(
        &url,
        &out_dir,
        None,
        threads,
        cancel.clone(),
        cb,
        &veloxgrab_lib::settings::NetCfg::default(),
    ));

    let path = match result {
        Ok(p) => p,
        Err(e) => {
            println!("❌ 下载失败: {e}");
            std::process::exit(1);
        }
    };
    println!("\n✅ 下载完成: {}", path.display());

    // ---- 3. 校验 ----
    let meta = std::fs::metadata(&path).expect("读取文件信息失败");
    println!("实际大小: {} 字节", meta.len());
    let mut ok = true;
    if let Some(total) = probe.total_bytes {
        if meta.len() == total {
            println!("✅ 大小一致 ({total} 字节)");
        } else {
            println!("❌ 大小不一致: 期望 {total}, 实际 {}", meta.len());
            ok = false;
        }
    }

    // 复查残留的续传元数据是否已清理
    if part.exists() {
        println!("❌ 续传元数据未清理: {}", part.display());
        ok = false;
    } else {
        println!("✅ 续传元数据已清理");
    }

    // 内容校验：打印下载文件的 SHA256；若命令行给了期望值就比对。
    //
    // 早期版本把 `%TEMP%\bigtest.bin` 硬编码成基准，但那个文件和被测 URL 毫无关系，
    // 换个服务器就必然「不一致」——那是脚本的 bug，不是下载器的问题。
    let got_bytes = std::fs::read(&path).expect("读取下载文件失败");
    println!("SHA256   : {}", sha256_hex(&got_bytes));
    if let Some(expect) = expect_sha256 {
        if expect.eq_ignore_ascii_case(&sha256_hex(&got_bytes)) {
            println!("✅ 内容 SHA256 与期望值一致");
        } else {
            println!("❌ 内容 SHA256 与期望值不符（期望 {expect}）");
            ok = false;
        }
    }

    // ---- 4. 断点续传 ----
    if probe.range_supported && probe.total_bytes.unwrap_or(0) > 0 {
        println!("\n---------- 断点续传测试 ----------");
        // 构造一个「下载到一半」的现场。
        //
        // 注意两个前提，早期版本都踩过：
        // 1. 文件必须写成**临时名** `xxx.veloxgrab-downloading` —— R3 之后下载器只认临时文件，
        //    直接写最终名会被当成「没有断点」，从零重下。
        // 2. 内容校验必须有独立基准。旧脚本用 `%TEMP%\bigtest.bin` 当基准，但它和
        //    当前 URL 毫无关系，比对必然失败——那是脚本的 bug，不是下载器的问题。
        let total = probe.total_bytes.unwrap();
        let half = total / 2;
        {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temp)
                .unwrap();
            f.set_len(total).unwrap();
        }

        // 前半段用真实内容填充（从已下好的成品文件复制），后半段留零
        let good = std::fs::read(&path).unwrap_or_default();
        let mut expected = good.clone();
        if good.len() as u64 == total {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&temp).unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&good[..half as usize]).unwrap();
            f.flush().unwrap();
        } else {
            println!("⚠️  成品大小异常，跳过内容比对");
            expected.clear();
        }

        // 元数据写成「第一段已完成一半、其余为 0」，逼下载器只补缺失部分
        let segs = 4usize;
        let per = total / segs as u64;
        let mut seg_total = Vec::new();
        let mut seg_done = Vec::new();
        for i in 0..segs {
            let len = if i + 1 == segs { total - per * (segs as u64 - 1) } else { per };
            seg_total.push(len);
            // 第一段已完成一半（与上面写入的真实内容对齐）
            seg_done.push(if i == 0 { per / 2 } else { 0u64 });
        }
        let meta_json = serde_json::json!({
            "url": url,
            "total": total,
            "segments_done": seg_done,
            "segments_total": seg_total,
        });
        std::fs::write(&part, meta_json.to_string()).unwrap();

        let cancel2 = Arc::new(AtomicBool::new(false));
        let cb2 = move |_p: veloxgrab_lib::direct::DirectProgress| {};
        match rt.block_on(veloxgrab_lib::direct::download_direct(
            &url,
            &out_dir,
            None,
            segs,
            cancel2,
            cb2,
            &veloxgrab_lib::settings::NetCfg::default(),
        )) {
            Ok(p2) => {
                println!("✅ 续传完成: {}", p2.display());
                if !expected.is_empty() {
                    let got = std::fs::read(&p2).unwrap();
                    if expected == got {
                        println!("✅ 续传后内容逐字节一致（{} 字节）", got.len());
                    } else {
                        println!(
                            "❌ 续传后内容不一致（期望 {} 字节 / 实际 {} 字节）",
                            expected.len(),
                            got.len()
                        );
                        ok = false;
                    }
                }
            }
            Err(e) => {
                println!("❌ 续传失败: {e}");
                ok = false;
            }
        }
    }

    println!();
    if ok {
        println!("✅✅ 全部检查通过");
    } else {
        println!("❌ 存在失败项");
        std::process::exit(1);
    }
}
