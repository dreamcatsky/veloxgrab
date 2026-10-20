//! 通用多线程直链下载器（HTTP Range 分块下载）
//!
//! 设计要点：
//! - 先发 HEAD（失败则退化为带 Range 的 GET）探测：是否支持 Range、总大小、文件名、MIME
//! - 支持 Range 时把文件切成 N 段，每段一个连接并发拉取，各自 seek 写入同一个文件
//! - 不支持 Range 时退化为单流顺序下载
//! - 断点续传：每段已写偏移定期落盘到 `<文件名>.veloxgrab-part`，重启后按偏移继续
//! - 取消通过共享 AtomicBool 实现，不依赖 reqwest 的取消机制

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::settings::NetCfg;

/// 分块时每段的最小字节数：太小会让连接数虚高，收益被请求开销吃掉
const MIN_CHUNK_SIZE: u64 = 1024 * 1024; // 1 MiB
/// 低于这个大小不值得分块
const MIN_MULTI_THREAD_SIZE: u64 = 4 * 1024 * 1024; // 4 MiB
/// 并发分块数硬上限（对齐 TDM Fast 的「默认下载线程数 1-64」）。
///
/// IPC 可以被任意调用方传入更大的值。1 GB 文件传 1000 会真的开出 1000 条连接：
/// 端口耗尽、被站点限流、fd 打满。
const MAX_THREADS: usize = 64;
/// 下载中的临时文件后缀。完成后再 rename 成最终名字。
const TEMP_SUFFIX: &str = ".veloxgrab-downloading";

/// 直链探测结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectProbe {
    pub url: String,
    pub filename: String,
    pub total_bytes: Option<u64>,
    pub content_type: Option<String>,
    pub range_supported: bool,
    /// 跟随重定向后的最终 URL
    pub final_url: String,
    /// 建议并发数
    pub suggested_threads: usize,
}

/// 进度回调数据
#[derive(Debug, Clone, Serialize)]
pub struct DirectProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub progress: f64,
    pub speed: Option<String>,
    /// 预计剩余时间。速度还测不出来（刚起步 / 暂停中）时为 None，
    /// 界面显示「—」而不是编一个数字出来。
    pub eta: Option<String>,
}

/// 构造 HTTP 客户端。`proxy` 是 settings.proxy_url() 的结果（None = 直连）。
fn client_with(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) VeloxGrab/0.1")
        .connect_timeout(Duration::from_secs(20))
        // 这里绝对不能用 `timeout()`：reqwest 的 timeout 是「总超时」，从连接建立
        // 一直算到响应体读完。10 GB 文件在 2 MB/s 的线路上要 85 分钟，超过 1 小时
        // 会被直接掐断。`read_timeout` 是空闲读超时——30 秒没有新数据才判定断流，
        // 正常的大文件下载不受总时长限制。
        .read_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(p) = proxy {
        b = b.proxy(
            reqwest::Proxy::all(p).map_err(|e| format!("代理配置无效: {e}"))?,
        );
    }
    b.build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))
}

/// 全局限速：固定 100ms 窗口，窗口内累计字节超预算就睡到下一窗口。
///
/// 为什么不用令牌桶：这里的预算粒度是「每 100ms 允许过多少字节」，
/// 窗口法实现最短、误差最多一个窗口（100ms 的突发），对限速场景完全够用；
/// 而且所有分块共用一个 Mutex，锁只在「攒够一批要落盘」时抢一次，
/// 不在每 8 KiB 的流块上抢。
/// static 里不能调 `Instant::now()`（非 const fn），所以窗口状态用 `Option`
/// 表示「还没开窗口」：第一个进限速批次的调用负责初始化窗口起点。
static RATE_WINDOW: Mutex<Option<(Instant, u64)>> = Mutex::new(None);
const RATE_WINDOW_MS: u64 = 100;

/// 请求把 `need` 字节记入限速窗口；超预算时异步等待。rate_kb = 0 表示不限速。
async fn rate_wait(rate_kb: u64, need: u64) {
    if rate_kb == 0 || need == 0 {
        return;
    }
    let budget = rate_kb.saturating_mul(1024) * RATE_WINDOW_MS / 1000;
    let budget = budget.max(1);
    // 单批就超一个窗口的预算（比如限速 1KB/s 时攒了 256KiB）：
    // 按需要几个窗口来睡，而不是死循环等一个永远装不下的窗口。
    let windows_needed = need.div_ceil(budget);
    loop {
        let wait = {
            let mut g = match RATE_WINDOW.lock() {
                Ok(g) => g,
                Err(err) => err.into_inner(),
            };
            let (start, used) = match *g {
                Some(w) => w,
                None => {
                    *g = Some((Instant::now(), need));
                    return;
                }
            };
            let elapsed = start.elapsed();
            if elapsed >= Duration::from_millis(RATE_WINDOW_MS) {
                *g = Some((Instant::now(), need));
                return;
            }
            if used + need <= budget {
                *g = Some((start, used + need));
                return;
            }
            let remain = Duration::from_millis(RATE_WINDOW_MS) - elapsed;
            // 等完这个窗口还不一定够（need 可能跨多个窗口），按整窗数估一个下限；
            // 下一轮循环会重新检查，最多多睡一个窗口，不会睡过头太多。
            remain.max(Duration::from_millis(RATE_WINDOW_MS * (windows_needed - 1)))
        };
        // 睡在锁外：抱着全局锁睡觉会把所有并发分块一起卡死。
        tokio::time::sleep(wait).await;
    }
}

fn chrono_stamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

