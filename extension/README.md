# VeloxGrab 浏览器扩展

让「网页上点下载 → 交给 VeloxGrab」这件事真正成立。装好后：

- **网页里点任何下载链接，浏览器不再自己下，自动转给 VeloxGrab**（可在弹窗里关掉）
- 右键链接 → **用 VeloxGrab 下载此链接**
- 右键图片 / 视频 / 音频 → **用 VeloxGrab 下载此媒体**
- 右键页面空白处 / 视频 → **下载嗅探到的流媒体（HLS/DASH）**，抓页面里真正在播的 `.m3u8` / `.mpd`
- 点工具栏图标 → 弹出面板，顶部是**任务监视面板**：桌面端正在进行的每个下载
  （进度条、速度、状态），可直接暂停 / 继续，2 秒自动刷新；下方是发送入口
  （页面链接、页面里的媒体、嗅探到的流媒体一键发送）与目录 / 线程 / 接管 / 嗅探开关

## 安装（Chrome / Edge 通用）

1. 打开 `chrome://extensions`（Edge 用 `edge://extensions`）
2. 右上角打开 **开发者模式**
3. 点 **加载已解压的扩展程序**，选择本目录（`extension/`）
4. 首次点击时浏览器可能提示「是否允许访问所有网站」，选允许

> 目录位置：安装版里它位于 `%LOCALAPPDATA%\VeloxGrab\extension\`，
> 在 VeloxGrab 的「设置 → 浏览器集成 → 打开扩展目录」可以直接定位。

## 工作机制

### 传输：本地回环 HTTP 桥（主路径）

VeloxGrab 启动时在 `127.0.0.1:58943` 起一个极小的 HTTP 服务
（`src-tauri/src/bridge.rs`）。扩展 `fetch("http://127.0.0.1:58943/add", …)`
把下载请求递过去，VeloxGrab 收到后走与 `veloxgrab://` 完全相同的分发路径
（入队 → 唤醒前端 → 弹「另存为」）。

**为什么不用 `veloxgrab://` 做主路径**：Chrome/Edge 会**静默拦截**扩展发起的
自定义协议导航——`chrome.tabs.update({url: "veloxgrab://…"})` 既不报错也不发生，
标签页原地不动。协议本身是通的（注册表、单实例回调、冷启动 argv 都验过），
断的是「扩展 → 协议」这一步。这正是「网页点下载没反应」的根因。

回环 HTTP 不需要固定扩展 ID、不需要注册表、不需要额外二进制，一条 `fetch`
就到位。`veloxgrab://` 保留为兜底路径（桥连不上时用）。

### 拦截：`chrome.downloads.onCreated`

浏览器一决定下载，扩展立刻 `chrome.downloads.cancel()` 掉它，再用
`chrome.downloads.erase()` 抹掉下载面板里的残迹，然后把 URL 转给 VeloxGrab。
文件名优先用浏览器已经解析出的那个（`DownloadItem.filename`）——它包含
`Content-Disposition` 里的名字，比从 URL 猜准得多。

只接管 `http/https`：`blob:` / `data:` 交给浏览器，VeloxGrab 拿不到内容。
弹窗里的「接管浏览器下载」开关可以整体关掉（存在 `chrome.storage.sync`）。

### 桌面端没在运行：自动拉起

拦截/发送时如果本地桥连不通（VeloxGrab 没在跑），扩展会通过 MV3 的
**offscreen document** 导航到 `veloxgrab://launch`——offscreen 页面是真实的
扩展页面，`location.href` 触发的系统协议分发不会被 Chrome 拦（被拦的只是
`tabs.update/create` 对自定义协议的导航写法）。桌面端起来后用户重试一次即可；
任务不随 launch 重发（桌面端可能还没监听桥，避免时序竞态）。

前提是协议注册存在——桌面端安装后首次启动会写 `HKCU\...\veloxgrab`。
便携版没注册协议时 offscreen 导航静默无效，退回「只提示用户手动启动」。

拉起之后扩展会**轮询桥并自动补交任务**（800ms 间隔、20 秒上限）：桌面端
冷启动 1~3 秒就能监听桥，等通后用与首次尝试完全相同的参数重发——用户
点一次下载就完事，不需要手动重试。补交成功对调用方就是普通的成功路径；
20 秒仍不通（协议分发没生效）才把下载交回浏览器并提示手动启动。

### 任务监视面板：`/tasks` 与 `/task-action`

弹窗顶部的面板就是下载监视器（GD3 插件的同款体验）：桌面端正在进行的每个
任务一行，2 秒轮询一次。数据来自本地桥的三个只读/受控接口，与 `/add` 过
**完全相同**的门禁链（Host → Origin → token），但不经过 /add 的速率闸——
弹窗轮询是正常节奏，不是灌任务：

| 接口 | 作用 |
| --- | --- |
| `POST /tasks` | 快照：进行中 + 暂停/失败/排队 + 60 秒内刚完成的任务 |
| `POST /task-action` | `{id, action: "pause"|"resume"}`，走与桌面按钮同一套内核 |
| `POST /open-file` | 打开一个已完成任务的成品（只 open，不执行别的） |

桌面端没有独立的监视窗口：面板就在扩展弹窗里，popup 关掉轮询即停，
不在后台 SW 常驻。

### 深度嗅探：`webRequest.onResponseStarted`

只看 DOM 的 `<video src>` 抓不到真正在播的流：B 站 / 抖音 / YouTube 都用 MSE 播放，
`src` 是 `blob:`，地址只存在于页面内存里。但这些站点的**网络请求**里一定有真正的
分片清单（`.m3u8` / `.mpd`）。

