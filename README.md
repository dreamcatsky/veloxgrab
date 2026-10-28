# VeloxGrab

一个基于 **yt-dlp** 的桌面视频下载器 + 通用多线程 HTTP 下载器，用 Tauri 2 + Rust 构建。
（别名 VeloGrab——两个拼写都指本项目；本项目曾用名 VGrab。）

> 本项目是一个独立的开源实现，仅通过调用 yt-dlp 的公共命令行接口工作。
> 请遵守目标网站的服务条款与著作权法律，仅下载你有权获取的内容。

## 特性

### 媒体站点（走 yt-dlp）

- **链接解析** —— 粘贴 URL 自动抓取标题、时长、封面与全部可用清晰度
- **清晰度选择** —— 按分辨率/编码/容器列出可选格式，显示预估体积
- **多任务队列** —— 可配置并发数（1-8），自动排队调度
- **实时进度** —— 百分比、已下载量、速度、剩余时间
- **取消与清理** —— 运行中可取消（杀进程树，不留残余子进程）
- **音视频合并** —— 当选择的视频流不含音轨时，自动拼接最佳音轨并用 ffmpeg 合并为 MP4
- **Cookie 支持** —— 可读取 Chrome/Edge/Firefox/Brave 登录态，或指定 Netscape 格式 cookies.txt。
  **按需降级**：先不带 Cookie 解析，失败才带 Cookie 重试（原因见下方「Cookie 别无条件带上」）
- **深色 / 浅色主题** —— 设置面板里可切换深色、浅色或跟随系统，**改完立即生效**。
  主题在首帧之前就落定，切换系统配色时不会闪一下（见下方「主题为什么不闪」）
- **设置持久化** —— 下载目录、并发数、分片数、Cookie 来源、合并策略
- **单实例运行** —— 重复启动会把已有窗口拉到前台
- **应用内更新** —— 启动后自动查 GitHub Releases，发现新版本弹升级框：
  下载 → 静默安装 → 自动重启，不用去网页重下安装包（菜单「帮助 → 检查更新…」可手动触发）
- **浏览器集成** —— `veloxgrab://` 协议 + Chrome/Edge 扩展，网页里点一下链接就能下载
  （见下方「浏览器集成」）

### 通用直链（走内置 HTTP 引擎，不依赖 yt-dlp）

- **自动分流** —— 同一个输入框：识别为媒体站走 yt-dlp，其余按 HTTP 直链处理
- **多线程分块** —— 用 `Range` 请求把文件切成 N 段并行拉取，1/2/4/8/16/32/64 线程可选
- **断点续传** —— 每段已写偏移周期性落盘到临时文件旁的 `.veloxgrab-part`；
  即使进程被强杀，下次启动也能从断点继续。临时文件带任务 id
  （`<文件名>.veloxgrab-downloading.<id>`），同名任务并发也不互相覆盖
- **不支持 Range 自动降级** —— 服务端若忽略 `Range` 头，自动退回单流顺序下载
- **体积/类型探测** —— 显示文件大小、MIME 类型，可自定义保存文件名

### 手动与批量

- **手动新建下载** —— 有地址但不想先解析时，直接填 URL + 文件名 + 目录 + 线程数入队
- **下载记录** —— 所有终态任务（完成/失败/取消）写入 `history.json`，重启不丢；
  支持打开所在文件夹、重新下载、单条删除、清空
- **每个任务单独指定目录** —— `Task.save_dir` 优先于全局默认目录，两者都为空才报错

探测 Range 支持时始终发一次 `Range: bytes=0-0` 试探请求：本地 HEAD 返回 200
不代表支持分块，只有真正拿到 `206` + `Content-Range` 才判定可多线程。

## 依赖

运行时需要两个外部程序：

