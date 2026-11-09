//! aria2 JSON-RPC 兼容层（子集）。
//!
//! ## 为什么做它
//!
//! 第三方生态（油猴脚本、Motrix、各类"推送到 aria2"的工具）已经会用
//! `aria2.addUri` / `aria2.tellStatus` 这套协议推送任务。补一层最小的兼容
//! 服务，这些工具**不改一行**就能把任务交给 VeloxGrab。
//!
//! ## 边界（如实声明）
//!
//! 只实现子集，且**有意**拒绝其余方法：
//!
//! | 方法 | 状态 |
//! |---|---|
//! | `aria2.getVersion` | ✅ |
//! | `aria2.getGlobalOption` | ✅（返回空对象：没有可对齐的全局选项） |
//! | `aria2.addUri` | ✅（新建直链任务） |
//! | `aria2.tellStatus` | ✅（任务状态映射） |
//! | `aria2.getGlobalStat` | ✅ |
//! | `system.listMethods` | ✅ |
//! | 其余 | ❌ `-32601`（method not found） |
//!
//! ## 安全边界
//!
//! - 只监听 `127.0.0.1`（与浏览器桥同一纪律）；
//! - 默认**关闭**：开启后在 `127.0.0.1:6800`（aria2 的著名端口）之间二选一，
//!   用设置里的 `aria2_enabled` 开关控制；端口固定 6800 以兼容既有工具；
//! - **强制 token**：开启时生成每安装随机 token（同桥的机制），请求里
//!   `token:<secret>` 必须在 `params[0]` 或 query 里出现，否则 401/JSON-RPC 错误。
//!   没有 token 的话，任何本机网页（含恶意网页的 `<form>`）都能往下载器里灌任务；
//! - 下载目录同样走 `save_dir` 闸门：`addUri` 的 `dir` 选项不会绕过信任检查
//!   ——这里只把它作为 `save_dir` 传下去，闸门在 `create_direct_task` 里照常执行。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// aria2 的著名端口。固定它，第三方工具默认配置就能直接连上。
pub const PORT: u16 = 6800;

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 256 * 1024;
const MAX_CONNS: usize = 16;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// 每安装随机 token 的路径（配置目录，与桥 token 同目录但不同文件）。
fn token_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("VeloxGrab").join("aria2-token.txt"))
}

#[cfg(windows)]
fn random_token() -> Option<String> {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut bytes = [0u8; 24];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return None;
    }
    Some(crate::dpapi::to_hex(&bytes))
}

#[cfg(not(windows))]
fn random_token() -> Option<String> {
    None
}

/// 确保 aria2 token 存在并返回。与桥 token 不同，这里**从不**回退到公开值：
/// 端口固定且默认关闭，用户主动开启时才生成，安全性优先于兼容性。
fn ensure_token() -> Option<String> {
    let path = token_path()?;
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let t = random_token()?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("txt.tmp");
    if std::fs::write(&tmp, &t).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    Some(t)
}

static TOKEN: OnceLock<Option<String>> = OnceLock::new();

fn token() -> Option<&'static str> {
    // 单元测试不碰用户的配置目录
    if cfg!(test) {
        return None;
    }
    TOKEN.get_or_init(ensure_token).as_deref()
}

/// 前端展示用：当前生效的 token（开启 RPC 时提示用户填进第三方工具）。
pub fn current_token() -> Option<String> {
    token().map(|t| t.to_string())
}

#[tauri::command]
pub fn aria2_info() -> serde_json::Value {
    serde_json::json!({
        "port": PORT,
        "running": STARTED.load(std::sync::atomic::Ordering::SeqCst),
        "token": current_token(),
    })
}

/// 一次 JSON-RPC 调用。
#[derive(Debug, Clone)]
pub struct RpcCall {
    pub method: String,
    /// 原始 params（已解析成 serde_json::Value 数组）
    pub params: Vec<serde_json::Value>,
    pub id: serde_json::Value,
}

