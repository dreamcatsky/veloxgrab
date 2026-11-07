//! 浏览器扩展 ↔ VeloxGrab 的本地传输桥。
//!
//! ## 为什么需要它
//!
//! `veloxgrab://` 协议本身是通的（注册表、单实例回调、冷启动 argv 都验证过），
//! 但**扩展拉不起来它**：Chrome/Edge 会静默拦截扩展发起的自定义协议导航
//! （`chrome.tabs.update({url: "veloxgrab://..."})` 不报错，就是不发生）。
//! 于是「网页点下载 → 交给 VeloxGrab」这条链路在真实浏览器里 100% 断掉。
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
//! 1. **Origin 必须是扩展来源**（`chrome-extension://` / `moz-extension://`）。
//!    网页 JS 用 `fetch` 发请求时浏览器一定附上自己的 http(s) Origin → 403；
//!    而 **不带 Origin 的请求同样拒绝**——这条是后补的洞：`<img src="http://127.0.0.1:58943/add?...">`
//!    这类普通图片加载**从不发送 Origin**，旧实现把「无 Origin」当 curl 手工
//!    测试放行，等于给任意网页留了一条静默灌任务的 CSRF 通道（能不能被 Chrome
//!    的 PNA 提前掐掉取决于浏览器版本与页面自身分类，不能当依赖）。扩展走
//!    POST + `Content-Type: application/json`，属于非简单请求，真实请求必然
//!    带 Origin（预检也带），所以默认拒绝不伤正常路径。
//! 2. 请求体上限 64 KiB、请求头上限 8 KiB，防止本机程序用超长行把内存吃满。
//! 3. 只解析 `/add` 与 `/ping` 两个路径，其它一律 404——桥不是通用 RPC。
//! 4. **每安装 token 门禁**（`bridge-token.txt`）：请求必须带 `X-VeloxGrab-Token`
//!    头。Origin 只认 scheme 前缀，意味着任何已装扩展都能过 Origin 这关；token
//!    挡的是「照抄公开值就能驱动下载器」的抄写型扩展——安装包/仓库里那份
//!    token 只是**公开引导值**，首次运行即被替换成每安装随机值（见
//!    `ensure_install_token`），副本存配置目录（安装器覆盖不到）：应用自更新
//!    用公开值覆盖扩展目录后据此**还原**而不是再随机一次，已运行的扩展 SW
//!    缓存的旧 token 继续有效，升级零摩擦。其它扩展的沙箱读不到本程序的
//!    扩展目录与配置目录，「从 GitHub 抄 token」从此不成立。本机进程仍读得到
//!    这两处文件——那本来就在威胁模型之外（能读写你磁盘的进程不需要经过
//!    桥）。开发构建资源目录下没有扩展目录，token 缺席时**不启用**门禁
//!    （fail-open）并打警告——否则 `tauri dev` 全靠手搓请求调试。
//! 5. `Host` 头必须精确等于 `127.0.0.1:58943`：把 DNS rebinding 从「靠
//!    Origin 门禁间接覆盖」变成结构性不可能（rebinding 页面解析出的 evil.com
//!    会作为 Host 出现，直接 403）。
//! 6. **并发连接上限 32**、**`/add` 每 10 秒最多 20 条**、**待取深链队列上限
//!    64**：防恶意扩展灌爆任务队列 / 线程数，并缓解「每次 /add 都抢焦点」
//!    的骚扰面。
//!
//! 残余风险（如实声明）：固定端口 + 无挑战应答，意味着本机恶意进程仍可
//! 抢先 bind 58943 冒充桥收割扩展上发的 URL——token 在请求里，挡不住「接收」，
//! 只挡「驱动」。彻底解法要 Native Messaging（扩展 ID 固定）或端口协商文件，
//! 成本见文件头，暂不做。//!
//! curl 手工测试要带 `-H "Origin: chrome-extension://manual-test"`；能连上
//! 127.0.0.1 的本机进程本来就能直接拉 `veloxgrab://`，这条闸防的是**浏览器
//! 里的网页**，不是本机进程。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 本进程启动以来，是否真的收到过浏览器扩展发来的请求。
///
/// 这是「扩展装好了没有」唯一可靠的判据：`extension-installed.flag` 只说明
/// 安装器把文件拷过去了，用户完全可以一直没在浏览器里加载它。只有扩展真的
/// 跑起来并发出请求，这里才会变 true。
///
/// 用 AtomicBool 而不是加锁：写入方是每个连接线程，读取方是 Tauri 命令线程，
/// 一个字节的标志不值得为它引入互斥量。
static EXT_SEEN: AtomicBool = AtomicBool::new(false);

