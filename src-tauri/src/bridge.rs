//! 浏览器扩展 ↔ VGrab 的本地传输桥。
//!
//! ## 为什么需要它
//!
//! `vgrab://` 协议本身是通的（注册表、单实例回调、冷启动 argv 都验证过），
//! 但**扩展拉不起来它**：Chrome/Edge 会静默拦截扩展发起的自定义协议导航
//! （`chrome.tabs.update({url: "vgrab://..."})` 不报错，就是不发生）。
//! 于是「网页点下载 → 交给 VGrab」这条链路在真实浏览器里 100% 断掉。
//!
//! Native Messaging 是官方方案，但代价是：扩展 ID 必须固定（要往
//! manifest 里塞 RSA 公钥）、要额外注册
//! `HKCU\Software\Google\Chrome\NativeMessagingHosts\...`、还要多打一个
//! 中间 host 可执行文件。三样里任何一样装漏，用户看到的就是「扩展没反应」，
//! 而且极难自查。
//!
//! 回环 HTTP 把这些全省了：扩展 `fetch("http://127.0.0.1:58943/add?url=...")`
//! 就完事，不需要固定 ID、不需要注册表、不需要额外二进制。
//!
//! ## 安全边界
//!
//! 监听地址写死 `127.0.0.1`，外部网络根本连不到。真正的风险是**本机其它程序**
//! 或恶意网页往这个端口灌下载任务，所以：
//!
//! 1. 只接受 `Origin` 为 `chrome-extension://` 的请求。网页 JS 发起的请求
//!    Origin 一定是 `http(s)://...`，浏览器不允许伪造，这一条挡住了「任意
//!    网页偷偷让 VGrab 下东西」。
//! 2. 请求体上限 64 KiB、请求头上限 8 KiB，防止本机程序用超长行把内存吃满。
//! 3. 只解析 `/add` 一个路径，其它一律 404——桥不是通用 RPC。
//!
//! 注意 `Origin` 缺失时**也放行**：curl / 手工测试不会带 Origin，而能连上
//! 127.0.0.1 的本机程序本来就已经能直接调 `vgrab://`，拦它没有意义。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 本进程启动以来，是否真的收到过浏览器扩展发来的请求。
///
/// 这是「扩展装好了没有」唯一可靠的判据：`extension-installed.flag` 只说明
/// 安装器把文件拷过去了，用户完全可以一直没在浏览器里加载它。只有扩展真的
/// 跑起来并发出请求，这里才会变 true。
///
/// 用 AtomicBool 而不是加锁：写入方是每个连接线程，读取方是 Tauri 命令线程，
/// 一个字节的标志不值得为它引入互斥量。
static EXT_SEEN: AtomicBool = AtomicBool::new(false);

/// 前端据此决定要不要继续提示用户去加载扩展。
pub fn extension_seen() -> bool {
    EXT_SEEN.load(Ordering::Relaxed)
}

/// 监听端口。
///
/// 选在 58943 这种高位端口：避开系统服务段，也避开常见的 8080/3000 开发端口。
/// 固定端口而不是随机端口，是因为扩展那边要写死 URL——随机端口就得让用户
/// 手工复制粘贴一次端口号，那是更差的产品体验。
pub const PORT: u16 = 58943;

/// 请求头 / 请求体的硬上限。
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 64 * 1024;

/// 在后台线程里起监听。
///
/// 失败只打日志不阻断启动：端口被占（比如用户开了两个实例、或者别的程序
/// 正好占了 58943）时，程序其余功能必须照常可用，扩展侧会自动退回
/// `vgrab://` 协议路径。
pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[bridge] 无法监听 127.0.0.1:{PORT}: {e}（扩展将退回 vgrab:// 协议）");
                return;
            }
        };
        eprintln!("[bridge] 已监听 http://127.0.0.1:{PORT}/add");

        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let app = app.clone();
                    // 每个连接独立线程：扩展可能同时发多个（比如批量抓链接），
                    // 串行处理会让后面的等前面超时。
                    std::thread::spawn(move || handle(s, app));
                }
                Err(e) => eprintln!("[bridge] accept 失败: {e}"),
            }
        }
    });
}