| 依赖 | 用途 | 获取方式 |
|---|---|---|
| `yt-dlp.exe` | 解析与下载核心 | <https://github.com/yt-dlp/yt-dlp/releases> |
| `ffmpeg.exe` | 音视频合并/转码（可选但强烈建议） | <https://ffmpeg.org/download.html> |

**关于打包**：`yt-dlp.exe` 会作为 Tauri 资源随安装包一起分发，安装后位于
`%LOCALAPPDATA%\VeloxGrab\bin\yt-dlp.exe`。`ffmpeg.exe` 默认不打包（体积较大），
请自行安装到 PATH，或放到程序目录 / 设置环境变量 `VELOXGRAB_FFMPEG`。

查找顺序（yt-dlp）：

1. 环境变量 `VELOXGRAB_YTDLP` 指向的文件
2. Tauri 资源目录下的 `bin/yt-dlp.exe` 或 `yt-dlp.exe`（安装版走这条）
3. 程序所在目录的 `yt-dlp.exe` 或 `bin/yt-dlp.exe`
4. `PATH` 中的 `yt-dlp`

ffmpeg 同理，环境变量为 `VELOXGRAB_FFMPEG`。

> 若缺少依赖，程序启动时顶栏指示灯会变红并给出明确提示，不会静默失败。

## 构建

```powershell
# 安装前端/打包工具链
npm install

# 开发模式（热重载）
npm run dev

# 生产构建（产出 NSIS 安装包）
npm run build
```

构建产物位于 `src-tauri/target/release/bundle/nsis/`。

### 首次构建注意（NSIS 打包）

Tauri 打包 NSIS 时会从 GitHub 下载 NSIS 工具链。若网络受限导致
`failed to bundle project: timeout: global`，可手动准备缓存：

1. 下载 <https://github.com/tauri-apps/binary-releases/releases/download/nsis-3.13/nsis-3.13.zip>
2. **把 `nsis-3.13/` 里的内容平铺**到 `%LOCALAPPDATA%\tauri\NSIS\`
   （即 `%LOCALAPPDATA%\tauri\NSIS\Bin\makensis.exe`，**不要**保留 `nsis-3.13` 这一层子目录）
3. 下载 <https://github.com/tauri-apps/nsis-tauri-utils/releases/download/nsis_tauri_utils-v0.5.3/nsis_tauri_utils.dll>
   放到 `%LOCALAPPDATA%\tauri\NSIS\Plugins\x86-unicode\additional\`

Tauri 会按 12 项清单逐个校验，缺任何一项都会删除整个目录重新下载。

### 环境要求

- Rust 1.77+
- Node.js 18+
- MSVC 工具链（Windows）
- WebView2 Runtime（Windows 10/11 通常已内置）

## 主题为什么不闪

界面支持**深色 / 浅色 / 跟随系统**三态，在「设置 → 外观主题」里切换，改完**立即生效**，
不需要重启。实现上有两个不显然的地方：

### 1. 主题在首帧之前就落定

如果等 `app.js` 的 `boot()` 拿到 `get_settings` 再上色，窗口会先按默认深色渲染一帧，
然后才跳成用户要的颜色——这个闪烁在冷启动时特别明显（WebView2 首帧要一秒以上）。

所以主题的落地被拆成两步：

- `src/theme.js` 是 `<head>` 里的**同步**脚本（不 `defer`），在首帧之前读
  `localStorage` 缓存并设置 `<html data-theme="...">`
- `app.js` 的 `boot()` 拿到 `settings.json` 里的真值后再纠正一次

代价是首帧用的是缓存值，可能与磁盘上的真值短暂不一致（换过机器、清过缓存）。
不一致最多持续到 `boot()` 结束，且纠正后立即写回缓存。这是「宁可错一帧也不能闪一帧」的取舍。

### 2. 浅色不是把深色反转

深色版的成功色 `#3ecf8e`（亮绿）在白底上几乎看不见，直接拿来做浅色主题会得到
一个「按钮存在但看不清」的结果。浅色版四个语义色都按 4.5:1 对比度重新选过：