扩展监听响应的类型与 URL 后缀，把清单地址按页签记账（每页签最多 40 条、导航或
关闭时清空），弹窗与右键菜单可以直接发送。嗅探可在弹窗里单独关掉。

### Referer：防盗链直链

浏览器自己发起下载时天然带页面 Referer；接管之后由桌面端重发，很多对象存储的
签名直链只认「Referer 在白名单里」。所以拦截路径会把 `DownloadItem.referrer`
（回退到当前页 URL）作为 `referer` 字段交给桥，桌面端落到任务的 `Referer` 请求头
（直链经 `NetCfg` 注入，视频任务映射为 yt-dlp `--referer`），并随 `history.json`
持久化——续传不会 403。

### 桥的安全边界

监听地址写死 `127.0.0.1`，外部网络连不到。真正的风险是本机其它程序或恶意
网页往端口灌任务，所以：

1. **只接受 `Origin` 为 `chrome-extension://` / `moz-extension://` 的请求**。
   网页 JS 发起的请求 Origin 一定是 `http(s)://…`，浏览器不允许伪造。
   `Origin` 缺失时放行——curl 和手工测试不带 Origin，而能连本机回环的程序
   本来就能直接调 `veloxgrab://`。
2. 请求头 ≤ 8 KiB、请求体 ≤ 64 KiB。
3. 只解析 `/add`，其余一律 404。

### 两个踩过的坑（改链接构造时必须记住）

1. **浏览器会给 `veloxgrab://add` 补斜杠。** 按 WHATWG URL 规则，
   `veloxgrab://add?url=...` 里的 `add` 会被解析成 authority，规范化后变成
   `veloxgrab://add/?url=...`。VeloxGrab 侧因此必须容忍 `add/`，否则网页里
   100% 报「不支持的动作：add/」。
2. **拼 query 不能用 `URLSearchParams.toString()`。** 它会把 `:` 编码成
   `%3A`，浏览器再解析 `veloxgrab://` 时 scheme 的分隔语义被吃掉：
   `veloxgrab://https://a.com/x.zip` 会变成 `veloxgrab://https//a.com/x.zip`（单斜杠），
   裸链接形态直接废掉。所以 `background.js` 里是逐项 `encodeURIComponent` 手工拼。

## 权限说明

| 权限 | 用途 |
| --- | --- |
| `activeTab` | 只读当前标签页 URL，用来预填弹窗 |
| `contextMenus` | 右键菜单 |
| `scripting` | 在页面上下文里抓 `video/audio/img/a[href]` |
| `storage` | 记住你填过的目录、线程数、接管开关 |
| `downloads` | **拦截浏览器下载**（`onCreated` → `cancel` → 转给 VeloxGrab） |
| `notifications` | 转交成功 / VeloxGrab 未运行时提示 |
| `webRequest` + `host_permissions` | **只读取**响应 URL 与类型，识别 HLS/DASH 清单（不改写、不阻塞任何请求） |
| `tabs` | 读取当前标签页地址（Referer）与页面导航时清理嗅探记录 |

脚本只用 `scripting.executeScript`（`activeTab` 授权：点开弹窗那一刻才对当前标签页生效）
抓当前页的 `video/audio/img/a[href]`，
不上传任何数据，不与任何第三方通信。`webRequest` 只做**观测**（MV3 不支持阻塞式
拦截，本扩展也从不尝试改请求）。唯一的网络请求是 `127.0.0.1:58943`
（VeloxGrab 自己）。

## 点了没反应？按这个顺序查

1. **VeloxGrab 没在跑时会自动拉起**：拦截/发送失败时扩展会通过
   `veloxgrab://launch` 启动桌面端（通知栏也会提示）。如果拉起没发生，
   先检查协议注册：`reg query HKCU\Software\Classes\veloxgrab`——
   便携版没有这条。
2. **确认桥通不通**：浏览器地址栏输入
   `http://127.0.0.1:58943/add?url=https%3A%2F%2Fexample.com%2Fx.zip` 回车。
   - 返回 `{"ok":true}` → 桥正常，问题在扩展侧（看下一步）；
   - 连接被拒绝 → VeloxGrab 没运行，或端口被别的程序占了（`netstat -ano | findstr 58943`）。
3. **确认扩展被刷新**：改完代码要在 `chrome://extensions` 里点一次刷新（↻），
   否则跑的还是旧版本。MV3 的 service worker 会被浏览器回收，重新加载扩展最稳。
4. **确认 VeloxGrab 真的收到了**：点完下载后 VeloxGrab 应该弹「另存为」。
   - 弹了但立刻报错 → 是下载本身失败（URL 404 / 需要登录），看弹窗里的错误；
   - 完全不弹 → 链接没进程序。查 `chrome://extensions` 里扩展的
     「服务工作进程」→ 检查视图，Console 里应有 `fetch` 失败的记录。
5. **协议兜底路径**（桥不可用时）：地址栏直接输入
   `veloxgrab://add/?url=https%3A%2F%2Fexample.com%2Fx.zip` 回车，
   弹出「打开 VeloxGrab?」说明协议已注册。协议由 VeloxGrab 启动时写
   `HKCU\Software\Classes\veloxgrab`，可用 `reg query HKCU\Software\Classes\veloxgrab` 确认。

## 已知限制

- 下载拦截发生在浏览器**已经开始下载之后**（`onCreated`），所以超大文件可能
  已经传了几百 KB 才被取消——那些字节白费了。要完全避免得用
  `webRequest` 拦截，但 MV3 的阻塞式 `webRequest` 已不可用。
- `chrome://`、Web Store、PDF 内置查看器等受限页面上的下载仍由浏览器处理。
- 需要浏览器保持运行；扩展不能在没有浏览器时独立工作（这本来也是它的职责）。