fn handle(mut stream: TcpStream, app: tauri::AppHandle) {
    // 读超时：防止连上就不发数据的连接把线程永久占住。
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let (method, path, query, origin, body) = match read_request(&mut stream) {
        Ok(v) => v,
        Err(e) => {
            let _ = respond(&mut stream, 400, &format!("{{\"ok\":false,\"error\":{}}}", json_str(&e)));
            return;
        }
    };

    // 预检：扩展用 GET 就够（没有自定义头），但别的地方可能发 OPTIONS。
    if method == "OPTIONS" {
        let _ = respond(&mut stream, 204, "");
        return;
    }

    if path != "/add" && path != "/ping" {
        let _ = respond(&mut stream, 404, "{\"ok\":false,\"error\":\"unknown path\"}");
        return;
    }

    if let Some(o) = &origin {
        if !o.starts_with("chrome-extension://") && !o.starts_with("moz-extension://") {
            let _ = respond(
                &mut stream,
                403,
                "{\"ok\":false,\"error\":\"only browser extensions may use this bridge\"}",
            );
            return;
        }
        // 走到这里说明扩展真的在跑，记下来（见 EXT_SEEN 的注释）。
        EXT_SEEN.store(true, Ordering::Relaxed);
    }

    // 扩展刚启动时打一次 /ping，只为了上报「我活着」。见 EXT_SEEN 注释：
    // 扩展在后台 SW 里启动就 ping，用户就算从没下载过，VGrab 也能知道扩展已加载。
    if path == "/ping" {
        let _ = respond(&mut stream, 200, "{\"ok\":true}");
        return;
    }

    // 参数可以走 query（GET）或 JSON body（POST）。POST 留给 URL 特别长
    // 的场景：某些带签名的下载链接能到 4~8 KB，塞 query 会撞上限。
    let link = if method == "POST" && !body.is_empty() {
        match build_link_from_json(&body) {
            Ok(l) => l,
            Err(e) => {
                let _ = respond(&mut stream, 400, &format!("{{\"ok\":false,\"error\":{}}}", json_str(&e)));
                return;
            }
        }
    } else {
        format!("vgrab://add?{query}")
    };

    // 交给既有的协议分发路径：入队 + 唤醒事件 + 把窗口拉到前台。
    // 复用而不是另写一套，是为了让「扩展发来的」和「协议跳转来的」走
    // 完全相同的校验与前端交互（包括另存为弹窗）。
    crate::dispatch_external_link(&app, link);

    let _ = respond(&mut stream, 200, "{\"ok\":true}");
}

/// 解析请求行 + 头，必要时读 body。返回 (method, path, query, origin, body)。
fn read_request(stream: &mut TcpStream) -> Result<(String, String, String, Option<String>, String), String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // 一直读到出现空行（\r\n\r\n）为止。
    loop {
        let n = stream.read(&mut chunk).map_err(|e| format!("读取请求失败: {e}"))?;
        if n == 0 {
            return Err("连接在请求头结束前关闭".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEAD {
            return Err("请求头过长".into());
        }
    }

    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("请求头缺少结束空行")?
        + 4;
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut rest = buf[head_end..].to_vec();

    let mut lines = head.lines();
    let request_line = lines.next().ok_or("空请求行")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("");

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut origin = None;
    let mut content_len = 0usize;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == "origin" {
            origin = Some(v.to_string());
        } else if k == "content-length" {
            content_len = v.parse().unwrap_or(0);
        }
    }

    if content_len > MAX_BODY {
        return Err("请求体过长".into());
    }

    // 头读完之后 body 可能已经在 rest 里了，补读剩下的。
    while rest.len() < content_len {
        let n = stream.read(&mut chunk).map_err(|e| format!("读取请求体失败: {e}"))?;
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&chunk[..n]);
    }

    let body = String::from_utf8_lossy(&rest[..rest.len().min(content_len)]).to_string();
    Ok((method, path, query, origin, body))
}

/// 把 POST 的 JSON body 转成 `vgrab://add?...` 链接。
fn build_link_from_json(body: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("body 不是合法 JSON: {e}"))?;
    let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("").trim();
    if url.is_empty() {
        return Err("body 里缺少 url".into());
    }
    let mut parts = vec![format!("url={}", urlencoding::encode(url))];
    if let Some(name) = v.get("name").and_then(|x| x.as_str()) {
        if !name.is_empty() {
            parts.push(format!("name={}", urlencoding::encode(name)));
        }
    }
    if let Some(dir) = v.get("dir").and_then(|x| x.as_str()) {
        if !dir.is_empty() {
            parts.push(format!("dir={}", urlencoding::encode(dir)));
        }
    }
    if let Some(t) = v.get("threads").and_then(|x| x.as_u64()) {
        parts.push(format!("threads={t}"));
    }
    Ok(format!("vgrab://add?{}", parts.join("&")))
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    // Access-Control-Allow-Origin 给 * 是安全的：真正的门禁是上面的 Origin 校验，
    // 而不是这条响应头。扩展拿 host_permissions 本来就跨域，这里加上是为了
    // 将来用普通网页做测试页时不用改后端。
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    if status != 204 {
        stream.write_all(body.as_bytes())?;
    }
    let _ = stream.flush();
    Ok(())
}

/// 把字符串转义成 JSON 字面量（只用于错误消息）。
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}