| 变量 | 深色 | 浅色 |
| --- | --- | --- |
| `--ok` | `#3ecf8e` | `#12855c` |
| `--warn` | `#e0a33e` | `#9a6208` |
| `--err` | `#e5544b` | `#cf2f24` |
| `--accent` | `#4b8bf5` | `#2f6fe0` |

阴影也从纯黑 `rgba(0,0,0,.38)` 换成带蓝调的低透明深色 `rgba(16,24,40,.14)`——
纯黑阴影落在浅色背景上会显脏。

### 3. 颜色全部走变量

样式表里不再有任何硬编码颜色，只引用 `var(--*)`。加新主题时只改
`styles.css` 顶部的变量块，不用去几十条规则里翻 `rgba`。派生色（`--accent-soft`、
`--warn-tint`、`--shadow`、`--scrim` 等）也整组跟着主题切换。

### 4. 改主题不会误伤其它设置

主题走独立的 `set_theme` 命令，**不复用 `save_settings`**：后者会把前端表单里
**当前**的每一项都写一遍，包括正被编辑但还没保存的下载目录、代理密码。
用户只是换个颜色，不该顺手把半截表单落盘。

## 架构

```
┌──────────────────────────────────────────────────────┐
│  前端 (WebView2)                                      │
│  index.html / app.js / styles.css / theme.js          │
│  invoke('probe_auto') / invoke('enqueue_download')    │
└──────────────────────┬───────────────────────────────┘
                       │ Tauri IPC
┌──────────────────────▼───────────────────────────────┐
│  Rust 后端                                            │
│                                                       │
│  commands.rs  probe_auto 分流 + 队列调度（并发泵）    │
│  ytdlp.rs     解析（yt-dlp -J）与格式枚举             │
│  engine.rs    子进程管理、进度解析、取消、直链任务     │
│  direct.rs    内置 HTTP 引擎：Range 探测/分块/续传     │
│  deeplink.rs  veloxgrab:// 协议解析与注册表写入           │
│  history.rs   下载记录持久化                         │
│  settings.rs  配置读写与边界校验                       │
└───────────┬───────────────────────────┬───────────────┘
            │ std::process              │ reqwest + tokio
┌───────────▼───────────────┐  ┌────────▼───────────────┐
│  yt-dlp（+ ffmpeg 合并）   │  │  任意 HTTP/HTTPS 服务器  │
└───────────────────────────┘  └────────────────────────┘
```

外部入口（浏览器扩展、地址栏、命令行）通过 `veloxgrab://` 进入，由
`deeplink.rs` 解析成 `LinkRequest` 压入 `AppState.pending_links`，再以
`external-link` 事件唤醒前端；前端 `take_pending_deep_links()` 取走后，
与手动新建下载共用同一条 `create_direct_task` 路径入队。

关键设计点：

- **自动分流**：`probe_auto` 先查内置媒体站名单（抖音/YouTube/B站等 → yt-dlp `-J`）；
  非名单链接先试 HTTP 直链探测，失败再兜底试 yt-dlp。返回 `kind: video | direct | error`，
  前端据此渲染视频卡片或直链卡片。
- **进度来源**：给 yt-dlp 传 `--newline --progress`，使每一行都是一个完整进度快照，前端逐行解析即可，无需缓冲多行。
  直链任务则由独立的进度线程每 300ms 读取共享计数器上报。
- **输出路径**：用 `--print after_move:filepath` 让 yt-dlp 回传真正的最终文件路径（合并后扩展名可能变化），比前端猜测文件名可靠。
- **取消语义**：Windows 上用 `taskkill /T /F` 终止整棵进程树，避免 ffmpeg 子进程成为孤儿。
  `CancelToken` 同时持有一个共享 `AtomicBool`，`cancel()` 会一并通知直链下载的各分块。