fn extract_disposition_filename(cd: &str) -> Option<String> {
    // 优先 RFC 5987 的 filename*=UTF-8''xxx
    for part in cd.split(';') {
        if let Some(rest) = part.trim().strip_prefix("filename*=") {
            let rest = rest.trim_matches('"');
            let encoded = rest.splitn(3, '\'').nth(2).unwrap_or(rest);
            if let Ok(decoded) = urlencoding::decode(encoded) {
                return Some(decoded.into_owned());
            }
        }
    }
    for part in cd.split(';') {
        if let Some(rest) = part.trim().strip_prefix("filename=") {
            let rest = rest.trim_matches('"').trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// 从 Content-Disposition / URL / MIME 推断文件名
fn guess_filename(
    url: &str,
    content_disposition: Option<&str>,
    content_type: Option<&str>,
) -> String {
    if let Some(cd) = content_disposition {
        if let Some(name) = extract_disposition_filename(cd) {
            if !name.trim().is_empty() {
                return crate::util::sanitize_filename(&name);
            }
        }
    }
    let path_part = url.split('?').next().unwrap_or(url);
    if let Some(last) = path_part.rsplit('/').find(|s| !s.is_empty()) {
        let decoded = urlencoding::decode(last)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| last.to_string());
        let has_ext = Path::new(&decoded)
            .extension()
            .map(|e| e.len() <= 5)
            .unwrap_or(false);
        if has_ext {
            return crate::util::sanitize_filename(&decoded);
        }
    }
    let ct = content_type.unwrap_or("");
    let ext = if ct.contains("zip") {
        "zip"
    } else if ct.contains("pdf") {
        "pdf"
    } else if ct.contains("json") {
        "json"
    } else if ct.contains("mpeg") && ct.contains("audio") {
        "mp3"
    } else if ct.starts_with("image/png") {
        "png"
    } else if ct.starts_with("image/jpeg") {
        "jpg"
    } else if ct.starts_with("image/webp") {
        "webp"
    } else if ct.starts_with("text/") {
        "txt"
    } else if ct.contains("mp4") {
        "mp4"
    } else {
        "bin"
    };
    format!("download_{}.{ext}", chrono_stamp())
}

/// 探测整体上限。
///
/// HEAD 只给 3 秒，但真正决定成败的是 `Range: bytes=0-0` 那个请求——
/// 它没有任何超时（client 的 timeout 是 1 小时）。服务器半开连接或响应极慢时，
/// 整个探测会挂在那里，用户点取消也毫无反应。
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// 最近一次探测结果缓存。
///
/// 前端点「解析」时已经探过一次，用户点下载时 `download_direct` 又探一次，
/// 一次下载等于两次 HEAD + 两次 Range 探测。这里把最近一次结果留 60 秒，
/// URL 一致就复用，省掉一整轮往返。
static LAST_PROBE: Mutex<Option<(String, Instant, DirectProbe)>> = Mutex::new(None);
const PROBE_CACHE_TTL: Duration = Duration::from_secs(60);

fn cached_probe(url: &str) -> Option<DirectProbe> {
    let g = LAST_PROBE.lock().ok()?;
    let (cached_url, at, probe) = g.as_ref()?;
    if cached_url == url && at.elapsed() < PROBE_CACHE_TTL {
        Some(probe.clone())
    } else {
        None
    }
}

fn store_probe(url: &str, probe: &DirectProbe) {
    if let Ok(mut g) = LAST_PROBE.lock() {
        *g = Some((url.to_string(), Instant::now(), probe.clone()));
    }
}

/// 轮询取消标志，供 `select!` 使用（AtomicBool 本身没有 await 接口）
async fn wait_cancel(flag: Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 探测直链。
///
/// `cancel` 为 Some 时，探测过程可以被取消打断——否则用户在探测阶段点取消，
/// 要等整个探测跑完才有反应。
pub async fn probe_direct(
    url: &str,
    cancel: Option<Arc<AtomicBool>>,
    net: &NetCfg,
) -> Result<DirectProbe, String> {
    let fut = tokio::time::timeout(PROBE_TIMEOUT, probe_uncached(url, net.proxy.as_deref()));
    let timed_out = || format!("探测超时（{} 秒），服务器响应过慢", PROBE_TIMEOUT.as_secs());

    let probe = match cancel {
        Some(flag) => tokio::select! {
            r = fut => r.map_err(|_| timed_out())??,
            _ = wait_cancel(flag) => return Err("已取消".into()),
        },
        None => fut.await.map_err(|_| timed_out())??,
    };

    store_probe(url, &probe);
    Ok(probe)
}

/// 实际的探测逻辑（无超时、无缓存），由 `probe_direct` 包装后调用。
async fn probe_uncached(url: &str, proxy: Option<&str>) -> Result<DirectProbe, String> {
    let c = client_with(proxy)?;

    // HEAD 和 Range 探测并发发出，谁先回先用谁。
    //
    // 早期是个顺序实现：先 await HEAD、拿到结果再发 Range。慢速服务器
    // （实测本地限速服务）对 HEAD 的响应可能比 GET 慢一个数量级——HEAD
    // 要 16.5 秒，1 字节 Range 只要 1.1 秒。顺序等 HEAD 会把整个探测拖到
    // 十几秒，用户点取消后要干等，看起来像“取消没反应”。
    //
    // HEAD 本身也不是必需的：Range 响应带的 Content-Range / Content-Length
    // 已经足够判断大小和是否支持断点续传。
    // HEAD 只是“锦上添花”的元信息，给它 3 秒上限，超时就算了。
    // 不能让一个可选的探测请求把主流程卡住。
    let head_fut = async {
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            c.head(url).send(),
        )
        .await
        {
            Ok(Ok(r)) if r.status().is_success() => Some(r),
            _ => None,
        }
    };
    let range_fut = async {
        c.get(url)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
    };

    let (head_ok, range_resp) = tokio::join!(head_fut, range_fut);
    let range_resp = range_resp.map_err(|e| format!("连接失败: {e}"))?;

    let is_206 = range_resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let resp = if range_resp.status().is_success() || is_206 {
        range_resp
    } else if let Some(h) = head_ok {
        h
    } else {
        return Err(format!("服务器返回 HTTP {}", range_resp.status()));
    };

    let final_url = resp.url().to_string();
    let headers = resp.headers().clone();

    let content_length = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());

    let content_range = headers
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 判据：206 + 有 Content-Range
    let range_supported = is_206 && content_range.is_some();

    let total_bytes = if let Some(cr) = &content_range {
        cr.rsplit('/')
            .next()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .or(content_length.filter(|&n| n > 1))
    } else {
        content_length
    };

    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());

    let content_disposition = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok());

    let filename = guess_filename(&final_url, content_disposition, content_type.as_deref());

    let suggested_threads = if range_supported {
        match total_bytes {
            Some(n) if n >= 64 * 1024 * 1024 => 8,
            Some(n) if n >= MIN_MULTI_THREAD_SIZE => 4,
            _ => 1,
        }
    } else {
        1
    };

    Ok(DirectProbe {
        url: url.to_string(),
        filename,
        total_bytes,
        content_type,
        range_supported,
        final_url,
        suggested_threads,
    })
}

