# Ghost-Downloader-3 对照与演进路线

> 基线：Ghost-Downloader-3 `main`（2026-10 浅克隆，`app/` + `features/`）／ VeloxGrab 1.2.8。
> 本文只做差异分析与实现路径，不含代码改动。

---

## 0. 一句话结论

GD3 是「协议全家桶 + 插件包体系」的通用下载器，VeloxGrab 是「媒体站解析 + 直链多线程」的窄而深工具。
**真正值得抄的不是它的协议数量，而是它把「下载器日常」做圆了的那一圈**：分类目录、同名策略、
剪贴板监听、关闭行为、完成后动作、任务编辑、请求头预设。这些不碰架构，却直接决定「能不能天天用」。

不建议抄的：BT / eD2k / FTP 引擎、插件包体系、aria2 全量 RPC、8 语言全量翻译。
理由见 §4。

---

## 1. 定位差异

| | Ghost-Downloader-3 | VeloxGrab |
|---|---|---|
| 形态 | Python + PySide6（qfluentwidgets），多平台 | Rust + Tauri 2，单 exe，仅 Windows |
| 扩展方式 | `features/*_pack` 动态加载（manifest.toml + pack.py） | 编译期内置：`kind` 分流到 yt-dlp / direct |
| 主战场 | 什么都能下（HTTP/BT/eD2k/FTP/M3U8/DASH/站点包） | 视频站（yt-dlp）+ 直链（自研 Range 引擎） |
| 调度模型 | `Task` → `TaskStep` 多步流水（解析/下载/合并/做种） | `Task` 单步，视频走子进程、直链走线程池 |
| 安全姿态 | 默认关 SSL 校验、可模拟客户端指纹 | 只放行 http(s)、目录闸门、DPAPI、摘要校验 |

**含义**：GD3 的很多设置项是为「什么协议都接」服务的（SSL 校验、指纹、系统 DNS、文件关联）。
VeloxGrab 只做 http(s) 时，这些项的价值要打折——不能照着设置页 1:1 抄。

---

## 2. 能力矩阵

### 2.1 引擎与调度

| 能力 | GD3 | VeloxGrab | 差距 |
|---|---|---|---|
| 最大并发任务 | 1–10（默认 3） | 1–8（默认 3） | 小 |
| 分块线程 | 1–256（默认 8） | 1–64（默认 8） | 中 |
| 自动提速 | 有（实时测各线程效率加线程） | 无 | **有** |
| 线程再分配 | 有（`maxReassignSize`，线程提前完成就重分剩余量） | 无（分块固定，慢线程拖尾） | **有** |
| Range 降级 | 有 | 有（206 判定 + 单流退化） | 持平 |
| 断点续传 | 有（分片元数据） | 有（`PartMeta` + 一致性校验） | 持平，我方校验更严 |
| 全局限速 | 有（开关 + 1 KiB–100 MiB/s） | 有（KB/s，0=不限，无开关） | 小 |
| 单任务限速 | 无（全局） | 无 | 持平 |
| 保留 Last-Modified | 有 | 无 | 小 |
| SSL 校验开关 | 有（默认**关**） | 固定校验 | 不建议抄（见 §4） |
| 系统 DNS | 有 | 无（reqwest 默认） | 小 |
| 磁盘空间预检 | 有（`diskSpaceInsufficient`） | 无 | 小 |
| 完成后校验和 | 有（md5/sha1/sha256/sha512，可取消） | 无（仅更新包校验） | 中 |

### 2.2 来源与协议

| 能力 | GD3 | VeloxGrab |
|---|---|---|
| HTTP(S) 直链 | 有 | 有 |
| 站点解析 | yt-dlp / bili / github / huggingface / jack_yao 五个 pack | yt-dlp（单一入口） |
| HLS / DASH 原生 | 有（m3u8 pack + N_m3u8DL-RE） | 交给 yt-dlp |
| FTP | 有 | 无 |
| BitTorrent（含做种） | 有（librqbit 风格 session） | 无 |
| eD2k | 有 | 无 |
| 播放列表 / 多文件 | 有（TaskFile 多选） | **无**（`--no-playlist` 显式禁用） |
| 任务草稿（先选文件再入队） | 有 | 无 |

### 2.3 任务生命周期