/// 「扩展活跃」的**持久化**副本：`%APPDATA%\VeloxGrab\extension-seen.flag`。
///
/// 为什么内存标志不够：EXT_SEEN 随进程清零，而扩展的 ping 只在它的 service
/// worker 醒来时打（浏览器启动、或用户操作扩展）。典型死锁场景——
/// 浏览器一直开着（SW 早就睡下，不会再 ping），用户重启 VeloxGrab：
/// 每次 boot() 查 `extension_active` 都是 false，于是每次启动都弹
/// 「去加载扩展」引导，哪怕扩展明明装好并在工作。
/// 落盘一份就彻底解决：收到过一次扩展请求 = 永久事实。
fn seen_flag_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("VeloxGrab").join("extension-seen.flag"))
}

/// 首次确认扩展活跃时写盘。只在 false→true 的那一次写，ping 再多也不重复 IO；
/// 写失败（目录权限等）不影响内存标志，只是下次启动还要靠 ping 重新确认。
fn persist_seen() {
    let Some(p) = seen_flag_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, b"1");
}

/// 前端据此决定要不要继续提示用户去加载扩展。
///
/// 内存标志为假时回读一次磁盘（覆盖「扩展上次 ping 过、这次进程还没收到」的
/// 重启场景），读到就把内存标志补上，避免每次查询都 stat 文件。
pub fn extension_seen() -> bool {
    if EXT_SEEN.load(Ordering::Relaxed) {
        return true;
    }
    let seen = seen_flag_path().map(|p| p.is_file()).unwrap_or(false);
    if seen {
        EXT_SEEN.store(true, Ordering::Relaxed);
    }
    seen
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

/// 并发连接上限：扩展批量抓链接也就几个并发，32 已经远超正常用量；
/// 超出的连接直接关掉，不让「每连接一线程」被恶意刷爆。
const MAX_CONNS: usize = 32;
static ACTIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

/// `/add` 速率限制：10 秒窗口内最多 20 条。正常用户手点+批量抓取到不了；
/// 恶意扩展 `setInterval` 灌任务会撞上 429。
const ADD_RATE_WINDOW: Duration = Duration::from_secs(10);
const ADD_RATE_MAX: usize = 20;
static ADD_HITS: Mutex<VecDeque<Instant>> = Mutex::new(VecDeque::new());

/// 随安装 token 的权威副本位置（配置目录，安装器覆盖不到）。
///
/// 放配置目录而不是安装目录是轮换方案的关键：应用自更新会用安装包里的
/// **公开引导值**覆盖扩展目录与资源目录下的 bridge-token.txt。若权威值也在
/// 安装目录，更新后扩展继续拿公开值——轮换等于没做。权威值在配置目录，
/// 「更新后公开值回归扩展目录」就能被识别并用权威值还原（见
/// `ensure_install_token` 的写入分支）。
fn install_token_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("VeloxGrab").join("bridge-token.txt"))
}

/// 资源目录里的随包 token（公开引导值）。开发构建没有资源目录 → None。
fn bundled_token_path() -> Option<std::path::PathBuf> {
    crate::ytdlp::resource_dir().map(|d| d.join("extension").join("bridge-token.txt"))
}

/// 32 字节加密随机数 → 64 位十六进制。BCryptGenRandom 走系统首选 RNG
/// （BCRYPT_USE_SYSTEM_PREFERRED_RNG），无需打开算法句柄；与 update.rs 的
/// SHA-256 同属 BCrypt，不新增特性面。
#[cfg(windows)]
fn random_token() -> Option<String> {
    use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut bytes = [0u8; 32];
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
    // 本程序只发布 Windows 构建；非 Windows 平台门禁本就 fail-open
    //（资源目录缺席），这里返回 None 走同一条降级路径。
    None
}