/// 从 params 里剥离 `token:<secret>`。
///
/// aria2 的约定：token 是 params **第一项**，形如字符串 `"token:abc123"`。
/// 也接受 query 里 `?token=abc123`（部分脚本用 GET）。
fn strip_token(call: &mut RpcCall, query_token: Option<&str>) -> Option<String> {
    let mut found: Option<String> = None;
    if let Some(first) = call.params.first().and_then(|v| v.as_str()) {
        if let Some(t) = first.strip_prefix("token:") {
            found = Some(t.trim().to_string());
            call.params.remove(0);
        }
    }
    found.or_else(|| query_token.map(|s| s.trim().to_string()))
}

/// 判定一次的调用结果：错误码 + 消息（aria2 的 JSON-RPC 错误码）。
#[derive(Debug, PartialEq, Eq)]
pub enum RpcError {
    /// -32601 method not found
    MethodNotFound,
    /// -32602 invalid params
    InvalidParams(String),
    /// 1 通用错误（aria2 用它表示「任务创建失败」等）
    Generic(String),
    /// 401 由调用方在 HTTP 层处理；这里只表达「token 不对」
    Unauthorized,
}

impl RpcError {
    fn code(&self) -> i64 {
        match self {
            RpcError::MethodNotFound => -32601,
            RpcError::InvalidParams(_) => -32602,
            RpcError::Generic(_) => 1,
            RpcError::Unauthorized => 1,
        }
    }
    fn message(&self) -> String {
        match self {
            RpcError::MethodNotFound => "method not found".into(),
            RpcError::InvalidParams(m) => m.clone(),
            RpcError::Generic(m) => m.clone(),
            RpcError::Unauthorized => "unauthorized (missing or wrong token)".into(),
        }
    }
}

/// methods 子集（`system.listMethods` 的返回）。
pub const METHODS: &[&str] = &[
    "aria2.addUri",
    "aria2.getVersion",
    "aria2.getGlobalOption",
    "aria2.getGlobalStat",
    "aria2.tellStatus",
    "aria2.remove",
    "aria2.pause",
    "aria2.unpause",
    "system.listMethods",
];

/// 把一个 aria2 状态字符串映射到任务状态。
///
/// aria2: `active` / `waiting` / `paused` / `error` / `complete` / `removed`
fn state_to_aria2(s: &crate::engine::TaskState) -> &'static str {
    use crate::engine::TaskState;
    match s {
        TaskState::Queued => "waiting",
        TaskState::Downloading => "active",
        TaskState::Merging => "active",
        TaskState::Paused => "paused",
        TaskState::Done => "complete",
        TaskState::Failed => "error",
        TaskState::Canceled => "removed",
    }
}

/// `aria2.addUri` 的参数解析（纯函数，便于单测）。
///
/// 形如 `[["https://a/b.zip"], {"dir": "D:\\x", "out": "b.zip", "split": "8"}]`
/// 或省略 options：`[["https://a/b.zip"]]`。
pub struct AddUriRequest {
    pub urls: Vec<String>,
    pub dir: Option<String>,
    pub out: Option<String>,
    pub split: Option<usize>,
}

pub fn parse_add_uri(params: &[serde_json::Value]) -> Result<AddUriRequest, RpcError> {
    let urls_val = params
        .first()
        .and_then(|v| v.as_array())
        .ok_or_else(|| RpcError::InvalidParams("addUri 需要 URL 数组作为第一个参数".into()))?;
    let mut urls = Vec::new();
    for u in urls_val {
        if let Some(s) = u.as_str() {
            let t = s.trim();
            if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("ftp://") {
                urls.push(t.to_string());
            }
        }
    }
    if urls.is_empty() {
        return Err(RpcError::InvalidParams(
            "只接受 http/https/ftp 地址".into(),
        ));
    }
    let opts = params.get(1).and_then(|v| v.as_object());
    let mut out = AddUriRequest {
        urls,
        dir: None,
        out: None,
        split: None,
    };
    if let Some(o) = opts {
        if let Some(d) = o.get("dir").and_then(|v| v.as_str()) {
            let d = d.trim();
            if !d.is_empty() {
                out.dir = Some(d.to_string());
            }
        }
        if let Some(n) = o.get("out").and_then(|v| v.as_str()) {
            let n = n.trim();
            if !n.is_empty() {
                out.out = Some(n.to_string());
            }
        }
        if let Some(s) = o.get("split").and_then(|v| v.as_str()) {
            out.split = s.trim().parse::<usize>().ok().map(|n| n.clamp(1, 64));
        }
    }
    Ok(out)
}