| 能力 | GD3 | VeloxGrab |
|---|---|---|
| 队列 | 有，可置顶（`moveToFront`） | FIFO，无排序/置顶 |
| 暂停/继续/重下 | 有 | 有 |
| 编辑任务（线程/目录/文件名/头） | 有（`task_service.edit`） | 无（只能删了重建） |
| 同名冲突 | keepBoth / overwrite / ask | keepBoth（直链递增改名）/ skip（视频），可切 overwrite（仅本程序同 URL 产物） |
| 分类目录路由 | 有（扩展名 → `{default}/Video` 等 8 类） | 有（可选，默认关闭；五类预设可编辑，手选目录优先） |
| 删除时删文件 | 有（开关） | 有（`delete_files` 参数） |
| 文件被外部改动监控 | 有（`fileWatcher`） | 无 |
| 崩溃恢复 | 有（活跃态落盘为 paused） | 有（同上） |
| 完成后动作（关机/休眠/重启/打开文件） | 有（`Plan`） | 无 |
| 系统通知 | 有 | 无（只有托盘） |

### 2.4 网络身份与请求

| 能力 | GD3 | VeloxGrab |
|---|---|---|
| 代理 | Auto / Off / 自定义 + 最近列表 | 手动 http/socks5 + 认证 |
| 自定义请求头 | 有（多套预设，可切换） | 有（多套预设，按 host 作用域；危险头白名单过滤） |
| User-Agent | 可配（含身份预设按 host 匹配） | 可配（请求头预设里的 `User-Agent`，直链与 yt-dlp 都生效） |
| 客户端指纹模拟 | 有（`clientProfile`） | 无 |
| 每 host 凭证 | 身份预设按 host 注入 UA | 仅 HuggingFace `Authorization`（作用域校验） |
| Cookie | 无（靠请求头预设塞） | 浏览器读取 / Netscape 文件，**按需降级** |

> 我方 Cookie「先不带、失败再带」是 GD3 没有的；这条别退回去。

### 2.5 系统集成

| 能力 | GD3 | VeloxGrab |
|---|---|---|
| 浏览器扩展 | 资源嗅探 + WS 推流 + 配对令牌 + 端口可配 + 草稿模式 + 商店链接/CRX 导出 | 右键/协议 + 回环 HTTP 桥（Origin 白名单，端口写死 58943） |
| URL Scheme | 有（可开关注册） | 有（`veloxgrab://`，自写注册表） |
| aria2 RPC | 有（子集：getVersion/getGlobalOption/addUri） | 无 |
| CLI | `ghd`（probe/add/tasks/pause/start/remove/wait） | 无（只有 exe 参数消费深链） |
| 开机自启 | 有 | 有（HKCU Run） |
| 关闭行为 | 询问 / 后台 / 退出 | 固定「隐藏到托盘」 |
| 剪贴板监听 | 有（可按 pack 忽略） | 只有手动「从剪贴板添加」 |
| 单实例 | 有 | 有 |
| 应用内更新 | 有 | 有（含 sha256 摘要校验） |
| 便携模式 | 有（Portable / 用户模式迁移） | 无（固定 `%APPDATA%`） |
| 文件类型关联 | 有 | 无 |

### 2.6 界面与设置

| 能力 | GD3 | VeloxGrab |
|---|---|---|
| 主题 | 深/浅/跟随 | 深/浅/跟随（首帧前落定，无闪） |
| 语言 | 8 种（含繁体/粤语） | 中 / 英 |
| 窗口材质 | Acrylic / Mica / MicaAlt / Aero / None | 无（纯 CSS） |
| 界面缩放 | 0–5 | 无（跟随系统 DPI） |
| 设置分组折叠 | 有（可记住展开状态与顺序） | 无（单列长表单） |
| 首次引导 OOBE | 有 | 无 |
| 分类树 / 筛选 | 有（分类即目录） | 有（按 `kind` 过滤，不落目录） |

---

## 3. 需要补的功能（分级）

排序依据：**用户每天会碰到的 > 锦上添花的 > 要新引擎的**。
工作量按「熟悉本仓库的前提下」估，单位人日。

### P0 · 体验补齐（不动架构，建议下个版本吃掉）

#### P0-1 同名文件策略（keepBoth / overwrite / ask）
- **现状**：`direct.rs::unique_path()` 固定递增避让；yt-dlp 侧未显式传 `--no-overwrites`。
- **GD3**：三档可选，默认 keepBoth。
- **改法**：`settings.rs` 加 `name_conflict: String`（`keep_both|overwrite|ask`）；
  `direct.rs` 把 `unique_path` 换成 `resolve_conflict(path, policy)`；
  覆盖前必须校验「目标是本任务产物或用户已确认」，避免误删无关文件。