/// 确保生效 token 存在并写回扩展目录，返回当前生效值。
///
/// 判定读三个位置，优先级：配置目录（权威）> 扩展目录现值 > 随包引导值 > 新随机。
/// 分支语义：
/// - 权威值存在：扩展目录里的值若与它不同（安装包覆盖、用户手改），用权威值
///   **还原**扩展目录——扩展 SW 每次醒来重读文件，缓存值只在 SW 生命周期内，
///   还原后下一轮 SW 就同步了。已装扩展若正持有被覆盖掉的旧值（极小窗口），
///   会收到一次 403 并由前端提示「刷新扩展」，随后自愈。
/// - 权威值缺席：**沿用扩展目录现值**（含随包公开值）并提升为权威——不强行
///   换新，避免「重装系统丢配置目录」之外的场景里已加载扩展的缓存失效。
/// - 两处都空：生成随机值，同时写权威与扩展目录。
///
/// 只在 token 门禁首次求值时调用一次（OnceLock），之后全部走内存缓存。
#[cfg(windows)]
fn ensure_install_token() -> Option<String> {
    fn read_trim(p: &std::path::Path) -> Option<String> {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
    fn write_atomic(p: &std::path::Path, s: &str) {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = p.with_extension("txt.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }

    let authority = install_token_path().and_then(|p| read_trim(&p));
    let installed = bundled_token_path().and_then(|p| read_trim(&p));

    // 判定拆成纯函数（decision_for）；文件副作用（还原/提升/新写）只按它的
    // 指示执行，便于单测三种分支的判定语义。
    let action = resolve_token_action(authority.as_deref(), installed.as_deref());
    let token = match action {
        TokenAction::KeepAuthority => {
            // 权威值存在：扩展目录与它不一致就用权威值还原（安装包把公开值
            // 写回去是每次自更新都会发生的事，还原让扩展目录与生效值重新一致）。
            if installed.as_deref() != authority.as_deref() {
                if let Some(p) = bundled_token_path() {
                    write_atomic(&p, authority.as_deref().unwrap_or_default());
                }
            }
            authority.unwrap()
        }
        TokenAction::PromoteInstalled => {
            // 权威缺席：现值（可能是公开引导值）提升为权威，写入配置目录。
            // 从公开值切到「公开值即权威」对请求方无感；真正的随机轮换发生在
            // 下一次配置目录被清且扩展目录也被覆盖时——那已经是全新安装。
            let t = installed.unwrap();
            if let Some(p) = install_token_path() {
                write_atomic(&p, &t);
            }
            t
        }
        TokenAction::GenerateFresh => {
            // 全新环境：生成每安装随机值，同时落权威与扩展目录。
            let Some(t) = random_token() else {
                eprintln!("[bridge] 随机 token 生成失败，token 门禁未启用");
                return None;
            };
            if let Some(p) = install_token_path() {
                write_atomic(&p, &t);
            }
            if let Some(p) = bundled_token_path() {
                write_atomic(&p, &t);
            }
            t
        }
    };
    Some(token)
}

/// token 轮换判定的三种动作（判定与文件 IO 分离，纯函数可单测）。
#[derive(Debug, PartialEq, Eq)]
enum TokenAction {
    /// 权威值生效；调用方负责把扩展目录还原成权威值。
    KeepAuthority,
    /// 扩展目录现值提升为权威（权威缺席、引导值/现值在场）。
    PromoteInstalled,
    /// 两处皆空：生成每安装随机值。
    GenerateFresh,
}

fn resolve_token_action(authority: Option<&str>, installed: Option<&str>) -> TokenAction {
    match (
        authority.map(str::trim).filter(|s| !s.is_empty()),
        installed.map(str::trim).filter(|s| !s.is_empty()),
    ) {
        (Some(_), _) => TokenAction::KeepAuthority,
        (None, Some(_)) => TokenAction::PromoteInstalled,
        (None, None) => TokenAction::GenerateFresh,
    }
}

#[cfg(not(windows))]
fn ensure_install_token() -> Option<String> {
    bundled_token_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 随安装 token 的内存缓存：首次求值时经 `ensure_install_token` 建立或轮换。
static BRIDGE_TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// 当前生效的 token；None 表示门禁未启用（开发构建 / 生成失败）。
fn bridge_token() -> Option<&'static str> {
    BRIDGE_TOKEN
        .get_or_init(|| {
            // 单元测试进程不得触碰用户的真实配置目录：ensure_install_token
            // 的 GenerateFresh 分支会往 %APPDATA%\VeloxGrab 写文件。测试里
            // 门禁整体跳过（token 相关行为由 route_rejects_wrong_token_when_enabled
            // 在「token 恰好可用」的环境下断言，写文件同样被这里拦住）。
            if cfg!(test) {
                return None;
            }
            let t = ensure_install_token();
            if t.is_none() {
                eprintln!("[bridge] 未找到 extension/bridge-token.txt，token 门禁未启用（开发构建属正常现象）");
            }
            t
        })
        .as_deref()
}

/// 本桥各监听 socket 的「Host 白名单」。
///
/// 门禁原先写死 `127.0.0.1:58943`，与固定端口耦合；socket 端到端测试要起
/// 随机端口，写死值就成了跨模块的隐藏常量。改成 spawn 时登记实际监听地址，
/// route 按登记表判定——顺带让「将来加备用端口/Unix socket」不用再改门禁。
static APPROVED_HOSTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn approved_hosts() -> &'static Mutex<Vec<String>> {
    APPROVED_HOSTS.get_or_init(|| Mutex::new(vec!["127.0.0.1:58943".to_string()]))
}

fn approve_host(host: String) {
    if let Ok(mut g) = approved_hosts().lock() {
        if !g.contains(&host) {
            g.push(host);
        }
    }
}

fn host_is_approved(h: Option<&str>) -> bool {
    let Some(h) = h else { return false };
    approved_hosts()
        .lock()
        .map(|g| g.iter().any(|a| a.eq_ignore_ascii_case(h)))
        .unwrap_or(false)
}

/// `Host` 头是否精确等于本桥的监听地址。
///
/// 浏览器发出的请求一定带 Host 且就是它连接的 `127.0.0.1:<port>`；DNS
/// rebinding 页面解析出来的域名会原样出现在 Host 里，在这里被拒。
/// 纯函数化的单测见 host_gate 系列；登记表语义的集成验证在 socket 测试里。
fn host_is_bridge(h: Option<&str>) -> bool {
    host_is_approved(h)
}

/// `/add` 速率闸：窗口内超限返回 false（调用方回 429）。
/// 锁中毒（理论不可能：闭包内不 panic）时按未限速处理，宁滥勿崩。
fn add_rate_limited() -> bool {
    let mut hits = match ADD_HITS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let cutoff = Instant::now() - ADD_RATE_WINDOW;
    hits.retain(|t| *t > cutoff);
    if hits.len() >= ADD_RATE_MAX {
        return true;
    }
    hits.push_back(Instant::now());
    false
}

/// 在后台线程里起监听。
///
/// 失败只打日志不阻断启动：端口被占（比如用户开了两个实例、或者别的程序
/// 正好占了 58943）时，程序其余功能必须照常可用，扩展侧会自动退回
/// `veloxgrab://` 协议路径。
pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[bridge] 无法监听 127.0.0.1:{PORT}: {e}（扩展将退回 veloxgrab:// 协议）");
                // 旧实现只打 stderr——GUI 进程里没人看得见，用户只知道「扩展
                // 没反应」。这里通知前端弹一次提示，至少让「桥挂了」可见。
                let _ = tauri::Emitter::emit(&app, "bridge-offline", PORT);
                return;
            }
        };
        eprintln!("[bridge] 已监听 http://127.0.0.1:{PORT}/add");
        // Host 门禁按实际监听地址登记（与写死的 PORT 一致；将来换端口
        // 只需要这里跟 bind 对齐，门禁判定不用改）。
        approve_host(format!("127.0.0.1:{PORT}"));

        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    // 并发上限：超出直接关连接（不回 5xx——那还要再占一次写），
                    // 正常扩展路径永远到不了这个数。
                    if ACTIVE_CONNS.load(Ordering::SeqCst) >= MAX_CONNS {
                        drop(s);
                        continue;
                    }
                    ACTIVE_CONNS.fetch_add(1, Ordering::SeqCst);
                    let app = app.clone();
                    // 每个连接独立线程：扩展可能同时发多个（比如批量抓链接），
                    // 串行处理会让后面的等前面超时。
                    std::thread::spawn(move || {
                        let _guard = ConnGuard;
                        handle(s, app);
                    });
                }
                Err(e) => eprintln!("[bridge] accept 失败: {e}"),
            }
        }
    });
}