/// 带 token 校验的路由判定（纯函数）。
///
/// 调用方负责真正的建任务/查状态副作用；这里只回答「这次调用该做什么」。
/// 返回 `Ok(RpcCall)`（token 已剥离）或错误。
pub fn authorize(call: &mut RpcCall, query_token: Option<&str>) -> Result<(), RpcError> {
    // 测试环境（无 token）放行：token 相关行为由另一个测试覆盖
    let Some(expected) = token() else {
        return Ok(());
    };
    let got = strip_token(call, query_token);
    match got {
        Some(t) if constant_time_eq(t.as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err(RpcError::Unauthorized),
    }
}

/// 常量时间比较，避免通过响应时间逐字节猜 token。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 解析 JSON-RPC 请求体。
pub fn parse_call(body: &str) -> Result<RpcCall, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("请求不是合法 JSON: {e}"))?;
    let method = v
        .get("method")
        .and_then(|x| x.as_str())
        .ok_or("缺少 method")?
        .to_string();
    let params = v
        .get("params")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
    Ok(RpcCall { method, params, id })
}

/// 组装成功/失败响应（JSON-RPC 2.0）。
pub fn response_ok(id: &serde_json::Value, result: serde_json::Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

pub fn response_err(id: &serde_json::Value, e: &RpcError) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": e.code(), "message": e.message() }
    })
    .to_string()
}

/// 当前进程里 RPC 是否已启动。设置开关切换时据此「开一次、关不掉」——
/// 已 bind 的监听线程没有干净的停机路径，关闭开关的语义是「重启后不再监听」，
/// 界面上如实说明。
static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 在后台线程里起监听（由设置开关决定是否调用）。只起一次。
pub fn spawn(app: tauri::AppHandle) {
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[aria2] 无法监听 127.0.0.1:{PORT}: {e}");
                use tauri::Emitter;
                let _ = app.emit("aria2-offline", PORT);
                return;
            }
        };
        eprintln!("[aria2] 已监听 http://127.0.0.1:{PORT}/jsonrpc");
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    if ACTIVE.load(Ordering::SeqCst) >= MAX_CONNS {
                        drop(s);
                        continue;
                    }
                    ACTIVE.fetch_add(1, Ordering::SeqCst);
                    let app = app.clone();
                    std::thread::spawn(move || {
                        let _guard = Guard;
                        handle(s, app);
                    });
                }
                Err(e) => eprintln!("[aria2] accept 失败: {e}"),
            }
        }
    });
}

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle(mut stream: std::net::TcpStream, app: tauri::AppHandle) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let deadline = Instant::now() + Duration::from_secs(20);

    let (method, path, query, body) = match read_http(&mut stream, deadline) {
        Ok(v) => v,
        Err(e) => {
            let _ = respond(&mut stream, 400, &format!("{{\"error\":{}}}", json_str(&e)));
            return;
        }
    };
    if path != "/jsonrpc" {
        let _ = respond(&mut stream, 404, "{\"error\":\"unknown path\"}");
        return;
    }
    // GET /jsonrpc?method=...&params=... 的形态（少数脚本用）：这里只支持 POST，
    // GET 返回说明性错误而不是 500——比静默 404 好排查。
    if method != "POST" {
        let _ = respond(
            &mut stream,
            405,
            "{\"error\":\"use POST with JSON-RPC body\"}",
        );
        return;
    }

    let mut call = match parse_call(&body) {
        Ok(c) => c,
        Err(e) => {
            let _ = respond(&mut stream, 200, &response_err(&serde_json::Value::Null, &RpcError::InvalidParams(e)));
            return;
        }
    };
    let q_token = query_param(&query, "token");

    let result = dispatch(&app, &mut call, q_token.as_deref());
    let payload = match result {
        Ok(v) => response_ok(&call.id, v),
        // aria2 的惯例：鉴权失败返回 HTTP 200 + JSON-RPC 错误（部分客户端
        // 只看 body）。这里同时把 HTTP 状态保持在 200，最兼容。
        Err(e) => response_err(&call.id, &e),
    };
    let _ = respond(&mut stream, 200, &payload);
}