/// 断点续传元数据（与目标文件同目录，名为 `<文件名>.veloxgrab-part`）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PartMeta {
    pub url: String,
    pub total: u64,
    pub segments_done: Vec<u64>,
    pub segments_total: Vec<u64>,
}

impl PartMeta {
    fn path_for(target: &Path) -> PathBuf {
        let mut p = target.to_path_buf();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        p.set_file_name(format!("{name}.veloxgrab-part"));
        p
    }

    fn load(target: &Path) -> Option<PartMeta> {
        let text = std::fs::read_to_string(Self::path_for(target)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 原子落盘：先写 .tmp 再 rename。
    ///
    /// 这份元数据的唯一用途就是「进程被强杀后还能续传」，而直接 write 恰恰在
    /// 强杀时会留下截断的 JSON，load 解析失败返回 None，续传信息全丢。
    fn save(&self, target: &Path) {
        let Ok(text) = serde_json::to_string(self) else {
            return;
        };
        let path = Self::path_for(target);
        let tmp = path.with_extension("veloxgrab-part.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn remove(target: &Path) {
        let _ = std::fs::remove_file(Self::path_for(target));
    }
}

/// 把总长度切成若干段，返回闭区间列表
fn split_ranges(total: u64, threads: usize) -> Vec<(u64, u64)> {
    if total == 0 {
        return vec![(0, 0)];
    }
    let max_by_size = (total / MIN_CHUNK_SIZE).max(1) as usize;
    let n = threads.clamp(1, MAX_THREADS).min(max_by_size);
    let per = total / n as u64;
    let mut out = Vec::with_capacity(n);
    let mut start = 0u64;
    for i in 0..n {
        let end = if i + 1 == n {
            total - 1
        } else {
            ((start + per).saturating_sub(1)).min(total - 1)
        };
        out.push((start, end));
        start = end + 1;
    }
    out
}

fn human_speed(bytes_per_sec: f64) -> String {
    const K: f64 = 1024.0;
    if bytes_per_sec >= K * K * K {
        format!("{:.2} GiB/s", bytes_per_sec / (K * K * K))
    } else if bytes_per_sec >= K * K {
        format!("{:.2} MiB/s", bytes_per_sec / (K * K))
    } else if bytes_per_sec >= K {
        format!("{:.1} KiB/s", bytes_per_sec / K)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

/// 把「还剩多少字节 / 每秒多少字节」换算成 mm:ss 或 h:mm:ss。
///
/// 速度取不到或已下完时返回 None，让界面显示「—」。宁可空着也不要
/// 用 0 或者极大值糊弄——剩余时间显示成 99:59:59 比不显示更让人困惑。
fn human_eta(remaining: u64, speed_bps: f64) -> Option<String> {
    if speed_bps <= 0.0 || remaining == 0 {
        return None;
    }
    let secs = (remaining as f64 / speed_bps).round();
    // 超过 100 小时就不报具体数了，那时候的估算本身没有意义
    if !secs.is_finite() || secs > 100.0 * 3600.0 {
        return None;
    }
    let s = secs as u64;
    if s >= 3600 {
        Some(format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60))
    } else {
        Some(format!("{:02}:{:02}", s / 60, s % 60))
    }
}

/// 分块下载的共享计数状态。
///
/// `bytes`：每段累计已接收字节（用于进度汇总）
/// `segments_done`：每段"已确认落盘"的字节（用于断点续传记录）
///
/// 每个元素都是独立的 `AtomicU64`，不用 `Mutex<Vec<u64>>`：
/// 分块每写一批就要更新一次计数，而进度线程也在高频读它。
/// 早期为看一眼取消标志去抢同一把锁，实测把 2.5 秒的下载拖成 11.5 秒；
/// 后来改成只更新计数，可争用量级没变——这里彻底改成无锁。
///
/// 取消信号也不走这里：分块直接读传入的 `Arc<AtomicBool>`。
struct SegCounters {
    bytes: Vec<AtomicU64>,
    segments_done: Vec<AtomicU64>,
}

/// 单次 write 的聚合上限。
///
/// reqwest 的 `bytes_stream` 每块通常只有 8 KiB，逐块 `write_all` 会变成
/// 每秒上万次 syscall + `spawn_blocking` 往返。聚合到 256 KiB 再落盘，
/// 系统调用数量直接降到 1/32。
const WRITE_BATCH: usize = 256 * 1024;
/// 低速时按时间兜底 flush，避免进度条长时间不动
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);

/// 下载一段区间并写入文件的指定偏移
#[allow(clippy::too_many_arguments)]
async fn download_segment(
    c: reqwest::Client,
    url: String,
    start: u64,
    end: u64,
    total: u64,
    file_path: PathBuf,
    already_done: u64,
    counters: Arc<SegCounters>,
    seg_index: usize,
    rate_kb: u64,
    cancel: Arc<AtomicBool>,
) -> Result<u64, String> {
    let seg_len = end - start + 1;
    if already_done >= seg_len {
        return Ok(already_done);
    }
    let from = start + already_done;

    let resp = c
        .get(&url)
        .header(reqwest::header::RANGE, format!("bytes={from}-{end}"))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;

    let st = resp.status();
    if st != reqwest::StatusCode::PARTIAL_CONTENT && st != reqwest::StatusCode::OK {
        return Err(format!("分块请求返回 HTTP {st}"));
    }
    // 200 表示服务器忽略了 Range，body 是**整个文件**而不是这一段。
    // 只有本次请求本身就覆盖整个文件时（单段、from == 0、end 是文件末尾），
    // 把它写到 from 才是等价的；否则会把整份文件从中间开始覆盖，静默写坏数据。
    if st == reqwest::StatusCode::OK
        && (already_done > 0 || from != 0 || total == 0 || end + 1 != total)
    {
        return Err("服务器忽略了 Range 请求，无法分块下载".into());
    }

    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&file_path)
        .await
        .map_err(|e| format!("打开临时文件失败: {e}"))?;
    file.seek(std::io::SeekFrom::Start(from))
        .await
        .map_err(|e| format!("定位写入偏移失败: {e}"))?;

    let mut written = already_done;
    let mut buf: Vec<u8> = Vec::with_capacity(WRITE_BATCH);
    let mut last_flush = Instant::now();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        // 取消标志是无锁的 AtomicBool，每块读一次也不会有争用
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let chunk = chunk.map_err(|e| format!("读取数据流失败: {e}"))?;
        buf.extend_from_slice(&chunk);

        // 攒够一批、或距上次落盘超过 200ms 就写一次。
        // 时间兜底是为了低速链接上进度条别长时间不动。
        if buf.len() >= WRITE_BATCH || last_flush.elapsed() >= FLUSH_INTERVAL {
            // 限速按「已下载的字节」计：数据已经收进内存，先过闸再落盘。
            // 放在 write 前而不是 stream 里，是为了只在每 256 KiB 抢一次全局锁，
            // 而不是每个 8 KiB 流块抢一次。
            rate_wait(rate_kb, buf.len() as u64).await;
            file.write_all(&buf)
                .await
                .map_err(|e| format!("写入失败: {e}"))?;
            // written 只统计**真正落盘**的字节：续传按它跳过，多算一个字节
            // 就会在下次续传时丢掉一个字节。
            written += buf.len() as u64;
            buf.clear();
            last_flush = Instant::now();
            counters.bytes[seg_index].store(written, Ordering::Relaxed);
        }
    }

    // 收尾：缓冲区里剩下的也写掉（取消时同样要写，这些字节已经收下了，
    // 不写就等于白下载，而且 written 与磁盘内容会不一致）
    if !buf.is_empty() {
        rate_wait(rate_kb, buf.len() as u64).await;
        file.write_all(&buf)
            .await
            .map_err(|e| format!("写入失败: {e}"))?;
        written += buf.len() as u64;
        buf.clear();
    }
    file.flush().await.map_err(|e| format!("刷新失败: {e}"))?;
    counters.bytes[seg_index].store(written, Ordering::Relaxed);

    // 没被取消就必须写满本段。连接被中途掐断时 reqwest 不一定会报错，
    // 少了这一步就会把「少写了一段」当成成功。
    if !cancel.load(Ordering::SeqCst) {
        check_segment_len(seg_len, written)?;
    }

    // 关键：把已落盘字节数记入 segments_done，后续续传据此跳过。
    //
    // 必须取 max 而不是直接覆盖：分块可能在“已收到不少数据”时被取消，
    // 而进度线程已经记录了更大的值。早期用 *d = written 直接赋值，
    // 取消发生在循环刚入口时会把进度线程记好的几 MB 覆盖成 0，
    // 导致续传等于从头再来。
    counters.segments_done[seg_index].fetch_max(written, Ordering::Relaxed);
    counters.bytes[seg_index].fetch_max(written, Ordering::Relaxed);
    Ok(written)
}

/// 校验一个分块是否写满。
///
/// `written` 是**整段的累计字节**（`download_segment` 的初值就是 `already_done`，
/// 每落盘一批累加一次），所以这里必须直接跟 `seg_len` 比。
///
/// 曾经写成 `written - already_done != seg_len`：续传时 `already_done > 0`，
/// 左边只是「本次新增字节」，等式永远不成立 —— 结果断点续传从来没成功过，
/// 每次续传都必然报「分块长度不符」。这个函数的存在就是为了锁住那个坑。
fn check_segment_len(seg_len: u64, written: u64) -> Result<(), String> {
    if written != seg_len {
        return Err(format!(
            "分块长度不符：期望 {seg_len} 字节，实际 {written} 字节"
        ));
    }
    Ok(())
}

/// 决定最终落盘的文件名。
///
/// `override_name` 来自界面上的「保存文件名」输入框。用户通常只写标题、不写
/// 扩展名，这时必须补上探测到的扩展名，否则播放器认不出格式；已经写了扩展名
/// 就尊重用户的选择。
fn resolve_target_name(default_name: &str, override_name: Option<&str>) -> String {
    let Some(raw) = override_name.map(str::trim).filter(|s| !s.is_empty()) else {
        return default_name.to_string();
    };

    let name = crate::util::sanitize_filename(raw);
    if has_real_extension(&name) {
        return name;
    }

    // 从默认文件名里取扩展名补上（默认名本身也可能没有扩展名）
    match Path::new(default_name)
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .filter(|e| !e.is_empty())
    {
        Some(ext) => format!("{name}.{ext}"),
        None => name,
    }
}

/// 判断名字末尾是否已经是「像样的」扩展名。
///
/// 不能只看 `Path::extension()` 有没有值：`"v1.2 最终版"` 会被当成扩展名
/// `"2 最终版"`，于是不再补 `.mp4`，落盘文件反而没有可用后缀。
fn has_real_extension(name: &str) -> bool {
    match Path::new(name).extension() {
        Some(e) => {
            let s = e.to_string_lossy();
            !s.is_empty() && s.len() <= 5 && s.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// 最终路径对应的临时文件路径（`movie.mp4` → `movie.mp4.veloxgrab-downloading`）。
fn temp_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut p = target.to_path_buf();
    p.set_file_name(format!("{name}{TEMP_SUFFIX}"));
    p
}

/// 下载完成：把临时文件改名成最终文件名。
///
/// 先写临时文件是必须的：分块下载会 `set_len(total)` 预分配，取消时文件已经是
/// 最终名字 + 完整大小，用户双击得到的是一个黑屏/花屏文件，看不出没下完。
fn finish_download(temp: &Path, target: &Path) -> Result<PathBuf, String> {
    std::fs::rename(temp, target).map_err(|e| format!("重命名临时文件失败: {e}"))?;
    Ok(target.to_path_buf())
}

/// 清理某个直链任务在下载目录里留下的临时产物。
///
/// 只删「以该标题开头 + 以我们的后缀结尾」的文件，不会碰用户的其他文件。
/// 任务被移除后 `.veloxgrab-downloading` / `.veloxgrab-part` 否则会永久留在目录里。
pub fn cleanup_artifacts(dir: &Path, name_hint: &str) {
    let stem = crate::util::sanitize_filename(name_hint);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // 必须紧跟在标题后面的是 '.'：否则标题 "video" 会误删 "video2.mp4.*"
        let Some(rest) = name.strip_prefix(&stem) else {
            continue;
        };
        if !rest.starts_with('.') {
            continue;
        }
        if rest.ends_with(TEMP_SUFFIX) || rest.ends_with(".veloxgrab-part") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 下载直链文件到指定目录，返回最终路径。
#[allow(clippy::too_many_arguments)]
pub async fn download_direct(
    url: &str,
    dir: &Path,
    filename_override: Option<&str>,
    threads: usize,
    cancel: Arc<AtomicBool>,
    on_progress: impl Fn(DirectProgress) + Send + Sync + 'static,
    net: &NetCfg,
) -> Result<PathBuf, String> {
    // 前端解析时刚探过，60 秒内直接复用，省掉一整轮 HEAD + Range 往返
    let info = match cached_probe(url) {
        Some(p) => p,
        None => probe_direct(url, Some(cancel.clone()), net).await?,
    };
    let final_target = dir.join(resolve_target_name(&info.filename, filename_override));
    // 全程只写临时文件，成功后才改名（见 finish_download）
    let target = temp_path_for(&final_target);
    let total = info.total_bytes.unwrap_or(0);
    let threads = threads.clamp(1, MAX_THREADS);
    let use_multi = info.range_supported && total >= MIN_MULTI_THREAD_SIZE && threads > 1;
    let c = client_with(net.proxy.as_deref())?;

    // 不支持分块就只能顺序覆盖写，任何遗留断点都不再有效
    if !use_multi {
        PartMeta::remove(&target);
        match download_single(c, url, &target, total, net.rate_kb, cancel, on_progress).await {
            Ok(_) => return finish_download(&target, &final_target),
            Err(e) => {
                // 单流没有断点续传，失败/取消后留下的半截文件只会占地方
                let _ = std::fs::remove_file(&target);
                return Err(e);
            }
        }
    }

    // 断点续传：元数据与磁盘文件必须互相印证才复用。
    //
    // 只看 url / 总大小是不够的——用户删掉下到一半的文件、或把同名文件换成了别的内容，
    // 各分块仍会按旧偏移从中间开始写，产出「长度正确、中间是空洞」的损坏文件，
    // 而下面的长度校验发现不了（文件本来就被预分配成 total 字节）。
    let mut resume = PartMeta::load(&target);
    if let Some(m) = &resume {
        let meta_ok = m.url == url
            && (total == 0 || m.total == total)
            && m.segments_done.len() == m.segments_total.len()
            && m.segments_done.iter().sum::<u64>() <= total;
        if !meta_ok || !target.is_file() {
            resume = None;
            PartMeta::remove(&target);
        }
    }

    // 新建或复用目标文件，预分配长度避免写入碎片化
    if resume.is_none() {
        let f = std::fs::File::create(&target).map_err(|e| format!("创建文件失败: {e}"))?;
        if total > 0 {
            f.set_len(total)
                .map_err(|e| format!("预分配大小失败: {e}"))?;
        }
    } else if total > 0 {
        // 续传：文件可能比目标长度短（旧版本没预分配），补齐到 total
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .map_err(|e| format!("打开断点文件失败: {e}"))?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < total {
            f.set_len(total)
                .map_err(|e| format!("预分配大小失败: {e}"))?;
        }
    }

    let ranges = split_ranges(total, threads);
    let (done_vec, totals_vec) = match &resume {
        Some(m) if m.segments_total.len() == ranges.len() => {
            (m.segments_done.clone(), m.segments_total.clone())
        }
        _ => (
            vec![0u64; ranges.len()],
            ranges.iter().map(|(s, e)| e - s + 1).collect(),
        ),
    };

    // 续传时进度要从已有字节起算，否则第一个采样点的速度会虚高
    let initial_sum: u64 = done_vec.iter().sum();

    let counters = Arc::new(SegCounters {
        bytes: done_vec.iter().map(|&v| AtomicU64::new(v)).collect(),
        segments_done: done_vec.iter().map(|&v| AtomicU64::new(v)).collect(),
    });

    // 进度上报线程。顺带每秒把各段偏移落盘——进程被强杀时正常返回路径来不及执行，
    // 只有定期快照才能让下次真正续传。
    let stop_flag = Arc::new(AtomicBool::new(false));
    let progress_handle = {
        let counters = counters.clone();
        let stop = stop_flag.clone();
        let part_target = target.clone();
        let url_owned = url.to_string();
        let totals_for_task = totals_vec.clone();
        tauri::async_runtime::spawn(async move {
            // 速度用真正的滑动窗口算：保留最近约 1.2 秒内每次采样的
            // (时刻, 累计字节)，速度 = 窗口首尾差值 / 窗口实际时长。
            // 不能用相邻两次 150ms 采样的差值——分块是攒够 256 KiB 或满
            // 200ms 才落盘，150ms 窗口里一个字节都没新增是常态，那样算出
            // 来的速度会在「1.25 MiB/s」和「0 B/s」之间来回闪。
            // 也不固定每次前移一整秒：各段落盘节奏彼此错开时会刚好撞进
            // 同一个空档。窗口首尾都取实际采样点，任一侧有数据就不会是 0。
            const SPEED_WINDOW: Duration = Duration::from_millis(1200);
            let mut samples: std::collections::VecDeque<(Instant, u64)> =
                std::collections::VecDeque::with_capacity(16);
            samples.push_back((Instant::now(), initial_sum));
            let mut last_save = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_millis(150)).await;
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // 进度与断点都用 bytes：分块每落盘一批就更新它，
                // 比 150ms 轮询能拿到的更细。segments_done 只在分块退出时
                // 用它对齐一次，两者语义一致（都是“已确认落盘”的字节数）。
                let snapshot: Vec<u64> = counters
                    .bytes
                    .iter()
                    .map(|a| a.load(Ordering::Relaxed))
                    .collect();
                let sum: u64 = snapshot.iter().sum();

                // 断点元数据每 500ms 落盘一次：取消/崩溃后能接着下
                if last_save.elapsed() >= Duration::from_millis(500) {
                    last_save = Instant::now();
                    let meta = PartMeta {
                        url: url_owned.clone(),
                        total,
                        segments_total: totals_for_task.clone(),
                        segments_done: snapshot,
                    };
                    meta.save(&part_target);
                }

                let now_t = Instant::now();
                samples.push_back((now_t, sum));
                // 丢掉窗口外的旧样本，但至少留两个点
                while samples.len() > 2 {
                    let expired = match samples.front() {
                        Some((t0, _)) => now_t.duration_since(*t0) > SPEED_WINDOW,
                        None => false,
                    };
                    if expired {
                        samples.pop_front();
                    } else {
                        break;
                    }
                }
                let speed = match (samples.front(), samples.back()) {
                    (Some((t0, b0)), Some((t1, b1))) => {
                        let dt = t1.duration_since(*t0).as_secs_f64();
                        if dt > 0.0 {
                            (b1.saturating_sub(*b0)) as f64 / dt
                        } else {
                            0.0
                        }
                    }
                    _ => 0.0,
                };
                let pct = if total > 0 {
                    (sum as f64 / total as f64 * 100.0).min(100.0)
                } else {
                    0.0
                };
                on_progress(DirectProgress {
                    downloaded: sum,
                    total: Some(total),
                    progress: pct,
                    speed: Some(human_speed(speed)),
                    eta: human_eta(total.saturating_sub(sum), speed),
                });
            }
        })
    };

    let mut handles = Vec::with_capacity(ranges.len());
    for (i, (s, e)) in ranges.iter().enumerate() {
        let start = *s;
        let end = *e;
        let done = counters.bytes[i].load(Ordering::Relaxed);
        let c2 = c.clone();
        let url2 = url.to_string();
        let path2 = target.clone();
        let counters2 = counters.clone();
        let cancel2 = cancel.clone();
        let rate = net.rate_kb;
        handles.push(tauri::async_runtime::spawn(async move {
            download_segment(
                c2, url2, start, end, total, path2, done, counters2, i, rate, cancel2,
            )
            .await
        }));
    }

    let mut results = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(r) => results.push(r),
            Err(e) => results.push(Err(format!("分块任务异常: {e}"))),
        }
    }