- **音轨补齐**：探测阶段记录最佳纯音频流 `format_id`，若用户选的视频格式 `acodec == none`，则拼成 `<video>+<audio>` 交给 yt-dlp 合并。
- **格式排序**：分辨率高度降序 → 编码兼容性（H264 > H265 > AV1）→ 体积降序。同分辨率下默认推荐 H.264（兼容性最好），想省空间可选 H.265/AV1（同画质体积约为 H.264 的 1/3 ~ 1/4）。
- **分块续传**：`PartMeta` 记录每段的 `segments_done`，下载中每 1 秒落盘一次快照。
  只依赖"正常返回时写盘"是不够的——进程被强杀时那条路径根本执行不到，所以必须有周期快照。
- **独立停止信号**：进度线程退出用的是单独的 `stop_progress`，**不能复用用户的 cancel 标志**，
  否则正常下载完毕会被误判成"已取消"。

## 浏览器集成

网页里看到一个下载链接，点一下就下——这是 IDM 的核心体验，VeloxGrab 用
**`veloxgrab://` 协议 + Chrome/Edge 扩展**做到。

```
网页/邮件里的链接
   ↓ 扩展右键菜单或 popup
veloxgrab://add?url=...&name=...&dir=...&threads=...
   ↓ 浏览器按注册表把它交给 veloxgrab.exe
single_instance（已在跑）或 argv（冷启动）
   ↓ deeplink.rs 解析
前端 external-link 事件 → invoke new_download → 入队
```

### 协议注册

首次启动时 `deeplink::ensure_protocol_registered()` 写
`HKCU\Software\Classes\veloxgrab`：

| 值 | 内容 |
| --- | --- |
| `(默认)` | `URL:VeloxGrab Download Link` |
| `URL Protocol` | 空串（有这个值 Windows 才认它是协议） |
| `DefaultIcon` | veloxgrab.exe 路径 |
| `shell\open\command` | `"<安装目录>\veloxgrab.exe" "%1"` |

写的是 **HKCU**，不需要管理员权限。已指向自己时跳过，不会每次启动都重写。

**为什么不用 `tauri-plugin-deep-link`**：那个 API 在某些环境下会把自身目标写成
dev 期的 exe 路径，而安装后路径是 `%LOCALAPPDATA%\VeloxGrab\veloxgrab.exe`——
写错了协议关联静默失效，用户点了链接什么都不会发生，且没有任何报错。
自写注册表能保证指向的是 `current_exe()`，也就是真正在跑的这个文件。

### 扩展

位于 `extension/`，随安装包分发到 `%LOCALAPPDATA%\VeloxGrab\extension\`。
在 `chrome://extensions` 里开「开发者模式」→「加载已解压的扩展程序」选该目录即可。

- 右键链接 → 用 VeloxGrab 下载此链接
- 右键图片/视频/音频 → 用 VeloxGrab 下载此媒体
- 点工具栏图标 → 弹窗，可改目录与线程数，并列出当前页的媒体一键发送

**已知限制**：Chrome/Edge 不允许扩展随意拉起自定义协议。用户手势内的
`chrome.tabs.update` 是目前最可靠的一条路，但 `chrome://`、Web Store
等页面仍会被静默拦截。拦截时扩展会自动降级为「复制链接到剪贴板 + 系统通知」。
要彻底绕开需要 Native Messaging host（额外的 manifest + 注册表安装步骤），
默认不做。

### 支持的链接形态

```text
veloxgrab://add?url=<http链接>&name=<文件名>&dir=<目录>&threads=<n>
veloxgrab://add/?url=<...>                        # add 后面多一个斜杠也认
veloxgrab://https://example.com/file.zip         # 整个 body 就是目标 URL
```

参数名做了兼容（`url`/`u`、`name`/`filename`/`f`、`dir`/`d`、`threads`/`t`），
值一律 `urlencoding` 解码，`threads` 超过 64 会被 clamp。只放行 `http`/`https`
目标——`file://`、`javascript:` 这类必须挡在外面。