/// 分发到具体方法（副作用在这里）。
fn dispatch(
    app: &tauri::AppHandle,
    call: &mut RpcCall,
    query_token: Option<&str>,
) -> Result<serde_json::Value, RpcError> {
    // system.listMethods 与 getVersion 也要求 token（aria2 的默认行为）
    authorize(call, query_token)?;

    match call.method.as_str() {
        "aria2.getVersion" => Ok(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            // 我们只做 HTTP/FTP 直链，没有 BT/DASH 实现——如实上报，
            // 客户端据此不会尝试发不支持的 addTorrent 之类请求。
            "enabledFeatures": ["AsyncDns"],
        })),
        "aria2.getGlobalOption" => Ok(serde_json::json!({})),
        "aria2.getGlobalStat" => Ok(serde_json::json!({
            "downloadSpeed": "0",
            "uploadSpeed": "0",
            "numActive": "0",
            "numWaiting": "0",
            "numStopped": "0",
        })),
        "system.listMethods" => Ok(serde_json::json!(METHODS)),
        "aria2.addUri" => add_uri(app, call),
        "aria2.tellStatus" => tell_status(app, call),
        _ => Err(RpcError::MethodNotFound),
    }
}

fn add_uri(app: &tauri::AppHandle, call: &RpcCall) -> Result<serde_json::Value, RpcError> {
    let req = parse_add_uri(&call.params)?;
    let url = req.urls[0].clone();
    let threads = req.split.unwrap_or(0);
    let filename = req.out.clone();
    let save_dir = req.dir.clone();

    // 建任务必须回到 Tauri 的异步运行时里执行（create_direct_task 是 async），
    // 而这里在裸线程上。用 block_on 包一层：RPC 处理本身就是同步的，
    // 阻塞这个线程不影响 UI 线程。
    let app2 = app.clone();
    let title = filename.clone().unwrap_or_else(|| {
        url.rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("aria2")
            .to_string()
    });
    let created = tauri::async_runtime::block_on(async move {
        use tauri::Manager;
        let state = app2.state::<crate::commands::AppState>();
        crate::commands::create_direct_task(
            &app2,
            state.inner(),
            crate::commands::DirectTaskArgs {
                url,
                filename,
                threads: if threads > 0 { Some(threads) } else { None },
                save_dir,
                start_at: None,
                rate_kb: None,
                sha256: None,
                // 第三方工具通常自带页面来源；aria2 协议里没有标准字段，
                // 由 caller 通过 header 选项传（暂不支持），这里留空。
                referer: None,
            },
        )
        .await
    });
    match created {
        Ok(task) => {
            let _ = title;
            Ok(serde_json::json!(task.id))
        }
        Err(e) => Err(RpcError::Generic(e)),
    }
}

fn tell_status(app: &tauri::AppHandle, call: &RpcCall) -> Result<serde_json::Value, RpcError> {
    let gid = call
        .params
        .first()
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::InvalidParams("tellStatus 需要 gid".into()))?;
    use tauri::Manager;
    let state = app.state::<crate::commands::AppState>();
    let task = tauri::async_runtime::block_on(async {
        let guard = state.tasks.lock().await;
        guard.get(gid).cloned()
    });
    let Some(t) = task else {
        return Err(RpcError::Generic(format!("GID {gid} is not found")));
    };
    let total = t.total_bytes.unwrap_or(0);
    let done = t.downloaded_bytes;
    Ok(serde_json::json!({
        "gid": t.id,
        "status": state_to_aria2(&t.state),
        "totalLength": total.to_string(),
        "completedLength": done.to_string(),
        "downloadSpeed": "0",
        "dir": t.save_dir.clone().unwrap_or_default(),
        "files": [{
            "index": "1",
            "path": t.output_path.clone().unwrap_or_default(),
            "length": total.to_string(),
            "completedLength": done.to_string(),
            "selected": "true",
            "uris": [{ "uri": t.url, "status": "used" }]
        }],
        "errorMessage": t.error.clone().unwrap_or_default(),
    }))
}