- **ask 档的代价**：直链下载在 Rust 侧同步执行，要询问就得「暂停 → 发事件 → 前端弹窗 →
  带选择重入」，等于给 `run_direct_download` 加一个可等待的决策点。
  **建议先只做 keepBoth / overwrite 两档，ask 放 P1。**
- **yt-dlp 侧**：它原生只有「跳过 / 覆盖」，没有 keepBoth 重命名。
  视频任务建议只提供 `skip`（默认，现状）与 `overwrite`（显式开启），不承诺 keepBoth。
- **风险**：覆盖是破坏性操作，必须二次确认 + 只对「同 URL 同产物」生效。
- **验收**：单测三档；同 URL 连下两次；目标被其它程序占用时不崩。
- **工作量**：1.5 天（不含 ask）。
- **已落地（批次 B）**：`Settings.name_conflict` 只做 `keep_both`（默认）与
  `overwrite` 两档，ask 按建议留给 P1。直链在 `finish_download` 覆盖前用
  `history::is_known_output` 校验「同一 URL + canonicalize 后同一路径」，
  不满足就退回递增改名；yt-dlp 仅在 `history::has_done_output_in_dir` 命中时
  追加 `--force-overwrites`，否则保持默认跳过。设置页提供下拉框，覆盖模式
  不会碰来源不明的同名文件。

#### P0-2 分类目录路由
- **现状**：只有全局 `download_dir` + 单任务 `save_dir`；侧栏「分类」只是按 `kind` 过滤视图。
- **GD3**：扩展名 → 8 个预设类目 → `{default}/Video`、`{default}/Audio`…，可在设置里改扩展名与目录。
- **改法**：
  1. `settings.rs` 加 `categories: Vec<CategoryRule>`（`name` / `extensions` / `folder` / `enabled`），
     默认给 5 类（对齐现有侧栏：视频/音乐/程序/压缩包/文档）。
  2. 解析落点集中在 `engine.rs::resolve_task_dir()`——它已经是唯一入口，
     在这里按最终文件名扩展名查规则，命中则拼 `folder`，未命中落默认目录。
  3. `save_dir` 闸门（`save_dir.rs::admit_save_dir`）必须对新拼出的子目录同样生效，
     否则「分类目录」会成为绕过信任目录检查的后门。
  4. 前端侧栏分类计数改成按真实落点统计；设置页加「分类规则」卡片。
- **风险**：中。**闸门顺序错了就是安全问题**；另外用户在「添加 URL」里手选的目录
  应当优先于分类规则（显式 > 隐式）。
- **验收**：`.mp4` 落 Video、`.zip` 落 Archive、手选目录时不被覆盖、越界目录仍弹确认。
- **工作量**：2 天。
- **已落地（批次 B）**：`Settings` 增加 `categories_enabled`（默认 **false**）与
  `categories: Vec<CategoryRule>`（`folder` + `extensions`）。默认五类，保存时
  归一化：目录只允许单层（拒绝 `..`/分隔符/盘符，其余非法字符清洗），扩展名
  小写去点、同目录合并，最多 24 条规则 / 每类 64 个扩展名。`Task.expected_ext`
  由前端（视频取 `f.ext`）或后端从文件名/URL 推导，并随 `history.json` 持久化。
  `resolve_task_dir()` 仅在 `task.save_dir` 为空时拼接分类子目录——分类永远发生在
  已批准的下载根之下，不会绕开目录闸门；手选目录完全优先。设置页提供开关与
  可增删的规则行。侧栏分类仍按 `kind` 过滤（未改成按真实落点统计）。

#### P0-3 剪贴板监听
- **现状**：`clipboard.rs::read_clipboard_text()` 已有，只被「从剪贴板添加」按钮调用。
- **GD3**：后台监听，识别到链接就提示/入队，可按来源忽略。
- **改法**：Rust 侧起一个 800ms 轮询（`tauri::async_runtime::spawn`），
  与上次内容做哈希去重，只认 `http(s)`，命中后**发事件让前端提示，不自动入队**
  （自动入队会被恶意网页/脚本利用成「静默下载」）。
  设置项：`clipboard_watch: bool` + `clipboard_ignore_hosts: Vec<String>`。