**文件名来源的优先级**：显式 `name` 参数 > `Content-Disposition` > URL 末段 > MIME 类型。
只有 URL 末段带扩展名时才优先用它——`https://host/download?id=123` 这种名字
一律走 `Content-Disposition`。

已知小瑕疵：少数服务器只在 `200` 响应上带 `Content-Disposition`，下载走的是
分块 `206`，这时拿不到头，只能靠 URL 末段或 MIME 推断（实测某测试服务器会把
`/download` 存成 `download.bin`）。真实站点普遍在 `206` 上也带，影响有限。

**两个必须知道的不对称**：

1. `add` 后面带斜杠也要认。浏览器按 URL 规则规范化 `veloxgrab://add?...` 时会补一个
   `/`，path 从 `add` 变成 `add/`。不兼容的话网页里点扩展传来的链接 100% 失败，
   报「不支持的动作：add/」。
2. 扩展拼链接时**不能**把 `URLSearchParams.toString()` 的结果直接当 query 用。
   它会把 `:` 编码成 `%3A`，浏览器解析 `veloxgrab://` 时会把 scheme 的分隔语义吃掉
   ——实测 `veloxgrab://https://a.com/x.zip` 会被规范化成 `veloxgrab://https//a.com/x.zip`，
   裸链接形态整个废掉。手工 `encodeURIComponent` 逐项拼才安全。

### 链接投递：为什么是「先存后取」而不是「直接发事件」

Rust 侧解析完链接后，**先把结果压进 `AppState.pending_links`，再发一个空载荷的
`external-link` 事件当门铃**；前端在 `boot()` 结束和每次门铃响起时 invoke
`take_pending_deep_links()` 把队列一次取空（取走即清空）。

早期实现是「解析完直接 `app.emit` 带 payload」，冷启动路径上 100% 丢链接：

- Tauri 的 `emit` 对还没有 `listen` 的 webview 是**静默丢弃**，不排队、不回放。
- 冷启动时 webview2 首帧要一秒以上，而 `emit` 在 `setup` 后只等了 300ms。
- 症状极具误导性：进程命令行里明明带着 `veloxgrab://add/?url=…`（`Win32_Process.CommandLine`
  可见），任务列表却一条都没有，也没有任何报错。

队列方案把「数据面」和「通知面」拆开了：事件丢了只影响及时性，不影响正确性。
门铃可能连响多次（一次 drain 已清空队列），所以再加一个 `drainingLinks` 布尔锁
防止并发 drain 同一批。

## 测试

仓库内带六个自检程序，对着任意支持 Range 的地址跑端到端验证：

```bash
cd src-tauri

# 1. 单站点探测（yt-dlp 链路）
cargo run --release --example probe_test -- "https://www.bilibili.com/video/BV1GJ411x7h7"

# 2. 直链全量下载 + 逐字节校验 + 断点续传
#    第三个参数是期望 SHA256；留空则只校验大小与续传一致性
cargo run --release --example direct_test -- "https://example.com/file.zip" 4 /tmp/out

# 3. 取消响应速度（取消后多久真正停下）
cargo run --release --example cancel_test -- "https://example.com/big.bin" 4000 4

# 4. 断点续传正确性（先取消一次，再续传比对）
cargo run --release --example resume_test -- "https://example.com/big.bin" 4500

# 5. Cookie 降级（浏览器 Cookie 读不到时，公开视频仍应解析成功）
cargo run --release --example cookie_test -- "https://www.youtube.com/watch?v=..."

# 6. 深链解析回归（一行一条 veloxgrab:// 链接，打印解析结果）
cargo run --release --example link_check -- cases.txt
```

`direct_test` 会打印探测结果（是否支持 Range、总大小、建议线程数），下载完成后
逐字节校验内容，并额外跑一轮“删掉一部分再续传”的一致性检查。

