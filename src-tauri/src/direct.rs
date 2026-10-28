//! 通用多线程直链下载器（HTTP Range 分块下载）
//!
//! 设计要点：
//! - 先发 HEAD（失败则退化为带 Range 的 GET）探测：是否支持 Range、总大小、文件名、MIME
//! - 支持 Range 时把文件切成 N 段，每段一个连接并发拉取，各自 seek 写入同一个文件
//! - 不支持 Range 时退化为单流顺序下载
//! - 断点续传：每段已写偏移定期落盘到 `<临时文件>.veloxgrab-part`，重启后按偏移继续
//! - 临时文件带任务 id：同名任务并发时互不覆盖，清理也只删自己那一份
//! - 取消通过共享 AtomicBool 实现，不依赖 reqwest 的取消机制

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::settings::NetCfg;
use crate::util::host_of;

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
        // 跳转不能只数次数：入口的 scheme 校验管的是「用户给的 URL」，管不到
        // 「302 把我们带去哪」。恶意服务器可以一步跳进 169.254.169.254（云元数据），
        // 或把 https 带签名的下载链降级到 http 明文。判据抽成 redirect_is_allowed
        // 纯函数（Policy 闭包本身没法单测），这里只接线。
        // 边界要诚实：只拦 URL 字面量级别的违规。域名解析到内网（true SSRF）不防——
        // 每次跳转前做 DNS 预检既有 TOCTOU，又误伤「下载 NAS 上的文件」的正当用途：
        // 局域网/回环是刻意放行的，桌面下载器对用户的内网没有默认敌意。
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("重定向次数过多");
            }
            // 先算判定再消费 attempt：previous()/url() 借用了 attempt，
            // follow()/error() 要移动它，放在 match 分支里会撞上借用检查。
            let ok = match attempt.previous().last() {
                Some(from) => {
                    redirect_is_allowed(from.as_str(), attempt.url().as_str()).is_ok()
                }
                None => true,
            };
            if ok {
                attempt.follow()
            } else {
                attempt.error("重定向目标被安全策略拒绝")
            }
        }));
    if let Some(p) = proxy {
        b = b.proxy(
            reqwest::Proxy::all(p).map_err(|e| format!("代理配置无效: {e}"))?,
        );
    }
    b.build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))
}

/// 给请求附加按 host 作用域限定的认证头（目前只有 HuggingFace）。
///
/// `NetCfg.auth` 可能出现在任何直链任务里，但只有 URL 的 host 命中作用域时
/// 才真正带上——这是「token 跟着设置走」与「token 跟着请求走」的分界线。
fn apply_auth(rb: reqwest::RequestBuilder, url: &str, net: &NetCfg) -> reqwest::RequestBuilder {
    match net.auth.as_ref().and_then(|a| a.for_url(url)) {
        Some(v) => rb.header(reqwest::header::AUTHORIZATION, v),
        None => rb,
    }
}

/// 一个 URL 作为下载目标是否放行（初始 URL 与每一跳共用）。
///
/// 纯函数：reqwest 的 Policy 闭包没法单测，判据拆开测。
/// 只按 **字面量** 判：scheme 必须 http/https，host 若写成 IP 则不得是
/// 链路本地/云元数据段（169.254.0.0/16、fe80::/10）或未指定地址（0.0.0.0、::）。
/// 边界要诚实：域名解析到内网（true SSRF）不防——每次请求前做 DNS 预检既有
/// TOCTOU，又误伤「下载 NAS 上的文件」这一正当用途：局域网/回环是刻意放行
/// 的，桌面下载器对用户的内网没有默认敌意。
pub(crate) fn url_target_guarded(u: &str) -> Result<(), String> {
    let parsed = match reqwest::Url::parse(u) {
        Ok(x) => x,
        Err(_) => return Err("下载地址不是合法 URL".into()),
    };
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("下载地址协议不支持: {scheme}"));
    }
    if let Some(host) = parsed.host_str() {
        // url crate 对 IPv6 字面量返回**带方括号**的形式（`[fe80::1]`），
        // 而 Ipv6Addr::from_str 不认括号——不剥就等于 IPv6 一律 parse 失败、
        // 原样放行，这条闸就白装了。（回归测试抓到的就是这个。）
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
            if ip.is_link_local() || ip.is_unspecified() {
                return Err("下载地址指向链路本地/保留地址".into());
            }
        } else if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
            // fe80::/10：顶部 10 位固定为 1111111010
            let first = (u128::from(ip) >> 112) as u16;
            if ip.is_unspecified() || first & 0xffc0 == 0xfe80 {
                return Err("下载地址指向链路本地/保留地址".into());
            }
        }
    }
    Ok(())
}