- **风险**：低。注意别和「添加 URL」弹窗的自动预填打架（弹窗打开时暂停监听）。
- **验收**：复制链接 → 3 秒内出现提示；同一链接不重复提示；关掉开关后完全静默。
- **工作量**：1 天。
- **已落地（批次 A）**：`clipboard.rs::spawn_watcher`（独立线程，800ms），
  哈希指纹去重，只在主窗可见时 emit `clipboard-url`，前端给「添加」按钮；
  `copy_text` 会 `note_self_copy` 避免自己写的链接回弹。
  **未做** `clipboard_ignore_hosts`：当前只提示不入队，按 host 忽略的收益
  低于维护成本，留到有「自动入队」需求时再加。

#### P0-4 关闭行为 + 完成通知
- **现状**：`lib.rs` 的 `CloseRequested` 固定 `prevent_close()` + `hide()`。
- **GD3**：询问 / 后台 / 退出三档；完成有系统通知。
- **改法**：`settings.rs` 加 `close_mode`；`CloseRequested` 里按模式分流
  （询问用前端确认框，别用原生 MessageBox，保持视觉一致）。
  通知用**托盘气泡**（`Shell_NotifyIconW` + `NIF_INFO`，`windows-sys` 已在依赖里），
  不引入 `tauri-plugin-notification`，符合「零新依赖」。
- **风险**：低。托盘气泡在「专注助手」开启时会被吞，属系统行为。
- **验收**：三档行为正确；有任务在跑时退出要拦一次；完成后气泡可点击聚焦主窗。
- **工作量**：1.5 天。
- **已落地（批次 A）**：`close_mode` 三档（默认 `background`，兼容升级前行为）；
  「询问」用前端三选一弹窗（原生 MessageBox 只有两键）。
  完成提醒改成**闪烁任务栏 + 托盘 tooltip**，不用系统 toast：Windows toast 依赖
  注册过的 AppUserModelID，便携版/直接双击 exe 时常静默失效，而
  `request_user_attention` 与托盘提示在进程内必定生效，且零依赖。

#### P0-5 完成后动作（关机 / 休眠 / 重启 / 打开文件）
- **现状**：无。
- **GD3**：`Plan` 单例，等「全部任务完成」后执行一次。
- **改法**：新建 `plan.rs`，状态只存内存（一次性意图，不跨重启）；
  `pump_queue` 判定「队列空且无活跃任务」时触发；关机用 `shutdown /s /t 60`（可取消）。
- **风险**：中。**必须二次确认 + 60 秒可取消**，并在 UI 常驻显示「将在 60 秒后关机」。
- **验收**：单任务/多任务完成各触发一次；取消有效；手动暂停全部时不触发。
- **工作量**：1 天。
- **已落地（批次 A）**：`plan.rs`，状态**只存内存不落盘**——关机是一次性意图，
  落盘会让「昨天忘取消」的计划在今天随便下个文件时把电脑关了。
  触发条件：队列空 + 无活跃槽位 + 无 Queued/Downloading/Merging/Paused/Failed
  + 至少一个 Done；续泵之后再判定，避免并发队列里误判。
  关机/重启走 `shutdown /s|/r /t 60`，前端提示带「取消」（`shutdown /a`）；
  「打开文件」遇到 exe/msi/bat/cmd/ps1/vbs/js/scr/com/lnk/url/hta/reg/inf/jar/wsf/wsh/cpl/msc
  只在资源管理器定位，不自动运行。
  状态栏有常驻 chip，点击可改/取消。

#### P0-6 任务编辑
- **现状**：只能删除重建。
- **GD3**：`edit(options)` 可改目录、线程、请求头，运行中先停再改。
- **改法**：命令 `edit_task(id, patch)`；`Task` 增加 `filename_override: Option<String>`；
  仅允许 `Queued/Paused/Failed` 编辑，运行中返回明确错误。
  前端右键菜单加「编辑…」，复用「添加 URL」弹窗的字段。
- **风险**：低。编辑 `save_dir` 仍要走闸门。
- **验收**：暂停态改线程后继续生效；运行态编辑被拒且有提示。
- **工作量**：2 天。