/// 连接计数守卫：线程任何退出路径（含 panic）都会把计数还回去。
struct ConnGuard;

impl Drop for ConnGuard {
    fn drop(&mut self) {
        ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle(mut stream: TcpStream, app: tauri::AppHandle) {
    // 读超时：防止连上就不发数据的连接把线程永久占住。
    // 注意它约束的是「单次 read 的间隔」；滴字慢连（每 4.9 秒 1 字节）能让
    // read 永不超时，所以还要一个连接级总时限（见 read_request 的 deadline）。
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    // 连接总时限：正常扩展请求（预检 + 真实请求两次往返）远用不了 30 秒；
    // 超时按协议错误处理，线程回收。
    let deadline = Instant::now() + Duration::from_secs(30);

    let req = match read_request(&mut stream, deadline) {
        Ok(v) => v,
        Err(e) => {
            let _ = respond(&mut stream, 400, &format!("{{\"ok\":false,\"error\":{}}}", json_str(&e)));
            return;
        }
    };

    let outcome = route(req);
    if let Some(link) = outcome.dispatched_link {
        // 交给既有的协议分发路径：入队 + 唤醒事件 + 把窗口拉到前台。
        // 复用而不是另写一套，是为了让「扩展发来的」和「协议跳转来的」走
        // 完全相同的校验与前端交互（包括另存为弹窗）。
        crate::dispatch_external_link(&app, link);
    }
    let _ = respond(&mut stream, outcome.status, &outcome.body);
}

/// 路由决策：门禁链（Host → 路径 → Origin → 预检 → token → 限速）的纯函数化。
///
/// 从 `handle` 抽出来的动机是测试：socket 端到端测试构造 BridgeReq 走完整
/// 门禁链并断言状态码/放行链接，不必 mock AppHandle（dispatch 是唯一一处
/// 需要 app 的副作用，挪回 handle 后其余全部可测）。
/// 与 handle 的接线是结构性的：handle 只补「读请求 → route → 写响应 +
/// dispatch」，任何门禁调整都发生在 route 内，两边不会漂移。
struct RouteOutcome {
    status: u16,
    /// 响应体。String 而不是 &'static str：JSON 解析失败的错误文案是动态的，
    /// 为一条错误路径让成功路径多一次分配，换来 handle 接线的无条件正确
    /// （dispatched_link 为 Some 就派发，400 路径绝不带链接）。
    body: String,
    /// 放行时要交给 dispatch_external_link 的链接；None = 本请求不派发。
    dispatched_link: Option<String>,
}

fn static_outcome(status: u16, body: &'static str) -> RouteOutcome {
    RouteOutcome { status, body: body.to_string(), dispatched_link: None }
}

fn route(req: BridgeReq) -> RouteOutcome {
    let (method, path, query, body) = (req.method, req.path, req.query, req.body);

    // DNS rebinding 纵深：Host 不是本桥监听地址的请求直接拒。
    // 放在所有分支之前——rebinding 的探测也不该得到任何响应。
    if !host_is_bridge(req.host.as_deref()) {
        return static_outcome(403, "{\"ok\":false,\"error\":\"bad host\"}");
    }

    if path != "/add" && path != "/ping" {
        return static_outcome(404, "{\"ok\":false,\"error\":\"unknown path\"}");
    }

    // 安全门禁见文件头「安全边界」：Origin 缺失 = 拒绝（img 加载类无 Origin
    // 请求曾是静默灌任务的 CSRF 通道）；带 Origin 但不是扩展来源 = 拒绝。
    if !origin_is_extension(req.origin.as_deref()) {
        return static_outcome(
            403,
            "{\"ok\":false,\"error\":\"only browser extensions may use this bridge\"}",
        );
    }

    // 预检：浏览器对非简单请求先发 OPTIONS。放在 Origin 门禁**之后**——
    // 扩展的预检必然带 chrome-extension:// Origin，正常路径不受影响；
    // 旧实现把预检放在门禁前，任何网页都能借 OPTIONS 探测「本机 58943
    // 有一个说 JSON 的服务」。注意预检请求**不可能**携带自定义头（浏览器
    // 会剥掉），所以 token 校验只对真实请求做，这里只回允许的头清单。
    if method == "OPTIONS" {
        return static_outcome(204, "");
    }

    // 走到这里说明扩展真的在跑，记下来（见 EXT_SEEN 的注释）。
    // 只在首次翻转时落盘，避免每次 ping 都写文件。
    // 放在 token 校验之前：扩展加载了但版本旧（还不带 token）也是「已安装」
    // 事实，不该因此反复弹引导。
    if !EXT_SEEN.swap(true, Ordering::Relaxed) {
        persist_seen();
    }

    // 每安装 token 门禁（开发构建 token 为 None 时跳过）。
    if let Some(expected) = bridge_token() {
        if req.token.as_deref() != Some(expected) {
            return static_outcome(
                403,
                "{\"ok\":false,\"error\":\"bridge token mismatch; please update the VeloxGrab extension\"}",
            );
        }
    }

    // 扩展刚启动时打一次 /ping，只为了上报「我活着」。见 EXT_SEEN 注释：
    // 扩展在后台 SW 里启动就 ping，用户就算从没下载过，VeloxGrab 也能知道扩展已加载。
    if path == "/ping" {
        return static_outcome(200, "{\"ok\":true}");
    }

    // 灌任务防护：正常用户（手点 + 批量抓取）到不了这个量级。
    if add_rate_limited() {
        return static_outcome(429, "{\"ok\":false,\"error\":\"too many requests; slow down\"}");
    }

    // 参数可以走 query（GET）或 JSON body（POST）。POST 留给 URL 特别长
    // 的场景：某些带签名的下载链接能到 4~8 KB，塞 query 会撞上限。
    let link = if method == "POST" && !body.is_empty() {
        match build_link_from_json(&body) {
            Ok(l) => l,
            Err(e) => {
                return RouteOutcome {
                    status: 400,
                    body: format!("{{\"ok\":false,\"error\":{}}}", json_str(&e)),
                    dispatched_link: None,
                };
            }
        }
    } else {
        format!("veloxgrab://add?{query}")
    };

    RouteOutcome { status: 200, body: "{\"ok\":true}".to_string(), dispatched_link: Some(link) }
}

/// 一次解析完成的桥请求。字段含义见 `read_request`。
struct BridgeReq {
    method: String,
    path: String,
    query: String,
    host: Option<String>,
    origin: Option<String>,
    token: Option<String>,
    body: String,
}

/// 解析请求行 + 头，必要时读 body。
fn read_request(stream: &mut TcpStream, deadline: Instant) -> Result<BridgeReq, String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // 一直读到出现空行（\r\n\r\n）为止。
    loop {
        // 连接级总时限：set_read_timeout 只管「单次 read 的间隔」，滴字慢连
        // （每 4.9 秒发 1 字节）能让下面的循环永不超时，8 KiB 头上限意味着
        // 单连接最长可挂数小时——每连接一线程的模型下就是数百线程被长期
        // 占用。deadline 是硬上限。
        if Instant::now() >= deadline {
            return Err("请求超时（连接总时限 30 秒）".into());
        }
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
    let mut host = None;
    let mut token = None;
    let mut content_len = 0usize;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        match k.as_str() {
            "origin" => origin = Some(v.to_string()),
            "host" => host = Some(v.to_string()),
            "x-veloxgrab-token" => token = Some(v.to_string()),
            "content-length" => content_len = v.parse().unwrap_or(0),
            _ => {}
        }
    }

    if content_len > MAX_BODY {
        return Err("请求体过长".into());
    }

    // 头读完之后 body 可能已经在 rest 里了，补读剩下的。
    while rest.len() < content_len {
        if Instant::now() >= deadline {
            return Err("请求超时（连接总时限 30 秒）".into());
        }
        let n = stream.read(&mut chunk).map_err(|e| format!("读取请求体失败: {e}"))?;
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&chunk[..n]);
    }