`cargo test --lib` 覆盖 69 个单测：URL 分流、Range 判据、分块切分、续传累计值语义、
文件名清洗、进度行解析、音频打分、陈旧清理、历史只收终态、深链解析全部形态
（含 `add` 带斜杠、裸 URL 形态、参数别名、非 http 目标拒绝、threads clamp）、
目录闸门（词法折叠、越界判定、可信集、可删文件校验）、代码页解码（真实 GBK 字节
回归）、DPAPI 封装与旧明文兼容、本地桥 Origin 闸门、安装链引号剥离。

### 协议投递的手工验证

协议拉起这条链路没有单测能覆盖，必须真跑。装好之后按这三种形态各点一次：

```powershell
# 关闭程序后执行 → 冷启动路径
Start-Process 'veloxgrab://add/?url=http%3A%2F%2F127.0.0.1%3A8000%2Ff.bin&name=a.bin&threads=4'

# 程序已在运行时执行 → 热启动路径（single_instance 回调）
Start-Process 'veloxgrab://add/?url=http%3A%2F%2F127.0.0.1%3A8000%2Ff.bin&name=b.bin&threads=8'

# 裸链接形态
Start-Process 'veloxgrab://http://127.0.0.1:8000/f.bin'
```

三种形态都应该在「记录」页出现一条 `done` 记录，且下载文件与源 MD5 一致。
如果命令行里能看到 `veloxgrab://…` 但记录页空着，说明卡在**投递**环节而不是协议注册
——先查 `take_pending_deep_links` 有没有被前端调用到（见上一节）。

### 一个容易踩的坑：探测请求别串行等 HEAD

`probe_direct` 早期是「先 `await` HEAD，拿到结果再发 1 字节 Range」。这对普通
服务器没问题，但遇到响应慢的服务器会很惨：实测本地限速服务对 `HEAD` 要
**16.5 秒**才回，而 1 字节 `Range` 只要 1.1 秒。串行等 HEAD 会让整个探测卡十
几秒，用户点「取消」后看起来毫无反应（取消标志早置位了，但流程还堵在 HEAD 上，
压根没进下载阶段）。

现在 HEAD 与 Range 并发发出，且 HEAD 带 3 秒上限，超时就直接放弃——它只是
锦上添花的元信息，`Content-Range` / `Content-Length` 足够判断大小和续传支持。

### Cookie 别无条件带上

用户反馈「视频无法解析下载地址」，错误是：

```
ERROR: Could not copy Chrome cookie database. See https://github.com/yt-dlp/yt-dlp/issues/7271
ERROR: Failed to decrypt with DPAPI. See https://github.com/yt-dlp/yt-dlp/issues/10927
```

看起来像「Cookie 读不出来」，实际是**本来根本不需要 Cookie**。同一个 YouTube
视频不加任何 Cookie 能正常解析，一旦设置里选了「从浏览器导入」，早期实现每次
都强制加 `--cookies-from-browser`，于是被浏览器的 Cookie 库卡死。

根因：Chrome/Edge 从 **127 版**起启用 App-Bound Encryption（`Local State` 里的
`os_crypt.app_bound_encrypted_key` 带 `APPB` 前缀），Cookie 不再是纯 DPAPI 可解，
yt-dlp 拿不到也就报上面两个错。另注意报错文案可能写「Chrome」但其实来自 Edge——
yt-dlp 把 Chromium 系浏览器统一按 Chrome 的库路径处理。

修法是把 Cookie 从「默认开启」改成「按需降级」：

1. `probe` 先**不带 Cookie** 跑一次，覆盖绝大多数公开视频；
2. 失败了且用户配了 Cookie，才带 Cookie 重试；
3. 两轮都失败时，把**两轮的错误一起**返回，否则用户只看到 Cookie 的错误，
   会误以为问题出在 Cookie 上。