#### P0-7 自定义请求头 / UA（含 Referer）
- **现状**：UA 写死，无法带 Referer —— 防盗链直链基本下不动。
- **GD3**：多套请求头预设 + 按 host 的身份预设。
- **改法**：`settings.rs` 加 `header_presets: Vec<HeaderPreset>`（`name` + `headers`）；
  `direct.rs` 构造 `RequestBuilder` 时统一注入（探测与分块两条路径都要，别只改一条）。
  **黑名单**：禁止覆盖 `Host` / `Content-Length` / `Range` / `Connection`，
  `Cookie` 只允许来自现有 Cookie 机制，避免「设置页变成任意请求伪造器」。
  yt-dlp 侧映射为 `--add-header` / `--user-agent` / `--referer`。
- **风险**：中（安全）。预设要能「只对某 host 生效」，否则 token 会跟着重定向漏出去
  ——`HostAuth` 的作用域校验逻辑可以直接复用。
- **验收**：带 Referer 的防盗链可下；跨 host 重定向不携带敏感头；非法头名被拒。
- **工作量**：2 天。
- **已落地（批次 B）**：`Settings.header_presets: Vec<HeaderPreset>`（`name` + `host` +
  `headers`）。头名只放行 RFC 7230 token 字符，头值拒绝 CR/LF/NUL，`Host` /
  `Range` / `Content-Length` / `Connection` / `Cookie` / `Accept-Encoding` 等由
  下载器管理的头一律丢弃；空 host（全局）不允许 `Authorization`，最多 8 套预设、
  每套 16 个头。`headers_for_url` 按 host（精确或子域）过滤，相似域名与 query
  里的域名不算。直链在 `apply_net` 里统一注入探测 HEAD/Range 与分块/单流请求；
  若命中敏感头（Authorization、`*token*`、`*api-key*` 等），跨站重定向直接拒绝，
  避免凭证跟着 302 交给第三方 CDN。yt-dlp 侧映射为 `--user-agent` / `--referer` /
  `--add-headers`，并在 HF token 已注入时跳过重复的 Authorization。设置页支持
  增删预设、填 host 与多行 `Name: value`。

#### P0-8 磁盘空间预检
- **现状**：无，3 GB 的文件下到 99% 才报错。
- **改法**：`probe_direct` 拿到 `total_bytes` 后，用 `GetDiskFreeSpaceExW`
  （`windows-sys` 加 `Win32_Storage_FileSystem` feature，无新 crate）比较可用空间；
  不足则直接失败并给「还差 X GB」。yt-dlp 任务在解析后按 `filesize` 估。
- **风险**：低（预留 5% 余量，别卡太死）。
- **工作量**：0.5 天。
- **已落地（批次 A）**：`disk.rs::ensure_room`（`GetDiskFreeSpaceExW` + 5% 余量，
  拿不到容量时放行）。直链在提交时按 `expected_bytes` 预检、下载器内再按实际
  `Content-Length` 兜一次（续传只算差额）；yt-dlp 任务在解析出 `filesize` 时
  提交预检，HLS 这类没有大小的源跳过。

**P0 合计 ≈ 11.5 人日。**

---

### P1 · 能力扩展（一两个版本）

| # | 功能 | 要点 | 工作量 |
|---|---|---|---|
| P1-1 | 分类规则可编辑 | 已随批次 B 落地：设置页可增删规则、改目录与扩展名；「恢复默认」会带回五类预设 | 2d |
| P1-2 | 队列排序 / 置顶 | `queue` 从 `VecDeque` 换成可重排结构 + `move_to_front`；前端拖拽或右键置顶 | 1.5d |
| P1-3 | 完成后校验和 | `update.rs::sha256_file` 已用系统 BCrypt 实现，抽成 `hash.rs` 即可复用（MD5/SHA1 同样是 BCrypt 常量），**零新依赖**；完成后比对，可配置期望值 | 1d |
| P1-4 | 同名 ask 档 | 事件 + 弹窗 + 重入决策点（见 P0-1） | 1.5d |
| P1-5 | 浏览器扩展升级 | ① 配对令牌（桥现在只校验 Origin）② 端口可配（58943 冲突时）③ 资源嗅探列表 ④ 草稿模式 | 5–8d |
| P1-6 | aria2 RPC 子集 | `tokio::TcpListener` + 手写 JSON-RPC（`bridge.rs` 已有手写 HTTP 的经验）；只做 `getVersion/getGlobalOption/addUri/tellStatus`，强制 `127.0.0.1` + token | 3d |
| P1-7 | CLI | `veloxgrab.exe add <url> --dir …`，复用 `parse_deep_link` 与单实例转发 | 2d |
| P1-8 | 便携模式 | 检测 exe 同级 `portable.flag`，把 `%APPDATA%` 换成 `./data` | 1d |
| P1-9 | 界面缩放 | 根字号缩放 + 记住比例；注意与任务表 grid 列宽联动 | 1d |
| P1-10 | 多语言扩展 | 现有 i18n 结构可直接加 locale；成本在翻译与文案长度回归 | 2–3d/语言 |