    let body = String::from_utf8_lossy(&rest[..rest.len().min(content_len)]).to_string();
    Ok(BridgeReq {
        method,
        path,
        query,
        host,
        origin,
        token,
        body,
    })
}

/// 把 POST 的 JSON body 转成 `veloxgrab://add?...` 链接。
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
    Ok(format!("veloxgrab://add?{}", parts.join("&")))
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    };
    // Access-Control-Allow-Origin 给 * 是安全的：真正的门禁是请求侧的 Origin 校验
    // （缺失即 403）+ token 校验，不是这条响应头。它的存在只为让扩展的 POST JSON
    // 预检（非简单请求）能顺利通过；网页就算预检过了，真实请求带的 http(s)
    // Origin 一样被 403 挡住，响应里也没有可利用的信息。
    // Allow-Headers 必须包含扩展要发的自定义头，否则预检直接失败。
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type, X-VeloxGrab-Token\r\n\
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

/// Origin 是否是「浏览器扩展」来源。None（请求里没有 Origin）同样拒绝：
/// 恶意网页的 `<img>` / 顶层导航类请求恰恰是不带 Origin 的那批。
/// 纯函数，便于单测这条门禁。
fn origin_is_extension(origin: Option<&str>) -> bool {
    match origin {
        Some(o) => o.starts_with("chrome-extension://") || o.starts_with("moz-extension://"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个「扩展发来的合法请求」作基准，逐字段改坏验证每道门禁。
    fn good_req() -> BridgeReq {
        BridgeReq {
            method: "POST".into(),
            path: "/add".into(),
            query: String::new(),
            host: Some("127.0.0.1:58943".into()),
            origin: Some("chrome-extension://abcdefghijklmnop".into()),
            token: None,
            body: r#"{"url":"https://e.com/a.zip"}"#.into(),
        }
    }

    /// P3-8：route 门禁链的端到端判定——按 handle 的真实顺序逐门验证，
    /// 放行时 dispatched_link 必须携带可交给 parse_deep_link 的链接。
    /// token 门禁在测试环境可能缺席（无资源目录 → 跳过），单独在
    /// route_rejects_wrong_token_when_enabled 里按环境现状断言。
    #[test]
    fn route_gate_chain_end_to_end() {
        // 基准：放行，链接可解析
        let ok = route(good_req());
        assert_eq!(ok.status, 200);
        let link = ok.dispatched_link.expect("合法 /add 必须派发");
        let parsed = crate::deeplink::parse_deep_link(&link).expect("派发链接必须可解析");
        assert_eq!(parsed.url, "https://e.com/a.zip");

        // Host 门禁（rebinding / 端口混淆 / 缺失）
        for bad in ["evil.example", "localhost:58943", "127.0.0.1:80"] {
            let mut r = good_req();
            r.host = Some(bad.into());
            assert_eq!(route(r).status, 403, "host={bad}");
        }
        let mut r = good_req();
        r.host = None;
        assert_eq!(route(r).status, 403);

        // 路径门禁
        let mut r = good_req();
        r.path = "/other".into();
        assert_eq!(route(r).status, 404);

        // Origin 门禁：网页来源、缺失、伪装
        for bad in ["https://evil.example", "http://localhost:3000", "xchrome-extension://fake"] {
            let mut r = good_req();
            r.origin = Some(bad.into());
            assert_eq!(route(r).status, 403, "origin={bad}");
        }
        let mut r = good_req();
        r.origin = None;
        assert_eq!(route(r).status, 403, "无 Origin 的 img 类请求必须拒");

        // 预检：204 且不派发
        let mut r = good_req();
        r.method = "OPTIONS".into();
        let pre = route(r);
        assert_eq!(pre.status, 204);
        assert!(pre.dispatched_link.is_none(), "预检绝不派发");

        // JSON body 坏：400 且不派发（400 绝不能带着半个链接走 dispatch）
        let mut r = good_req();
        r.body = "{not json".into();
        let bad = route(r);
        assert_eq!(bad.status, 400);
        assert!(bad.dispatched_link.is_none(), "400 路径绝不派发");
        assert!(bad.body.contains("error"), "400 必须带错误文案");
        // body 缺 url 同拒
        let mut r = good_req();
        r.body = r#"{"name":"x"}"#.into();
        assert_eq!(route(r).status, 400);

        // GET /add（query 通道）：同样放行且可解析
        let mut r = good_req();
        r.method = "GET".into();
        r.body = String::new();
        r.query = "url=https%3A%2F%2Fe.com%2Fb.bin&threads=8".into();
        let ok = route(r);
        assert_eq!(ok.status, 200);
        let parsed = crate::deeplink::parse_deep_link(&ok.dispatched_link.unwrap()).unwrap();
        assert_eq!(parsed.threads, Some(8));
    }

    /// token 门禁（bridge_token() 为 Some 的环境下）。测试环境 token 可能
    /// 缺席（门禁跳过）——缺席时显式标注退化，而不是静默通过。
    #[test]
    fn route_rejects_wrong_token_when_enabled() {
        let Some(expected) = bridge_token().map(str::to_string) else {
            eprintln!("[test] bridge_token 未启用（测试环境无资源目录），token 断言退化");
            return;
        };
        let mut r = good_req();
        r.token = Some("wrong-token".into());
        assert_eq!(route(r).status, 403);
        let mut r = good_req();
        r.token = Some(expected);
        assert_eq!(route(r).status, 200);
    }

    /// 真 socket 端到端：bind 随机端口，跑与 handle 相同接线的 accept 循环，
    /// 用 TCP 客户端发一条完整 POST，断言响应行/头/体。read_request 的解析、
    /// route 的门禁、respond 的拼包全部覆盖。
    /// 不监听 58943（可能与开发实例冲突）；Host 门禁按监听端口现算。
    #[test]
    fn socket_roundtrip_end_to_end() {
        let (port, handle_threads) = spawn_test_bridge();
        let app_host = format!("127.0.0.1:{port}");

        let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = r#"{"url":"https://e.com/socket.bin"}"#;
        let req = format!(
            "POST /add HTTP/1.1\r\nHost: {app_host}\r\nOrigin: chrome-extension://abc\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        use std::io::{Read, Write};
        conn.write_all(req.as_bytes()).unwrap();
        conn.flush().unwrap();
        let mut buf = String::new();
        conn.read_to_string(&mut buf).unwrap();

        assert!(buf.starts_with("HTTP/1.1 200 OK\r\n"), "响应行: {buf}");
        assert!(buf.contains("Content-Type: application/json"), "必须有 Content-Type: {buf}");
        assert!(buf.contains("Access-Control-Allow-Origin: *"), "扩展 POST 需要 CORS 头: {buf}");
        assert!(buf.ends_with("{\"ok\":true}"), "响应体: {buf}");
        drop(handle_threads);
    }

    /// 真 socket 负路径：Host 错 → 403 响应真的写回客户端。
    #[test]
    fn socket_rejects_bad_host_with_response() {
        let (port, handle_threads) = spawn_test_bridge();

        let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        use std::io::{Read, Write};
        let req = "GET /add HTTP/1.1\r\nHost: evil.example\r\nOrigin: chrome-extension://abc\r\n\r\n";
        conn.write_all(req.as_bytes()).unwrap();
        conn.flush().unwrap();
        let mut buf = String::new();
        conn.read_to_string(&mut buf).unwrap();
        assert!(buf.starts_with("HTTP/1.1 403"), "响应行: {buf}");
        assert!(buf.contains("bad host"));
        drop(handle_threads);
    }

    /// 起一个测试桥：随机端口，accept 循环与 handle 的接线一致
    /// （读请求 → route → 写响应；dispatch 需要真实 AppHandle，测试里跳过）。
    /// 返回端口号；监听器在线程里存活到进程退出（测试进程短命，无需拆台）。
    fn spawn_test_bridge() -> (u16, std::sync::Arc<()>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        approve_host(format!("127.0.0.1:{port}"));
        let keep = std::sync::Arc::new(());
        let keep2 = keep.clone();
        std::thread::spawn(move || {
            let _keep = keep2;
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _guard = ConnGuard;
                    let mut s = stream;
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
                    let deadline = Instant::now() + Duration::from_secs(30);
                    let Ok(req) = read_request(&mut s, deadline) else { return };
                    let outcome = route(req);
                    let _ = respond(&mut s, outcome.status, &outcome.body);
                });
            }
        });
        (port, keep)
    }

    /// P2-2：token 轮换判定。权威值存在 → 沿用（还原扩展目录由调用方执行）；
    /// 权威缺席但引导/现值在场 → 提升为权威；两处皆空 → 生成新随机值。
    /// 空/纯空白输入与缺席同义。
    #[test]
    fn token_rotation_decision_prefers_authority() {
        use TokenAction::{GenerateFresh, KeepAuthority, PromoteInstalled};
        assert_eq!(
            resolve_token_action(Some("a"), Some("b")),
            KeepAuthority,
            "权威优先于扩展目录现值"
        );
        assert_eq!(resolve_token_action(Some("a"), None), KeepAuthority);
        // 权威值是空白串视为缺席，走提升分支
        assert_eq!(
            resolve_token_action(Some("   "), Some("bundled")),
            PromoteInstalled
        );
        assert_eq!(
            resolve_token_action(None, Some("bundled-or-installed")),
            PromoteInstalled
        );
        assert_eq!(resolve_token_action(None, None), GenerateFresh);
        assert_eq!(resolve_token_action(None, Some("")), GenerateFresh);
    }

    /// 每安装 token 必须是 64 位十六进制（32 随机字节），两次生成不重复。
    /// 非 Windows 平台 random_token 恒 None，本测试随 cfg(windows) 跳过。
    #[test]
    #[cfg(windows)]
    fn random_token_is_64_hex_and_unique() {
        let a = random_token().expect("Windows 上 BCryptGenRandom 应当可用");
        let b = random_token().expect("第二次生成");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次随机不得相同");
    }

    #[test]
    fn bridge_gate_requires_extension_origin() {
        assert!(origin_is_extension(Some("chrome-extension://abcdefghijklmnop")));
        assert!(origin_is_extension(Some("moz-extension://1234-5678")));
        // 网页来源、缺失 Origin、空 Origin、前缀伪装都不过
        assert!(!origin_is_extension(Some("https://evil.example")));
        assert!(!origin_is_extension(Some("http://localhost:3000")));
        assert!(!origin_is_extension(None));
        assert!(!origin_is_extension(Some("")));
        assert!(!origin_is_extension(Some("xchrome-extension://fake")));
    }

    #[test]
    fn host_gate_accepts_only_bridge_address() {
        // 扩展/浏览器实际发出的形态
        assert!(host_is_bridge(Some("127.0.0.1:58943")));
        // 大小写不敏感（host 是不区分大小写的）
        assert!(host_is_bridge(Some("127.0.0.1:58943".to_uppercase().as_str())));
        // DNS rebinding 页面的 Host、缺失、端口混淆都拒
        assert!(!host_is_bridge(Some("evil.example")));
        assert!(!host_is_bridge(Some("localhost:58943")));
        assert!(!host_is_bridge(Some("127.0.0.1:80")));
        assert!(!host_is_bridge(None));
        assert!(!host_is_bridge(Some("")));
    }

    #[test]
    fn add_rate_limit_blocks_flood_within_window() {
        // 用真实时钟的滑动窗口：先清空共享状态再连打 ADD_RATE_MAX 次
        ADD_HITS.lock().unwrap().clear();
        for _ in 0..ADD_RATE_MAX {
            assert!(!add_rate_limited(), "窗口内的前 {ADD_RATE_MAX} 条必须放行");
        }
        assert!(add_rate_limited(), "第 {ADD_RATE_MAX}+1 条必须被限流");
        // 清掉窗口记录后恢复放行（不测真实等待 10s）
        ADD_HITS.lock().unwrap().clear();
        assert!(!add_rate_limited());
    }
}
