# VGrab 浏览器扩展

让「网页上点下载 → 交给 VGrab」这件事真正成立。装好后：

- **网页里点任何下载链接，浏览器不再自己下，自动转给 VGrab**（可在弹窗里关掉）
- 右键链接 → **用 VGrab 下载此链接**
- 右键图片 / 视频 / 音频 → **用 VGrab 下载此媒体**
- 右键页面空白处 → **把当前页面链接发给 VGrab**
- 点工具栏图标 → 弹出面板，可改目录 / 线程 / 接管开关，并列出页面里的媒体一键发送

## 安装（Chrome / Edge 通用）

1. 打开 `chrome://extensions`（Edge 用 `edge://extensions`）
2. 右上角打开 **开发者模式**
3. 点 **加载已解压的扩展程序**，选择本目录（`extension/`）
4. 首次点击时浏览器可能提示「是否允许访问所有网站」，选允许

> 目录位置：安装版里它位于 `%LOCALAPPDATA%\VGrab\extension\`，
> 在 VGrab 的「设置 → 浏览器集成 → 打开扩展目录」可以直接定位。

## 工作机制

### 传输：本地回环 HTTP 桥（主路径）

VGrab 启动时在 `127.0.0.1:58943` 起一个极小的 HTTP 服务
（`src-tauri/src/bridge.rs`）。扩展 `fetch("http://127.0.0.1:58943/add", …)`
把下载请求递过去，VGrab 收到后走与 `vgrab://` 完全相同的分发路径
（入队 → 唤醒前端 → 弹「另存为」）。

**为什么不用 `vgrab://` 做主路径**：Chrome/Edge 会**静默拦截**扩展发起的
自定义协议导航——`chrome.tabs.update({url: "vgrab://…"})` 既不报错也不发生，
标签页原地不动。协议本身是通的（注册表、单实例回调、冷启动 argv 都验过），
断的是「扩展 → 协议」这一步。这正是「网页点下载没反应」的根因。

回环 HTTP 不需要固定扩展 ID、不需要注册表、不需要额外二进制，一条 `fetch`
就到位。`vgrab://` 保留为兜底路径（桥连不上时用）。

### 拦截：`chrome.downloads.onCreated`

浏览器一决定下载，扩展立刻 `chrome.downloads.cancel()` 掉它，再用
`chrome.downloads.erase()` 抹掉下载面板里的残迹，然后把 URL 转给 VGrab。
文件名优先用浏览器已经解析出的那个（`DownloadItem.filename`）——它包含
`Content-Disposition` 里的名字，比从 URL 猜准得多。

只接管 `http/https`：`blob:` / `data:` 交给浏览器，VGrab 拿不到内容。
弹窗里的「接管浏览器下载」开关可以整体关掉（存在 `chrome.storage.sync`）。

### 桥的安全边界

监听地址写死 `127.0.0.1`，外部网络连不到。真正的风险是本机其它程序或恶意
网页往端口灌任务，所以：

1. **只接受 `Origin` 为 `chrome-extension://` / `moz-extension://` 的请求**。
   网页 JS 发起的请求 Origin 一定是 `http(s)://…`，浏览器不允许伪造。
   `Origin` 缺失时放行——curl 和手工测试不带 Origin，而能连本机回环的程序
   本来就能直接调 `vgrab://`。
2. 请求头 ≤ 8 KiB、请求体 ≤ 64 KiB。
3. 只解析 `/add`，其余一律 404。

### 两个踩过的坑（改链接构造时必须记住）

1. **浏览器会给 `vgrab://add` 补斜杠。** 按 WHATWG URL 规则，
   `vgrab://add?url=...` 里的 `add` 会被解析成 authority，规范化后变成
   `vgrab://add/?url=...`。VGrab 侧因此必须容忍 `add/`，否则网页里
   100% 报「不支持的动作：add/」。
2. **拼 query 不能用 `URLSearchParams.toString()`。** 它会把 `:` 编码成
   `%3A`，浏览器再解析 `vgrab://` 时 scheme 的分隔语义被吃掉：
   `vgrab://https://a.com/x.zip` 会变成 `vgrab://https//a.com/x.zip`（单斜杠），
   裸链接形态直接废掉。所以 `background.js` 里是逐项 `encodeURIComponent` 手工拼。

## 权限说明

| 权限 | 用途 |
| --- | --- |
| `activeTab` | 只读当前标签页 URL，用来预填弹窗 |
| `contextMenus` | 右键菜单 |
| `scripting` | 在页面上下文里抓 `video/audio/img/a[href]` |
| `storage` | 记住你填过的目录、线程数、接管开关 |
| `downloads` | **拦截浏览器下载**（`onCreated` → `cancel` → 转给 VGrab） |
| `notifications` | 转交成功 / VGrab 未运行时提示 |
| `<all_urls>` | 扫描页面里的媒体直链 |

脚本只用 `scripting.executeScript` 抓当前页的 `video/audio/img/a[href]`，
不上传任何数据，不与任何第三方通信。唯一的网络请求是 `127.0.0.1:58943`
（VGrab 自己）。

## 点了没反应？按这个顺序查

1. **先确认 VGrab 在跑**：托盘里有它的图标。桥只在程序运行时存在。
2. **确认桥通不通**：浏览器地址栏输入
   `http://127.0.0.1:58943/add?url=https%3A%2F%2Fexample.com%2Fx.zip` 回车。
   - 返回 `{"ok":true}` → 桥正常，问题在扩展侧（看下一步）；
   - 连接被拒绝 → VGrab 没运行，或端口被别的程序占了（`netstat -ano | findstr 58943`）。
3. **确认扩展被刷新**：改完代码要在 `chrome://extensions` 里点一次刷新（↻），
   否则跑的还是旧版本。MV3 的 service worker 会被浏览器回收，重新加载扩展最稳。
4. **确认 VGrab 真的收到了**：点完下载后 VGrab 应该弹「另存为」。
   - 弹了但立刻报错 → 是下载本身失败（URL 404 / 需要登录），看弹窗里的错误；
   - 完全不弹 → 链接没进程序。查 `chrome://extensions` 里扩展的
     「服务工作进程」→ 检查视图，Console 里应有 `fetch` 失败的记录。
5. **协议兜底路径**（桥不可用时）：地址栏直接输入
   `vgrab://add/?url=https%3A%2F%2Fexample.com%2Fx.zip` 回车，
   弹出「打开 VGrab?」说明协议已注册。协议由 VGrab 启动时写
   `HKCU\Software\Classes\vgrab`，可用 `reg query HKCU\Software\Classes\vgrab` 确认。

## 已知限制

- 下载拦截发生在浏览器**已经开始下载之后**（`onCreated`），所以超大文件可能
  已经传了几百 KB 才被取消——那些字节白费了。要完全避免得用
  `webRequest` 拦截，但 MV3 的阻塞式 `webRequest` 已不可用。
- `chrome://`、Web Store、PDF 内置查看器等受限页面上的下载仍由浏览器处理。
- 需要浏览器保持运行；扩展不能在没有浏览器时独立工作（这本来也是它的职责）。