    // 用独立信号让进度线程退出，绝不复用调用方的 cancel 标志，
    // 否则正常下载完成会被误判成「已取消」。
    stop_flag.store(true, Ordering::SeqCst);
    let _ = progress_handle.await;

    // 两个来源取 max：`bytes` 在每次落盘时更新（包含「下到一半出错」的段），
    // `segments_done` 只在段正常结束时更新。只读后者会把进度线程 500ms 前
    // 写好的快照覆盖掉，导致断网时整段进度归零、重试白下。
    // 同时按段长截断：meta 被改坏时不能让 already_done >= seg_len 蒙混过关。
    let final_done: Vec<u64> = ranges
        .iter()
        .enumerate()
        .map(|(i, (s, e))| {
            let seg_len = e - s + 1;
            counters.bytes[i]
                .load(Ordering::Relaxed)
                .max(counters.segments_done[i].load(Ordering::Relaxed))
                .min(seg_len)
        })
        .collect();
    let mut meta = PartMeta {
        url: url.to_string(),
        total,
        segments_total: totals_vec.clone(),
        segments_done: final_done,
    };
    for (i, r) in results.iter().enumerate() {
        if let Ok(written) = r {
            // 只能取 max。取消恰好发生在分块循环入口时，written 会小于
            // 进度线程已经记录的值，直接赋值会把续传进度回退。
            if let Some(slot) = meta.segments_done.get_mut(i) {
                *slot = (*slot).max(*written);
            }
        }
    }

