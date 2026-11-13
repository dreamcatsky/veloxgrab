# 两项已设计未落地：TLS 指纹 与 下载中改参数

来自 Ghost-Downloader-3 借鉴清单（见 gd3-comparison.md）的另外两项。
写明设计而不直接落代码的原因在各自小节里给出——都不是「想不想」而是
「前置条件没就绪」。

## ① 真实浏览器 TLS 指纹

### 解决什么问题

VeloxGrab 的直链引擎用 reqwest（rustls）。国内网盘/CDN 的风控按 **TLS
ClientHello 指纹**（JA3/JA4）区分客户端：rustls 的指纹一看就不是浏览器，
并发分块请求会被限速、劫持到验证页甚至 403。Ghost-Downloader-3 用
curl-impersonate 系的库伪装真实 Chrome 指纹，同样的分块请求就能过。

### 技术选型

Rust 路线只有 boringssl 系可行：**`wreq`**（原 rquest 的维护延续）——
`Client::builder().impersonate(Chrome)` 一行得到 Chrome 的 TLS+HTTP/2
指纹。备选 `curl` crate + curl-impersonate 预编译库。

### 落地前置条件（本次未实施的原因）

`wreq` 的 boringssl 构建需要 **CMake + LLVM/Clang + NASM**，本机三者全缺
（2026-11-14 实测 `cargo check` 直接失败于 cmake 缺失）。装齐：

```powershell
winget install Kitware.CMake LLVM.LLVMLibrary NASM.NASM
```

（NASM 也可 scoop：`scoop install nasm`。装完重开终端确认 `cmake --version`、
`clang --version`、`nasm -version` 可用。）

### 接入设计（就绪后约一个下午的活）

1. **依赖**：`[target.'cfg(windows)'.dependencies] wreq = { version = "...",
   features = ["impersonate"] }`——与现有 reqwest 并存，不替换。
2. **客户端工厂**：`direct.rs` / `bridge.rs` 的 `build_client` 增加
   `Settings.fingerprint: bool`（默认 false）——true 时用 wreq 构造
   `Client`（`impersonate(Chrome)`），false 走 reqwest 原路。两条路径的
   接口差异（reqwest::Client vs wreq::Client）用一个小 enum 包装：
   `AnyClient { Reqwest(reqwest::Client), Wreq(wreq::Client) }`，暴露
   `get/head/execute`。分块 worker 与 probe 全部走 `AnyClient`。
3. **设置 UI**：直链设置卡加「模拟浏览器 TLS 指纹」开关，文案注明
   「风控站点（网盘/CDN）限速时开启」。
4. **验证**：拿一个已知按 JA3 限速的 CDN 对比开关前后的分块吞吐与状态码。

### 为什么不现在硬上

装 ~2GB 工具链 + 改造 client 工厂 + 全链路 E2E，塞进本次会话只会交付
半成品。设计已定，前置就绪后照做即可。

## ② 下载中改参数（Session 热更新）

### 解决什么问题

直链任务跑到一半发现 Referer 给错了、代理要换、源站切换了 CDN 域名
（URL 变了、内容同一个）——现在只能取消重下。Ghost-Downloader-3 支持
下载中改 URL/headers/proxy 而不丢进度。

### 设计

1. **任务侧**：`Task` 增加 `session: Arc<RwLock<SessionOpts>>`，
   `SessionOpts { referer, user_agent, extra_headers, proxy_url, url }`。
   分块 worker 每次发起 Range 请求前 `read()` 一次快照——热更新天然生效
   于「下一个请求」，进行中的请求自然结束，无需中断任何连接。
2. **改 URL**：只允许换**同内容的源**（协议不变、路径由用户确认）。已下
   的分块偏移与源无关（Range 布局固定在 `.veloxgrab-part`），换 URL 后
   worker 对新源发同样的 Range 续拉——这正是「进度不丢」的原理。
3. **UI**：任务右键「编辑会话参数」对话框（编辑任务对话框的运行中变体），
   提交后 `update_session` 命令写 `session`；各 worker 在 ≤N 秒内自然
   切换。UI 显示「下次请求生效」。
4. **代理热切**：proxy 是 client 级构造参数——热切换需要 per-request
   proxy（reqwest 支持 `RequestBuilder` 上没有 proxy；做法是维护 2~3 个
   预建 client（直连/系统代理/自定义代理），`AnyClient` 快照里带索引，
   worker 按快照选 client。这也是 wreq 接入后 enum 包装的另一个收益。
5. **测试**：本地起一个会校验 Referer/UA 的假服务器：任务跑一半改
   Referer → 断言后续请求带头新值、任务完成、.veloxgrab-part 偏移连续。

### 为什么没直接做

`direct.rs` 的 worker 结构是「启动时快照全部参数」，热更新要动 worker
主循环与 client 构造的全生命周期——是一个需要完整 E2E（真实断网续传、
代理切换）的独立特性，不适合夹在本次五项里赶工。设计已定稿，随时可做。