---

### P2 · 长期 / 需要新引擎

| # | 功能 | 判断 |
|---|---|---|
| P2-1 | BitTorrent | 需 `librqbit`/`transmission` 级依赖 + 做种/端口/UPnP，与「单 exe 零重依赖」冲突。**不做**，除非定位改成通用下载器 |
| P2-2 | eD2k / FTP | 用户量极低，收益不抵维护成本。**不做** |
| P2-3 | 播放列表 / 多文件任务 | 有价值：`--no-playlist` 放开 + `Task` 支持 `files[]` + 选择界面。但会牵动任务模型与 UI。**评估后再定** |
| P2-4 | 窗口材质（Mica/Acrylic） | `DwmSetWindowAttribute` + `windows-sys`（`Win32_Graphics_Dwm`）可行，但与设计稿的 CSS 磨砂会打架：建议做成「系统材质」开关，开了就把 CSS 玻璃降为纯色 |
| P2-5 | 插件包体系 | GD3 的 `features/` 是 Python 动态加载；Rust 侧等价物是 dylib 插件，ABI 不稳定、签名/加载都是坑。**不抄**，用编译期 `Source` 枚举替代 |
| P2-6 | OOBE | 首次引导（选目录 + 装扩展）能降流失，但优先级低于 P0 全部 |

---

## 4. 不建议照搬的

1. **关 SSL 校验 / 客户端指纹模拟**：GD3 为兼容反爬站点默认关校验；本项目安全姿态更严，
   开了等于给中间人开门。若确有需求，只做「单 host 白名单 + 显式警告」。
2. **插件包动态加载**：Python 的 `import` 模型搬到 Rust 需要 dylib ABI，收益远小于风险。
3. **aria2 全量 RPC**：GD3 自己都只实现 3 个方法；全量意味着几十个方法 + 状态机对齐，
   维护成本会长期压在主干上。做子集即可。
4. **8 语言全量翻译**：没有翻译维护者时，多语言会变成「一半英文一半机翻」。
   先把中英打磨好，按需求逐个加。
5. **设置项 1:1 对齐**：GD3 的设置页有 60+ 项，其中一半服务于本项目没有的协议。
   照抄会把设置页变成迷宫（现有单列表单已经偏长，应借设计稿改成两列卡片）。

---

## 5. 三批落地建议

**批次 A（体验闭环，≈6 天）**：P0-3 剪贴板监听 → P0-4 关闭行为+通知 → P0-5 完成后动作 → P0-8 磁盘预检。
全部是「加一个模块 + 一个设置项」，互不依赖，可独立发版。

**批次 B（下载质量，≈6 天）**：P0-2 分类目录 → P0-1 同名策略 → P0-7 请求头 → P0-6 任务编辑。
这批会碰 `resolve_task_dir` / `save_dir` 闸门 / 请求构造，**必须一起回归**：
目录越界、跨 host 重定向、同名覆盖、暂停后编辑。

**批次 C（集成，按需）**：P1-5 扩展升级 → P1-6 aria2 RPC → P1-7 CLI。

与设计稿的关系：批次 A/B 的新增设置项直接进设计稿的「两列卡片」布局；
分类规则卡片、完成后动作、剪贴板监听各占一张，不需要改导航结构。

---

## 6. 回归清单（新增功能共通）

- 目录闸门：任何新路径（分类子目录、编辑后目录）都必须过 `admit_save_dir`
- 敏感头：跨 host 重定向后不得携带 `Authorization` / 自定义 token
- 本地服务：新监听一律 `127.0.0.1` + 显式开关 + 令牌/Origin 校验
- 破坏性操作：覆盖、关机、删除文件都要二次确认且可取消
- 崩溃恢复：新增字段必须能进 `history.json` 且旧版本读到不炸（`serde(default)`）
- 文案：所有新字符串进 i18n，中英同步，注意中文长度膨胀 30%