    if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
        meta.save(&target);
        return Err(e.clone());
    }
    if cancel.load(Ordering::SeqCst) {
        meta.save(&target);
        return Err("已取消".into());
    }

    // 完成度只能按每段自己写了多少来判断。
    // 文件在开头就被 set_len(total) 预分配过，长度恒等于 total，
    // 用 metadata().len() 做校验等于什么都没校验——少写一段也照样通过。
    let missing: u64 = ranges
        .iter()
        .enumerate()
        .map(|(i, (s, e))| {
            let seg_len = e - s + 1;
            let done = meta.segments_done.get(i).copied().unwrap_or(0);
            seg_len.saturating_sub(done)
        })
        .sum();
    if missing > 0 {
        meta.save(&target);
        return Err(format!("仍有 {missing} 字节未下载完成，已保存断点"));
    }

    PartMeta::remove(&target);
    finish_download(&target, &final_target)
}

/// 不支持 Range 时的单流下载
async fn download_single(
    c: reqwest::Client,
    url: &str,
    target: &Path,
    total: u64,
    rate_kb: u64,
    cancel: Arc<AtomicBool>,
    on_progress: impl Fn(DirectProgress) + Send + Sync + 'static,
) -> Result<PathBuf, String> {
    use tokio::io::AsyncWriteExt;

    let resp = c
        .get(url)
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("服务器返回 HTTP {}", resp.status()));
    }

    let mut file = tokio::fs::File::create(target)
        .await
        .map_err(|e| format!("创建文件失败: {e}"))?;

    let mut downloaded: u64 = 0;
    let mut buf: Vec<u8> = Vec::with_capacity(WRITE_BATCH);
    let mut last_t = Instant::now();
    let mut last_bytes: u64 = 0;
    let mut last_flush = Instant::now();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            let _ = file.flush().await;
            return Err("已取消".into());
        }
        let chunk = chunk.map_err(|e| format!("读取数据流失败: {e}"))?;
        buf.extend_from_slice(&chunk);

        // 与分块路径一致：攒够 256 KiB 或距上次落盘超过 200ms 再写，
        // 否则 8 KiB 一块就是每秒上千次 write_all + spawn_blocking 往返。
        if buf.len() >= WRITE_BATCH || last_flush.elapsed() >= FLUSH_INTERVAL {
            rate_wait(rate_kb, buf.len() as u64).await;
            file.write_all(&buf)
                .await
                .map_err(|e| format!("写入失败: {e}"))?;
            downloaded += buf.len() as u64;
            buf.clear();
            last_flush = Instant::now();

            let now = Instant::now();
            let dt = now.duration_since(last_t).as_secs_f64();
            if dt >= 0.3 {
                let speed = (downloaded.saturating_sub(last_bytes)) as f64 / dt;
                let pct = if total > 0 {
                    (downloaded as f64 / total as f64 * 100.0).min(100.0)
                } else {
                    0.0
                };
                on_progress(DirectProgress {
                    downloaded,
                    total: if total > 0 { Some(total) } else { None },
                    progress: pct,
                    speed: Some(human_speed(speed)),
                    eta: human_eta(total.saturating_sub(downloaded), speed),
                });
                last_t = now;
                last_bytes = downloaded;
            }
        }
    }

    if !buf.is_empty() {
        rate_wait(rate_kb, buf.len() as u64).await;
        file.write_all(&buf)
            .await
            .map_err(|e| format!("写入失败: {e}"))?;
        downloaded += buf.len() as u64;
    }

    file.flush().await.map_err(|e| format!("刷新失败: {e}"))?;

    // 流「正常结束」不等于内容完整：chunked 编码下对端干净地提前关闭不会报错。
    if total > 0 && downloaded != total {
        return Err(format!(
            "大小校验失败：期望 {total} 字节，实际 {downloaded} 字节"
        ));
    }

    on_progress(DirectProgress {
        downloaded,
        total: if total > 0 { Some(total) } else { None },
        progress: 100.0,
        speed: None,
        // 已经下完了，剩余时间没有意义
        eta: None,
    });
    Ok(target.to_path_buf())
}