`run_download` 同理：先不带 Cookie 下载，失败且非用户取消时才带 Cookie 重试一次
（重试前重置进度，避免界面停在旧错误上）。这样抖音/B站这类需要登录态的场景
仍能靠 Cookie 兜底，而公开视频不会被浏览器加密问题连累。

### 取消语义的两个必要条件

1. **分块要能看到取消标志**。分块每轮循环直接读 `Arc<AtomicBool>`，不去抢
   `counters` 那把锁——早期版本为了看一眼 `stop` 字段每块都去 lock 一次，
   四个分块加进度线程争一把锁，把 2.5 秒的下载拖成 11.5 秒。
2. **断点记录只能变大不能变小**。分块退出时写 `segments_done` 必须取
   `max(已有值, 本次写入)`。直接用 `*d = written` 赋值的话，取消恰好发生在
   循环刚入口时会把进度线程记好的几 MB 覆盖成 0，续传等于从头再来。

## 安全姿态（已知边界与取舍）

2026-10 全项目审查后的定稿姿态。已修项散见各模块注释，这里只记录**有意保留**
的边界，避免后来者把「没修」当成「漏了」，也避免把「防线」建在不成立的假设上：

- **威胁模型**：桌面单机应用，无服务端、无鉴权。不可信输入 = 深链/扩展传来的
  URL 与文件名与目录、HTTP 响应头与响应体、磁盘上的 settings/history/part 文件
  （手改或半截写）、以及**被 XSS 污染的 webview**——后者是命令层的假想敌，所以
  命令参数一律按不可信对待（目录闸门、URL 闸门、断点互证），而前端靠
  「不可信文本一律 textContent」纪律 + CSP 纵深守住注入面。
- **目录闸门**（save_dir.rs）：默认下载目录子树内直接放行；越界目录首次弹
  **OS 级原生确认框**（不是 webview 弹窗——XSS 下前端的确认不可信），同意后
  记入信任集。**刻意放行局域网/回环**（下载 NAS 是正当用途）；「域名解析到
  内网」的 true SSRF 不做 DNS 预检——有 TOCTOU 且误伤内网用户。重定向逐跳
  过同一道闸（含拦 https→http 降级、云元数据字面量）。
- **更新链维持「信任 GitHub 仓库」姿态，未做验签**（产品决定，2026-10-23）：
  下载源锁死 github 官方三域 + TLS 校验开启 + 大小校验，信任边界与「人去网页
  上点下载」一致；代价是**仓库账号被盗 = 可推送任意安装包**。要升级这个姿态
  就引入 minisign（tauri-plugin-updater）或 release 附带 SHA-256 并校验。
  安装链的 bat 路径做了引号剥离（合法文件名含 `"` 不致注入命令）。
- **机密存储**：hf_token / proxy_pass 落盘前过 DPAPI（CryptProtectData，
  用户+机器绑定），settings.json 里只有 `dpapi1:<hex>`；加密失败退回明文
  （保存失败丢整个配置比不加密更糟），解密失败按未配置处理、绝不把密文漏进
  请求头。不防同用户身份的进程（DPAPI 公开边界），也不防 XSS（那是 CSP 的活）。
- **本地桥**（127.0.0.1:58943）：Origin 缺失即 403——只认浏览器扩展的
  非简单请求；本机进程的利用面维持接受（它能直接拉 veloxgrab://，拦了没意义）。
- 已知低危未修项：CSP `style-src 'unsafe-inline'` 保留（前端大量动态 style
  赋值，收益配不上改造成本）；错误回显偶含绝对路径。
- CSP 的 `script-src` 已收紧到 `'self'`：Tauri 对**自己注入**的初始化脚本自动
  计算并附加 hash（`dangerous_disable_asset_csp_modification` 保持默认），项目
  自有 HTML 无内联脚本/内联事件，去 unsafe-inline 零影响。若未来引入第三方
  内联片段，宁可加显式 hash，不要再开 `unsafe-inline`。

## 许可

MIT