/// 从 `from` 跳到 `to` 是否放行：目标本身要过关，且不许 https 降级 http。
fn redirect_is_allowed(from: &str, to: &str) -> Result<(), String> {
    url_target_guarded(to)?;
    // https → http 降级：签名直链、token、Cookie 都可能随之掉进明文信道。
    // （http → https 升级无害，放行。from 解析不了就不比——目标自己的闸已经过了。）
    if let Ok(from_url) = reqwest::Url::parse(from) {
        if from_url.scheme() == "https"
            && reqwest::Url::parse(to).map(|t| t.scheme() == "http").unwrap_or(false)
        {
            return Err("重定向把 HTTPS 降级成了 HTTP".into());
        }
    }
    Ok(())
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

fn cached_probe(key: &str) -> Option<DirectProbe> {
    let g = LAST_PROBE.lock().ok()?;
    let (cached_key, at, probe) = g.as_ref()?;
    if cached_key == key && at.elapsed() < PROBE_CACHE_TTL {
        Some(probe.clone())
    } else {
        None
    }
}

fn store_probe(key: &str, probe: &DirectProbe) {
    if let Ok(mut g) = LAST_PROBE.lock() {
        *g = Some((key.to_string(), Instant::now(), probe.clone()));
    }
}

/// 探测缓存键。
///
/// 同一个 URL「带 token」与「不带 token」的结果完全不同（gated 仓库是 401 vs 200），
/// 只按 URL 缓存会把一次 401 复用给刚填好 token 的下载，或反过来。
fn probe_cache_key(url: &str, net: &NetCfg) -> String {
    format!("{url}\u{1}{}", net.auth.is_some())
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
    // 初始 URL 与每一跳过同一道闸（scheme + 字面量保留地址）。commands.rs 的
    // http(s) 前缀检查只看字符串开头，`https://[fe80::1]/`、`http://0.0.0.0/`
    // 这类"前缀合法、host 违规"的写法从这里漏过去；跳转后再拦也晚了——
    // 第一跳就是元数据地址时请求已经发出去了。
    url_target_guarded(url)?;
    let fut = tokio::time::timeout(PROBE_TIMEOUT, probe_uncached(url, net));
    let timed_out = || format!("探测超时（{} 秒），服务器响应过慢", PROBE_TIMEOUT.as_secs());

    let probe = match cancel {
        Some(flag) => tokio::select! {
            r = fut => r.map_err(|_| timed_out())??,
            _ = wait_cancel(flag) => return Err("已取消".into()),
        },
        None => fut.await.map_err(|_| timed_out())??,
    };

    store_probe(&probe_cache_key(url, net), &probe);
    Ok(probe)
}

/// 实际的探测逻辑（无超时、无缓存），由 `probe_direct` 包装后调用。
async fn probe_uncached(url: &str, net: &NetCfg) -> Result<DirectProbe, String> {
    let c = client_with(net.proxy.as_deref())?;

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
            apply_auth(c.head(url), url, net).send(),
        )
        .await
        {
            Ok(Ok(r)) if r.status().is_success() => Some(r),
            _ => None,
        }
    };
    let range_fut = async {
        apply_auth(c.get(url), url, net)
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

    /// 断点文件的可读上限（字节）。
    ///
    /// `.veloxgrab-part` 与目标文件同目录，而目录分量在威胁模型里可被外部
    /// 写入（旧版本目录闸门缺失时已落盘的目录，以及 history 回灌路径）——
    /// 一个 4 GB 的「断点文件」不报错、只是被 read_to_string 整个吸进内存。
    /// 正常元数据 < 4 KiB（64 段 × 2 个 u64 数组 + url），256 KiB 已是十倍余量；
    /// 超限按「不是断点、是垃圾」处理：返回 None → 调用方从头下。
    const MAX_FILE_BYTES: u64 = 256 * 1024;

    fn load(target: &Path) -> Option<PartMeta> {
        let path = Self::path_for(target);
        if std::fs::metadata(&path).map(|m| m.len()).ok()? > Self::MAX_FILE_BYTES {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
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
    auth: Option<String>,
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

    let mut rb = c
        .get(&url)
        .header(reqwest::header::RANGE, format!("bytes={from}-{end}"));
    if let Some(v) = auth {
        rb = rb.header(reqwest::header::AUTHORIZATION, v);
    }
    let resp = rb
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

        // 本段的上限是**声明过的** seg_len，一字节都不许多收：恶意/故障服务器
        // 可以在 Content-Range 里谎报 10 MB、实际流 500 GB——不设闸的话循环会
        // 一路写穿磁盘，直到文件系统报错才停（盘满期间还会连累别的任务与系统）。
        // 超额时把「写到 seg_len 为止」的合法部分收进缓冲、落盘后按对端违规报错：
        // 数据不多不少，续传状态也自洽（断点记到 seg_len），绝不静默截断成
        // 「看起来成功」。
        if (written + buf.len() as u64 + chunk.len() as u64) > seg_len {
            let room = seg_len.saturating_sub(written + buf.len() as u64);
            buf.extend_from_slice(&chunk[..room as usize]);
            if !buf.is_empty() {
                file.write_all(&buf)
                    .await
                    .map_err(|e| format!("写入失败: {e}"))?;
                written += buf.len() as u64;
                buf.clear();
                counters.bytes[seg_index].store(written, Ordering::Relaxed);
            }
            let _ = file.flush().await;
            return Err(format!(
                "服务器发送的数据超出分块声明的范围（>{seg_len} 字节），已中断"
            ));
        }
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

/// 断点元数据与磁盘现状是否自洽。自洽才允许续传，否则整份作废从头下。
///
/// 纯函数：伪造的 `.veloxgrab-part` 是威胁模型里的输入，这条判据必须能单测。
/// `file_len` 是临时文件的实际长度；`ranges` 是**本轮**的分段方案——分段按
/// 线程数切，线程数一改边界就漂移，旧 done 套新 ranges 等于按错误偏移续传，
/// 所以逐段比对 `segments_total` 与本轮段长，比「两数组长度相等」更严。
///
/// 盘上校验的判据是「每段的写入区间 [start, start+done) 必须完整落在 EOF 内」：
/// 文件截短只会砍掉尾部——后段谎报的字节其实已经没了，而只查
/// `done <= file_len` 会漏掉这种（第 4 段的 done 远小于文件偏移照样"过"）。
/// 各段区间互不重叠地落在 [0, file_len) 内，天然蕴含 done 总和 <= file_len。
fn resume_is_consistent(
    m: &PartMeta,
    url: &str,
    total: u64,
    ranges: &[(u64, u64)],
    file_len: u64,
) -> bool {
    m.url == url
        && (total == 0 || m.total == total)
        && m.segments_done.len() == ranges.len()
        && m.segments_total.len() == ranges.len()
        // 每条声明段长必须等于本轮实际切出来的段长（分段方案没变）
        && m.segments_total
            .iter()
            .zip(ranges)
            .all(|(t, (s, e))| *t == e - s + 1)
        // 任何一段都不能声称写得比段长还多；写入区间不得越过 EOF
        && ranges
            .iter()
            .zip(&m.segments_done)
            .all(|((s, e), d)| *d <= e - s + 1 && s.saturating_add(*d) <= file_len)
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

/// 最终路径对应的临时文件路径。
///
/// `tag` 是任务 id（`t<毫秒>_<序号>`）。带上它之后，同一个标题的两个任务
/// 各自写各自的临时文件——旧实现是 `<文件名>.veloxgrab-downloading`，两个
/// 同名并发任务会互相覆盖对方的分块，最终各自 rename 出「看起来完整」的
/// 损坏文件。`tag` 为空时退回旧命名（只给测试和迁移路径用）。
fn temp_path_for(target: &Path, tag: &str) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let suffix = if tag.is_empty() {
        TEMP_SUFFIX.to_string()
    } else {
        format!("{TEMP_SUFFIX}.{tag}")
    };
    let mut p = target.to_path_buf();
    p.set_file_name(format!("{name}{suffix}"));
    p
}

/// 这个文件名是不是直链下载器留下的临时产物（新命名或升级前的旧命名）。
///
/// 只按后缀模糊判断会把 `notes.veloxgrab-downloading.bak` 也算进来，启动清理
/// 就会删用户的普通文件。新命名里的 tag 必须是任务 id 的形状：`t` + 数字/下划线。
/// （任务 id 由 `commands::next_task_id` 生成，形如 `t1759..._0`。）
pub fn is_temp_artifact(name: &str) -> bool {
    fn part_tail(rest: &str) -> bool {
        rest.is_empty() || rest == ".veloxgrab-part" || rest == ".veloxgrab-part.tmp"
    }
    fn task_tag(tag: &str) -> bool {
        let mut cs = tag.chars();
        cs.next() == Some('t')
            && tag.len() > 1
            && cs.all(|c| c.is_ascii_digit() || c == '_')
    }

    let Some(pos) = name.find(TEMP_SUFFIX) else {
        return false;
    };
    let rest = &name[pos + TEMP_SUFFIX.len()..];
    // 旧命名：<name>.veloxgrab-downloading[.veloxgrab-part[.tmp]]
    if part_tail(rest) {
        return true;
    }
    // 新命名：<name>.veloxgrab-downloading.<task_id>[.veloxgrab-part[.tmp]]
    let Some(rest) = rest.strip_prefix('.') else {
        return false;
    };
    match rest.split_once('.') {
        Some((tag, tail)) => task_tag(tag) && matches!(tail, "veloxgrab-part" | "veloxgrab-part.tmp"),
        None => task_tag(rest),
    }
}

/// 把升级前遗留的「按标题命名」临时文件认领给当前任务。
///
/// 升级那一刻盘上可能还躺着旧版本的 `movie.mp4.veloxgrab-downloading` 与断点。
/// 不迁移的话，用户点「继续」会从头再下，之前下的几 GB 白费。迁移只在
/// **本任务的新临时文件不存在**时发生，且旧名字在升级后不会再有进程写它，
/// 所以不存在把别人的活跃数据抢走的问题。
fn adopt_legacy_temp(final_target: &Path, target: &Path, tag: &str) {
    if tag.is_empty() || target.exists() {
        return;
    }
    let legacy = temp_path_for(final_target, "");
    if !legacy.is_file() {
        return;
    }
    if std::fs::rename(&legacy, target).is_err() {
        return;
    }
    let old_part = PartMeta::path_for(&legacy);
    let new_part = PartMeta::path_for(target);
    if !new_part.exists() {
        let _ = std::fs::rename(old_part, new_part);
    }
}

/// 下载完成：把临时文件改名成最终文件名。
///
/// 先写临时文件是必须的：分块下载会 `set_len(total)` 预分配，取消时文件已经是
/// 最终名字 + 完整大小，用户双击得到的是一个黑屏/花屏文件，看不出没下完。
///
/// 改名必须走「不覆盖」语义：Windows 上 `std::fs::rename` 底层是
/// `MOVEFILE_REPLACE_EXISTING`，目标已存在时**静默覆盖**。正常用户也会踩
/// （同一目录里已有同名文件、或同一链接下第二次），恶意深链 `dir=` + 可预测
/// 文件名时它就是定向摧毁用户成品的原语。这里改成浏览器「另存为」的惯例：
/// 占用则递增 `name (1).ext`。
///
/// 已知残余窗口：exists 与 rename 之间别的进程刚好创建同名文件，Windows 上
/// 仍会被替换。这个 TOCTOU 子窗口极小且需要主动竞争，相比改前「永远覆盖」
/// 已经是质的收敛，暂不为此引入打开-重命名协议。
fn finish_download(temp: &Path, target: &Path) -> Result<PathBuf, String> {
    let final_target = unique_target(target);
    std::fs::rename(temp, &final_target).map_err(|e| format!("重命名临时文件失败: {e}"))?;
    Ok(final_target)
}

/// 同名冲突时给出下一个空闲名：`movie.mp4` → `movie (1).mp4` → `movie (2).mp4` …
///
/// `Path::exists` 在 NTFS 上按 Windows 语义大小写不敏感，`movie.MP4` 也算占用。
/// 递增到 9999 仍全被占用属于病态目录，返回兜底的时间戳名而不是覆盖——
/// 数据永远优先于「名字好看」。
fn unique_target(target: &Path) -> PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());
    let ext = target
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..=9999u32 {
        let cand = target.with_file_name(format!("{stem} ({n}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    // 兜底：带时间戳的名字，基本不可能再撞
    let stamp = crate::commands::now_ms();
    target.with_file_name(format!("{stem} (dl-{stamp}){ext}"))
}

/// 清理某个直链任务在下载目录里留下的临时产物。
///
/// 判据是「以该标题开头 + 带本任务的 id 标记 + 以我们的后缀结尾」：
/// 只删这个任务自己的 `.veloxgrab-downloading.<id>` 与配套断点，绝不会碰到
/// 同名任务的临时文件（这正是旧实现按标题前缀匹配的缺陷）。
///
/// 升级前的旧命名（不带 id）不在这里删：它被同标题的多个历史任务共享，
/// 删掉任何一个任务都可能踩坏另一个任务的续传数据；交给启动时的
/// `cleanup_stale_artifacts` 按时间窗口统一回收。
pub fn cleanup_artifacts(dir: &Path, name_hint: &str, task_id: &str) {
    let stem = crate::util::sanitize_filename(name_hint);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let marker = format!("{TEMP_SUFFIX}.{task_id}");
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
        // marker 之后只剩空串（临时文件本体）或断点后缀。
        // 用 rfind：标题本身若恰好含 marker（例如用户真把文件名起成
        // `x.veloxgrab-downloading.t1_0`），只能认最后一个，否则清理不到自己的文件。
        let Some(pos) = name.rfind(&marker) else {
            continue;
        };
        let tail = &name[pos + marker.len()..];
        if tail.is_empty() || tail == ".veloxgrab-part" || tail == ".veloxgrab-part.tmp" {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 下载直链文件到指定目录，返回最终路径。
///
/// `task_id` 只用于临时文件命名：同标题的两个并发任务各写各的临时文件，
/// 完成后再各自落到 `movie.mp4` / `movie (1).mp4`，不会互相覆盖。
#[allow(clippy::too_many_arguments)]
pub async fn download_direct(
    url: &str,
    task_id: &str,
    dir: &Path,
    filename_override: Option<&str>,
    threads: usize,
    cancel: Arc<AtomicBool>,
    on_progress: impl Fn(DirectProgress) + Send + Sync + 'static,
    net: &NetCfg,
) -> Result<PathBuf, String> {
    // 前端解析时刚探过，60 秒内直接复用，省掉一整轮 HEAD + Range 往返
    let info = match cached_probe(&probe_cache_key(url, net)) {
        Some(p) => p,
        None => probe_direct(url, Some(cancel.clone()), net).await?,
    };
    let final_target = dir.join(resolve_target_name(&info.filename, filename_override));
    // 全程只写临时文件，成功后才改名（见 finish_download）
    let target = temp_path_for(&final_target, task_id);
    let legacy_target = temp_path_for(&final_target, "");
    let total = info.total_bytes.unwrap_or(0);
    let threads = threads.clamp(1, MAX_THREADS);
    let use_multi = info.range_supported && total >= MIN_MULTI_THREAD_SIZE && threads > 1;
    let c = client_with(net.proxy.as_deref())?;

    // 不支持分块就只能顺序覆盖写，任何遗留断点都不再有效
    if !use_multi {
        PartMeta::remove(&target);
        // 升级前的旧命名也一并清掉：单流没有续传，留着只会占地方
        let _ = std::fs::remove_file(&legacy_target);
        PartMeta::remove(&legacy_target);
        match download_single(c, url, &target, total, net, cancel, on_progress).await {
            Ok(_) => return finish_download(&target, &final_target),
            Err(e) => {
                // 单流没有断点续传，失败/取消后留下的半截文件只会占地方
                let _ = std::fs::remove_file(&target);
                return Err(e);
            }
        }
    }

    // 升级后的第一次续传：把旧版本的「按标题命名」临时文件认领过来
    adopt_legacy_temp(&final_target, &target, task_id);

    let ranges = split_ranges(total, threads);

    // 断点续传：元数据与磁盘文件必须互相印证才复用。
    //
    // 只看 url / 总大小是不够的。三种盘上损坏都要能拦住（threat：history.json
    // 与 .veloxgrab-part 都在「不可信输入」清单里）：
    //  - 文件被删/被截短——旧偏移跳过已写段，set_len 把短文件补回 total，
    //    产出「长度正确、中间是空洞」的损坏文件，末尾校验发现不了；
    //  - 某段进度谎报超过段长本身——该段被整体跳过却计入完成度；
    //  - 分块数变了导致分段边界漂移——旧 done 套在新 ranges 上。
    // 「同长度换内容」仍然拦不住（要靠哈希重下），注释不再声称能拦。
    let mut resume = PartMeta::load(&target);
    if let Some(m) = &resume {
        let file_len = std::fs::metadata(&target).map(|md| md.len()).unwrap_or(0);
        if !target.is_file() || !resume_is_consistent(m, url, total, &ranges, file_len) {
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
    // 每个分块都要带认证头，但 host 判定只做一次：所有段共用同一个 URL
    let auth = net
        .auth
        .as_ref()
        .and_then(|a| a.for_url(url))
        .map(str::to_string);
    for (i, (s, e)) in ranges.iter().enumerate() {
        let start = *s;
        let end = *e;
        let done = counters.bytes[i].load(Ordering::Relaxed);
        let c2 = c.clone();
        let url2 = url.to_string();
        let auth2 = auth.clone();
        let path2 = target.clone();
        let counters2 = counters.clone();
        let cancel2 = cancel.clone();
        let rate = net.rate_kb;
        handles.push(tauri::async_runtime::spawn(async move {
            download_segment(
                c2, url2, auth2, start, end, total, path2, done, counters2, i, rate, cancel2,
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
            //
            // 取完 max 必须再夹一次段长：download_segment 的早退分支
            // `already_done >= seg_len → Ok(already_done)` 原样回吐了（可能被
            // 篡改过的）断点值，上面 final_done 的 min(seg_len) 会被这里重新
            // 撑破——伪造 meta 把某段报成 >seg_len，本段被整体跳过、完成度却
            // 已封顶，缺字节校验就形同虚设。夹逼收口在这最后一道写入处。
            if let Some(slot) = meta.segments_done.get_mut(i) {
                let seg_len = meta.segments_total.get(i).copied().unwrap_or(*slot);
                *slot = (*slot).max(*written).min(seg_len);
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
#[allow(clippy::too_many_arguments)]
async fn download_single(
    c: reqwest::Client,
    url: &str,
    target: &Path,
    total: u64,
    net: &NetCfg,
    cancel: Arc<AtomicBool>,
    on_progress: impl Fn(DirectProgress) + Send + Sync + 'static,
) -> Result<PathBuf, String> {
    use tokio::io::AsyncWriteExt;
    let rate_kb = net.rate_kb;

    let resp = apply_auth(c.get(url), url, net)
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

        // 与分块路径同一道闸：服务器声明了总长（Content-Length）却不守它，
        // 多发的一律算对端违规，立即中断——不给谎报大小的远端写穿磁盘的机会。
        // total == 0（无声明长度，如 chunked 且无头）时无从夹逼，原样接受。
        if total > 0 && (downloaded + buf.len() as u64 + chunk.len() as u64) > total {
            let room = total.saturating_sub(downloaded + buf.len() as u64);
            buf.extend_from_slice(&chunk[..room as usize]);
            if !buf.is_empty() {
                // 这批是收尾写入，超额分支随后就 return Err，调用方不再读
                // downloaded，这里不做累加（免得留「赋值未使用」的死存储）。
                // 不抢限速闸门：马上要报错退出，扣这一批令牌没有意义。
                file.write_all(&buf)
                    .await
                    .map_err(|e| format!("写入失败: {e}"))?;
                buf.clear();
            }
            let _ = file.flush().await;
            return Err(format!(
                "服务器发送的数据超出声明的文件大小（>{total} 字节），已中断"
            ));
        }
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
        let temp = temp_path_for(&target, "t1_0");
        assert_eq!(
            temp.file_name().unwrap().to_string_lossy(),
            "movie.mp4.veloxgrab-downloading.t1_0"
        );

        std::fs::write(&temp, b"hello").unwrap();
        let out = finish_download(&temp, &target).unwrap();

        assert_eq!(out, target);
        assert!(!temp.exists(), "临时文件应该已经被改名");
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归（安全）：目标同名文件已存在时，改名必须递增避让，**绝不覆盖**。
    /// 旧实现用 std::fs::rename，Windows 语义是静默替换——用户重复下同一链接
    /// 会毁掉自己已有的成品；恶意深链配可预测文件名时是定向摧毁原语。
    #[test]
    fn finish_download_never_clobbers_existing_file() {
        let dir = std::env::temp_dir().join(format!("veloxgrab_clobber_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("movie.mp4");
        std::fs::write(&target, b"IMPORTANT-USER-DATA").unwrap();

        let temp = temp_path_for(&target, "t1_0");
        std::fs::write(&temp, b"new-download").unwrap();
        let out = finish_download(&temp, &target).unwrap();

        // 用户原有文件一字未动
        assert_eq!(out, dir.join("movie (1).mp4"));
        assert_eq!(std::fs::read(&target).unwrap(), b"IMPORTANT-USER-DATA");
        assert_eq!(std::fs::read(&out).unwrap(), b"new-download");
        assert!(!temp.exists(), "临时文件应该已经被改名");

        // 再撞一次：继续递增到 (2)
        let temp2 = temp_path_for(&target, "t1_0");
        std::fs::write(&temp2, b"third").unwrap();
        let out2 = finish_download(&temp2, &target).unwrap();
        assert_eq!(out2, dir.join("movie (2).mp4"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归（P1-2）：两个同名并发任务必须各写各的临时文件。
    ///
    /// 旧实现按标题命名，B 任务会打开 A 任务正在写的同一份文件并覆盖分块。
    #[test]
    fn temp_path_is_scoped_to_task_id() {
        let dir = std::env::temp_dir().join(format!("veloxgrab_scope_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("movie.mp4");
        let a = temp_path_for(&target, "t100_0");
        let b = temp_path_for(&target, "t100_1");
        assert_ne!(a, b, "同标题不同任务必须落在不同临时文件");

        std::fs::write(&a, b"AAA").unwrap();
        std::fs::write(&b, b"BBB").unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"AAA");
        assert_eq!(std::fs::read(&b).unwrap(), b"BBB");

        // 清理 A 不能碰 B
        cleanup_artifacts(&dir, "movie", "t100_0");
        assert!(!a.exists(), "自己的临时文件应被清理");
        assert!(b.exists(), "另一个任务的临时文件不能被误删");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清理只认自己的任务标记，不碰用户文件、不碰其它任务。
    #[test]
    fn cleanup_artifacts_is_exact_to_task_marker() {
        let dir = std::env::temp_dir().join(format!("veloxgrab_clean_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mine_tmp = dir.join("movie.mp4.veloxgrab-downloading.t9_0");
        let mine_part = dir.join("movie.mp4.veloxgrab-downloading.t9_0.veloxgrab-part");
        let mine_ptmp = dir.join("movie.mp4.veloxgrab-downloading.t9_0.veloxgrab-part.tmp");
        let other_tmp = dir.join("movie.mp4.veloxgrab-downloading.t9_1");
        let legacy_tmp = dir.join("movie.mp4.veloxgrab-downloading");
        let user_file = dir.join("movie.mp4");
        let lookalike = dir.join("movie.mp4.veloxgrab-downloading.bak");
        for p in [
            &mine_tmp,
            &mine_part,
            &mine_ptmp,
            &other_tmp,
            &legacy_tmp,
            &user_file,
            &lookalike,
        ] {
            std::fs::write(p, b"x").unwrap();
        }

        cleanup_artifacts(&dir, "movie", "t9_0");

        assert!(!mine_tmp.exists());
        assert!(!mine_part.exists());
        assert!(!mine_ptmp.exists());
        assert!(other_tmp.exists(), "另一个任务的临时文件必须保留");
        assert!(legacy_tmp.exists(), "旧命名被同标题任务共享，交给启动清理");
        assert!(user_file.exists());
        assert!(lookalike.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 升级后第一次续传：旧命名的临时文件与断点要被认领过来。
    #[test]
    fn legacy_temp_and_part_are_adopted() {
        let dir = std::env::temp_dir().join(format!("veloxgrab_adopt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let final_target = dir.join("movie.mp4");
        let legacy = temp_path_for(&final_target, "");
        std::fs::write(&legacy, b"old-progress").unwrap();
        std::fs::write(PartMeta::path_for(&legacy), b"{}").unwrap();

        let target = temp_path_for(&final_target, "t7_0");
        adopt_legacy_temp(&final_target, &target, "t7_0");

        assert!(target.exists(), "旧临时文件应迁移到新名字");
        assert!(!legacy.exists(), "旧临时文件不能留在原地");
        assert!(PartMeta::path_for(&target).exists(), "断点文件应一并迁移");
        assert!(!PartMeta::path_for(&legacy).exists());

        // 新临时文件已存在时不得覆盖
        std::fs::write(&legacy, b"stale").unwrap();
        std::fs::write(&target, b"current").unwrap();
        adopt_legacy_temp(&final_target, &target, "t7_0");
        assert_eq!(std::fs::read(&target).unwrap(), b"current");
        assert!(legacy.exists(), "有活跃临时文件时不迁移、不删除旧文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 启动清理的判定：只认任务 id 形状，别把用户文件当垃圾。
    #[test]
    fn temp_artifact_detection_is_narrow() {
        assert!(is_temp_artifact("movie.mp4.veloxgrab-downloading"));
        assert!(is_temp_artifact("movie.mp4.veloxgrab-downloading.veloxgrab-part"));
        assert!(is_temp_artifact("movie.mp4.veloxgrab-downloading.t1729_0"));
        assert!(is_temp_artifact("movie.mp4.veloxgrab-downloading.t1729_0.veloxgrab-part"));
        assert!(is_temp_artifact(
            "movie.mp4.veloxgrab-downloading.t1729_0.veloxgrab-part.tmp"
        ));
        // 用户自己的文件、备份、相似后缀
        assert!(!is_temp_artifact("movie.mp4"));
        assert!(!is_temp_artifact("notes.veloxgrab-downloading.bak"));
        assert!(!is_temp_artifact("notes.veloxgrab-downloading.abc"));
        assert!(!is_temp_artifact("movie.mp4.veloxgrab-part"));
    }

    /// 无扩展名 / 中文标题的递增命名同样落在 stem 之后、扩展位之前。
    #[test]
    fn unique_target_inserts_counter_before_extension() {
        let dir = std::env::temp_dir().join(format!("veloxgrab_uname_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bare = dir.join("数据");
        std::fs::write(&bare, b"x").unwrap();
        assert_eq!(unique_target(&bare), dir.join("数据 (1)"));

        let with_ext = dir.join("a.b.tar.gz");
        std::fs::write(&with_ext, b"x").unwrap();
        // file_stem("a.b.tar.gz") = "a.b.tar"，扩展位只切最后一段
        assert_eq!(unique_target(&with_ext), dir.join("a.b.tar (1).gz"));

        // 没有冲突时原样返回
        assert_eq!(unique_target(&dir.join("fresh.mp4")), dir.join("fresh.mp4"));

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

    /// 回归（M-3 断点互证）：伪造/损坏的 `.veloxgrab-part` 不能骗过续传判定。
    ///
    /// 判据是纯函数，逐条对应注释里列的三种盘上损坏 + 两种合法场景。
    #[test]
    fn resume_is_consistent_rejects_forged_or_stale_meta() {
        let total = 10 * 1024 * 1024u64;
        let ranges = split_ranges(total, 4);
        let seg_len = ranges[0].1 - ranges[0].0 + 1;
        let good = PartMeta {
            url: "https://e.com/f.bin".into(),
            total,
            segments_total: ranges.iter().map(|(s, e)| e - s + 1).collect(),
            segments_done: vec![seg_len; 4],
        };
        // 写满但文件短了三段：done 总和 > file_len，不成立
        assert!(!resume_is_consistent(&good, "https://e.com/f.bin", total, &ranges, total - seg_len));
        // 完整文件 + 全部写满 → 放行（已下完的边界，交给 merge 校验，无害）
        assert!(resume_is_consistent(&good, "https://e.com/f.bin", total, &ranges, total));

        // 某段谎报进度：done > seg_len（伪造）
        let mut over = good.clone();
        over.segments_done[0] = seg_len + 1024;
        assert!(!resume_is_consistent(&over, "https://e.com/f.bin", total, &ranges, total));

        // 文件被截短到只剩前 3 段：第 4 段声称写满，它的写入区间越过 EOF
        assert!(!resume_is_consistent(
            &good, "https://e.com/f.bin", total, &ranges, seg_len * 3
        ));
        // 前 3 段写满 + 第 4 段没写，盘长恰好 3 段：这是合法的中断续传
        let mut three = good.clone();
        three.segments_done[3] = 0;
        assert!(resume_is_consistent(
            &three, "https://e.com/f.bin", total, &ranges, seg_len * 3
        ));
        // 同样 meta 但文件被砍到 2 段半：第 3 段的尾部字节没了 → 拒绝
        assert!(!resume_is_consistent(
            &three, "https://e.com/f.bin", total, &ranges, seg_len * 2 + seg_len / 2
        ));

        // url 换人 / 总长换人 / 段数对不上 / 分段边界漂移
        assert!(!resume_is_consistent(&good, "https://evil.com/f.bin", total, &ranges, total));
        assert!(!resume_is_consistent(&good, "https://e.com/f.bin", total + 1, &ranges, total));
        let shifted = split_ranges(total, 5); // 线程数变了，段长不同
        assert!(!resume_is_consistent(&good, "https://e.com/f.bin", total, &shifted, total));
        let fewer = split_ranges(total, 2);
        assert!(!resume_is_consistent(&good, "https://e.com/f.bin", total, &fewer, total));

        // 合法的中段续传：文件预分配成 total（长度够），每段各写了一半
        let mut half = good.clone();
        for d in half.segments_done.iter_mut() {
            *d = seg_len / 2;
        }
        assert!(resume_is_consistent(&half, "https://e.com/f.bin", total, &ranges, total));
        // 但如果文件本身被截短到连「后段写到一半」的区间都容不下，必须拒绝
        assert!(!resume_is_consistent(
            &half, "https://e.com/f.bin", total, &ranges, seg_len * 2
        ));
    }

    /// M-1：URL 字面量闸门的逐分支判定。
    #[test]
    fn url_gate_blocks_metadata_literals_but_not_lan() {
        // 正常公网 / 局域网 / 回环：放行（桌面下载器对用户内网没有默认敌意）
        assert!(url_target_guarded("https://example.com/a.zip").is_ok());
        assert!(url_target_guarded("http://192.168.1.10/share/x.bin").is_ok());
        assert!(url_target_guarded("http://127.0.0.1:8000/local.mp4").is_ok());
        // 云元数据 / 未指定地址：拦
        assert!(url_target_guarded("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(url_target_guarded("http://169.254.1.1/x").is_err());
        assert!(url_target_guarded("http://0.0.0.0/x").is_err());
        assert!(url_target_guarded("http://[fe80::1]/x").is_err());
        assert!(url_target_guarded("http://[::]/x").is_err());
        // 非 http(s) scheme：拦（file:// 是「复制本机文件」原语）
        assert!(url_target_guarded("file:///C:/Windows/win.ini").is_err());
        assert!(url_target_guarded("gopher://x/y").is_err());
        // 「前缀合法、host 违规」的写法——正是 commands.rs 字符串前缀检查漏掉的
        assert!(url_target_guarded("https://[fe80::1]/x").is_err());
    }

    #[test]
    fn redirect_rejects_https_downgrade_and_bad_targets() {
        assert!(redirect_is_allowed("https://a.com/f", "https://cdn.b.com/f").is_ok());
        // https → http 降级拦；http → https 升级放
        assert!(redirect_is_allowed("https://a.com/f", "http://cdn.b.com/f").is_err());
        assert!(redirect_is_allowed("http://a.com/f", "https://cdn.b.com/f").is_ok());
        // 跳到元数据段 / 跳进 file://：一步都不许
        assert!(redirect_is_allowed("https://a.com/f", "http://169.254.169.254/x").is_err());
        assert!(redirect_is_allowed("https://a.com/f", "file:///C:/x").is_err());
        // 目标解析不了 → 拒
        assert!(redirect_is_allowed("https://a.com/f", "::::").is_err());
    }
}