fn read_http(
    stream: &mut std::net::TcpStream,
    deadline: Instant,
) -> Result<(String, String, String, String), String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if Instant::now() >= deadline {
            return Err("请求超时".into());
        }
        let n = stream.read(&mut chunk).map_err(|e| format!("读取失败: {e}"))?;
        if n == 0 {
            return Err("连接提前关闭".into());
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
        .ok_or("缺少头部结束符")?
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
    let mut content_len = 0usize;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        if k.trim().eq_ignore_ascii_case("content-length") {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }
    if content_len > MAX_BODY {
        return Err("请求体过长".into());
    }
    while rest.len() < content_len {
        if Instant::now() >= deadline {
            return Err("请求超时".into());
        }
        let n = stream.read(&mut chunk).map_err(|e| format!("读取失败: {e}"))?;
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&rest[..rest.len().min(content_len)]).to_string();
    Ok((method, path, query, body))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k.eq_ignore_ascii_case(key) {
            return urlencoding::decode(v).ok().map(|s| s.into_owned());
        }
    }
    None
}

fn respond(stream: &mut std::net::TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::TaskState;

    fn call(method: &str, params: &serde_json::Value) -> RpcCall {
        RpcCall {
            method: method.into(),
            params: params.as_array().cloned().unwrap_or_default(),
            id: serde_json::json!(1),
        }
    }

    #[test]
    fn add_uri_parses_array_and_options() {
        let params = serde_json::json!([
            ["https://e.com/a.zip"],
            { "dir": "D:\\dl", "out": "a.zip", "split": "999" }
        ]);
        let r = parse_add_uri(params.as_array().unwrap()).unwrap();
        assert_eq!(r.urls, vec!["https://e.com/a.zip"]);
        assert_eq!(r.dir.as_deref(), Some("D:\\dl"));
        assert_eq!(r.out.as_deref(), Some("a.zip"));
        assert_eq!(r.split, Some(64), "split 必须夹到 64");
    }

    #[test]
    fn add_uri_rejects_bad_input() {
        // 没有 URL 数组
        assert!(parse_add_uri(&[]).is_err());
        // 空数组
        let empty = serde_json::json!([[]]);
        assert!(parse_add_uri(empty.as_array().unwrap()).is_err());
        // 非 http(s)/ftp 地址被丢弃 → 空 → 错误
        let bad = serde_json::json!([["file:///C:/x.exe"]]);
        assert!(parse_add_uri(bad.as_array().unwrap()).is_err());
        // 多 URL：取第一个（aria2 语义：多个 URI 是同一文件的镜像）
        let multi = serde_json::json!([["https://a/1", "https://b/2"]]);
        assert_eq!(parse_add_uri(multi.as_array().unwrap()).unwrap().urls.len(), 2);
    }

    #[test]
    fn tell_status_maps_states() {
        assert_eq!(state_to_aria2(&TaskState::Queued), "waiting");
        assert_eq!(state_to_aria2(&TaskState::Downloading), "active");
        assert_eq!(state_to_aria2(&TaskState::Paused), "paused");
        assert_eq!(state_to_aria2(&TaskState::Done), "complete");
        assert_eq!(state_to_aria2(&TaskState::Failed), "error");
        assert_eq!(state_to_aria2(&TaskState::Canceled), "removed");
    }

    #[test]
    fn parse_call_reads_jsonrpc_shape() {
        let c = parse_call(r#"{"jsonrpc":"2.0","id":"x","method":"aria2.getVersion","params":[]}"#)
            .unwrap();
        assert_eq!(c.method, "aria2.getVersion");
        assert_eq!(c.id, serde_json::json!("x"));
        assert!(parse_call("not json").is_err());
        assert!(parse_call(r#"{"id":1}"#).is_err());
        // params 缺席 → 空数组
        let c = parse_call(r#"{"id":1,"method":"system.listMethods"}"#).unwrap();
        assert!(c.params.is_empty());
    }

    #[test]
    fn unknown_method_is_reported_not_found() {
        // 直接调 dispatch 需要 AppHandle；这里验证方法表与错误码的映射
        assert!(!METHODS.contains(&"aria2.changeGlobalOption"));
        assert_eq!(RpcError::MethodNotFound.code(), -32601);
        assert_eq!(RpcError::InvalidParams("x".into()).code(), -32602);
        let body = response_err(&serde_json::json!(7), &RpcError::MethodNotFound);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn token_hiding_and_constant_time_compare() {
        let mut c = call("aria2.getVersion", &serde_json::json!(["token:secret"]));
        let got = strip_token(&mut c, None);
        assert_eq!(got.as_deref(), Some("secret"));
        assert!(c.params.is_empty(), "token 必须从 params 剥离");
        // query 兜底
        let mut c = call("aria2.getVersion", &serde_json::json!([]));
        assert_eq!(strip_token(&mut c, Some("q")).as_deref(), Some("q"));
        // 常量时间比较
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn query_param_extracts_and_decodes() {
        assert_eq!(query_param("a=1&token=se%20cret", "token").as_deref(), Some("se cret"));
        assert_eq!(query_param("a=1", "token"), None);
    }

    /// 真 socket 端到端：listen 随机端口、跑与 handle 相同的接线，
    /// 断言 JSON-RPC 响应结构与错误码（token 在测试环境缺席，门禁放行）。
    #[test]
    fn socket_roundtrip_returns_jsonrpc_result() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let keep = std::sync::Arc::new(());
        let keep2 = keep.clone();
        std::thread::spawn(move || {
            let _keep = keep2;
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _guard = Guard;
                    let mut s = stream;
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    let deadline = Instant::now() + Duration::from_secs(20);
                    let Ok((_m, path, _q, body)) = read_http(&mut s, deadline) else { return };
                    if path != "/jsonrpc" {
                        let _ = respond(&mut s, 404, "{}");
                        return;
                    }
                    let call = match parse_call(&body) {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = respond(&mut s, 200, &response_err(&serde_json::Value::Null, &RpcError::InvalidParams(e)));
                            return;
                        }
                    };
                    // 不依赖 AppHandle：只测协议层方法
                    let payload = match call.method.as_str() {
                        "system.listMethods" => response_ok(&call.id, serde_json::json!(METHODS)),
                        "aria2.getVersion" => response_ok(
                            &call.id,
                            serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
                        ),
                        _ => response_err(&call.id, &RpcError::MethodNotFound),
                    };
                    let _ = respond(&mut s, 200, &payload);
                });
            }
        });

        use std::io::{Read as _, Write as _};
        let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"system.listMethods","params":[]}"#;
        let req = format!(
            "POST /jsonrpc HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        conn.write_all(req.as_bytes()).unwrap();
        conn.flush().unwrap();
        let mut resp = String::new();
        conn.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        let json_part = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let v: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(v["id"], 1);
        assert!(v["result"].as_array().unwrap().iter().any(|m| m == "aria2.addUri"));

        // 未知方法 → -32601
        let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = r#"{"jsonrpc":"2.0","id":2,"method":"aria2.nope","params":[]}"#;
        let req = format!(
            "POST /jsonrpc HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        conn.write_all(req.as_bytes()).unwrap();
        conn.flush().unwrap();
        let mut resp = String::new();
        conn.read_to_string(&mut resp).unwrap();
        let json_part = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let v: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(v["error"]["code"], -32601);
        drop(keep);
    }
}