/// 从 URL 里取出 host（不含 userinfo 与端口），失败返回 None。
///
/// 不引入 `url` crate：这里只关心 host，且输入已经过 http/https 前缀校验。
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    // IPv6 字面量（[::1]:8080）不会出现在媒体站名单里，按端口截断即可
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// 判断 URL 是否属于已知的「需要解析器提取流地址」的媒体站点。
///
/// 必须按 host 做**后缀**匹配，不能对整条 URL 做 `contains`：
/// 早期实现用 `lower.contains("x.com")`，结果 `dropbox.com` / `box.com` 全被
/// 当成 X（Twitter）送进 yt-dlp，必然失败；而 `probe_auto` 在媒体站分支失败后
/// 不会兜底走直链，这类正常直链就完全下不了。
pub fn looks_like_media_site(url: &str) -> bool {
    const MEDIA_HOSTS: &[&str] = &[
        "youtube.com",
        "youtu.be",
        "bilibili.com",
        "b23.tv",
        "douyin.com",
        "iesdouyin.com",
        "xiaohongshu.com",
        "weibo.com",
        "weibo.cn",
        "twitter.com",
        "x.com",
        "tiktok.com",
        "reddit.com",
        "vimeo.com",
        "twitch.tv",
        "facebook.com",
        "instagram.com",
        "dailymotion.com",
        "nicovideo.jp",
        "ixigua.com",
        "kuaishou.com",
        "acfun.cn",
        "v.qq.com",
        "youku.com",
        "iqiyi.com",
        "mgtv.com",
    ];
    let Some(host) = host_of(url) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    MEDIA_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分段必须无缝无重叠地覆盖整个文件，否则完成度校验和写入偏移都会错
    #[test]
    fn split_ranges_covers_file_exactly() {
        for total in [4 * 1024 * 1024u64, 10 * 1024 * 1024, 100 * 1024 * 1024 + 7] {
            for threads in [2usize, 4, 8, 16] {
                let ranges = split_ranges(total, threads);
                assert_eq!(ranges[0].0, 0, "必须从 0 开始");
                assert_eq!(ranges.last().unwrap().1, total - 1, "必须覆盖到末尾");
                for w in ranges.windows(2) {
                    assert_eq!(w[0].1 + 1, w[1].0, "段之间不能有缝隙或重叠");
                }
                let sum: u64 = ranges.iter().map(|(s, e)| e - s + 1).sum();
                assert_eq!(sum, total, "各段长度之和必须等于文件大小");
            }
        }
    }

    /// 线程数不能把每段切得比 MIN_CHUNK_SIZE 还小
    #[test]
    fn split_ranges_respects_min_chunk_size() {
        let ranges = split_ranges(4 * 1024 * 1024, 16);
        assert_eq!(ranges.len(), 4);
        for (s, e) in ranges {
            assert!(e - s + 1 >= MIN_CHUNK_SIZE);
        }
    }

    /// 回归：媒体站判定必须按 host 后缀匹配
    #[test]
    fn media_site_matching_is_host_based() {
        // 曾经因为 contains("x.com") 被误判成 X，进而送进 yt-dlp 必然失败
        assert!(!looks_like_media_site("https://www.dropbox.com/s/abc/file.zip"));
        assert!(!looks_like_media_site("https://box.com/file.zip"));
        // 真正的媒体站仍然要认出来
        assert!(looks_like_media_site("https://x.com/user/status/1"));
        assert!(looks_like_media_site("https://www.youtube.com/watch?v=abc"));
        assert!(looks_like_media_site("https://youtu.be/abc"));
        // 后缀必须落在 label 边界上，不能是「以某媒体站域名结尾的别家域名」
        assert!(!looks_like_media_site("https://notyoutube.com/watch"));
        assert!(!looks_like_media_site("https://youtube.com.evil.cn/watch"));
        // 大小写与端口
        assert!(looks_like_media_site("https://WWW.YouTube.COM:443/watch"));
        // 非 URL 输入不能 panic
        assert!(!looks_like_media_site("not a url"));
    }

    /// 回归：界面上的「保存文件名」必须真正生效
    #[test]
    fn target_name_honors_override() {
        // 没有覆盖名时用服务器给的名字
        assert_eq!(resolve_target_name("movie.mp4", None), "movie.mp4");
        assert_eq!(resolve_target_name("movie.mp4", Some("  ")), "movie.mp4");

        // 用户只写标题：补上探测到的扩展名
        assert_eq!(resolve_target_name("movie.mp4", Some("我的视频")), "我的视频.mp4");
        // 用户写了扩展名：尊重用户的选择
        assert_eq!(resolve_target_name("movie.mp4", Some("我的视频.mkv")), "我的视频.mkv");
        // 标题里的点不能被误当成扩展名
        assert_eq!(
            resolve_target_name("movie.mp4", Some("v1.2 最终版")),
            "v1.2 最终版.mp4"
        );
        // 非法字符仍然要清洗
        assert_eq!(
            resolve_target_name("movie.mp4", Some("a/b:c")),
            "a_b_c.mp4"
        );
        // 服务器也没给扩展名时不硬造一个
        assert_eq!(resolve_target_name("download", Some("标题")), "标题");
    }

    /// 回归：下载全程只写临时文件，成功后才改名。
    /// 否则取消时会留下一个「名字和大小都正常」的残缺文件。
    #[test]
    fn downloads_go_through_temp_file() {
        let dir = std::env::temp_dir().join(format!("veloxgrab_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("movie.mp4");
        let temp = temp_path_for(&target);
        assert_eq!(
            temp.file_name().unwrap().to_string_lossy(),
            "movie.mp4.veloxgrab-downloading"
        );

        std::fs::write(&temp, b"hello").unwrap();
        let out = finish_download(&temp, &target).unwrap();

        assert_eq!(out, target);
        assert!(!temp.exists(), "临时文件应该已经被改名");
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn segment_len_check_treats_written_as_cumulative() {
        // 全新下载：written 从 0 涨到 seg_len，通过。
        assert!(check_segment_len(2 * 1024 * 1024, 2 * 1024 * 1024).is_ok());
        // 少写一段：不通过，错误信息要给出整段累计值。
        let e = check_segment_len(2 * 1024 * 1024, 1024 * 1024).unwrap_err();
        assert!(e.contains("期望 2097152 字节"), "{e}");
        assert!(e.contains("实际 1048576 字节"), "{e}");
        // 写超了同样不行。
        assert!(check_segment_len(1024, 2048).is_err());

        // 续传场景（就是当初栽坑的地方）：这一段 2 MiB，已有 1 MiB，
        // 本次又下完剩下的 1 MiB。written 的累计值是 2 MiB —— 必须通过。
        // 旧写法拿 `written - already_done`（= 1 MiB）跟 seg_len 比，必然失败。
        let seg_len = 2 * 1024 * 1024u64;
        let already_done = 1024 * 1024u64;
        let written = already_done + (seg_len - already_done);
        assert_eq!(written, seg_len);
        assert!(check_segment_len(seg_len, written).is_ok());
    }

    #[test]
    fn split_ranges_makes_each_chunk_cover_its_slice() {
        // 分块切分必须无缝覆盖整个文件，否则会漏数据。
        let total = 10 * 1024 * 1024u64;
        let ranges = split_ranges(total, 4);
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges.last().unwrap().1, total - 1);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0, "段与段之间不能有缝隙或重叠");
        }
    }
}
