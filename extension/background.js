// VGrab 扩展的后台脚本（MV3 service worker）
//
// ## 这个文件负责两件事
//
// 1. **拦截网页下载**：`chrome.downloads.onCreated` 一响就取消浏览器自己的
//    下载，转给 VGrab。这是「网页点下载就调用下载软件」的实现——没有它，
//    扩展就只是个右键菜单，用户点链接仍然走浏览器下载器。
// 2. **可靠地把链接送出去**：优先走本地回环 HTTP 桥
//    （`http://127.0.0.1:58943/add`），失败才退回 `vgrab://` 协议导航。
//
// ## 为什么不再只靠 vgrab://
//
// Chrome/Edge 会**静默拦截**扩展发起的自定义协议导航：
// `chrome.tabs.update({url: "vgrab://..."})` 既不报错也不发生，标签页原地不动。
// 于是「点了没反应」——这正是用户报的故障。协议本身是通的（注册表、单实例
// 回调、冷启动 argv 都验过），断的是「扩展 → 协议」这一步。
//
// 回环 HTTP 桥由 VGrab 自己监听（`src-tauri/src/bridge.rs`），扩展
// `fetch` 一下就行，不需要固定扩展 ID、不需要注册表、不需要额外二进制。

const SCHEME = "vgrab";

/** VGrab 的本地桥端口，与 `src-tauri/src/bridge.rs` 的 `PORT` 必须一致。 */
const BRIDGE = "http://127.0.0.1:58943";

/** 默认配置。`intercept` 是「接管浏览器下载」总开关。 */
const DEFAULTS = { threads: 4, dir: "", intercept: true, notify: true };

// ---------- 安装 / 启动 ----------

chrome.runtime.onInstalled.addListener(() => {
  setupMenus();
  chrome.storage.sync.get(Object.keys(DEFAULTS), (s) => {
    chrome.storage.sync.set({ ...DEFAULTS, ...s });
  });
});

// service worker 会被浏览器随时回收，重启后菜单和状态都得重建。
chrome.runtime.onStartup.addListener(setupMenus);

// 每次 SW 醒来都向 VGrab 报一次「我在」。
//
// 这是 VGrab 判断扩展到底有没有被加载的唯一硬证据：安装器把文件拷进
// $INSTDIR\extension 只说明「装了一份副本」，用户可能从没在 chrome://extensions
// 里加载过它——那正是「装上去了但浏览器里没有扩展」的成因。
// SW 每次被唤醒（含浏览器启动、扩展刚加载）都会跑这一行，所以只要加载成功，
// VGrab 那边最多几十秒内就能知道。
function ping() {
  // 必须用 POST + JSON 而不是裸 GET。
  //
  // 裸 GET 在 Chromium 里属于「简单请求」，**不带 Origin 头**；VGrab 那边正是
  // 拿 Origin 判断「这请求到底是不是扩展发来的」，于是 ping 虽然 200 了，
  // 却没被记成「扩展已加载」——用户装好了扩展，程序还在提示没装。
  // POST 带 Content-Type: application/json 会触发 CORS 预检，浏览器必定附上
  // Origin: chrome-extension://<id>，门禁才认得出它。
  fetch(`${BRIDGE}/ping`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ext: chrome.runtime.id }),
    cache: "no-store",
  })
    .then((r) => console.log("[vgrab] ping", r.status))
    .catch((e) => console.warn("[vgrab] ping failed:", String(e)));
}
ping();

function setupMenus() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "vgrab-link",
      title: "用 VGrab 下载此链接",
      contexts: ["link"],
    });
    chrome.contextMenus.create({
      id: "vgrab-page",
      title: "把当前页面链接发给 VGrab",
      contexts: ["page"],
    });
    chrome.contextMenus.create({
      id: "vgrab-media",
      title: "用 VGrab 下载此媒体",
      contexts: ["image", "video", "audio"],
    });
  });
}

// ---------- 下载拦截 ----------

/**
 * 正在被我们取消的下载 id。
 *
 * `chrome.downloads.cancel()` 是异步的，取消过程中 `onCreated` 可能因为
 * 重定向再触发一次；不记账就会把同一条下载转给 VGrab 两遍。
 */
const canceling = new Set();

chrome.downloads.onCreated.addListener(async (item) => {
  if (!item || !item.id) return;

  // 扩展自己发起的下载不拦（将来若做「用浏览器下载」按钮需要）。
  if (item.byExtensionId) return;

  // 只接管 http/https。blob:/data: 这类交给浏览器——VGrab 拿不到内容。
  if (!/^https?:/i.test(item.url || "")) return;

  const cfg = await getConfig();
  if (!cfg.intercept) return;

  if (canceling.has(item.id)) return;
  canceling.add(item.id);

  try {
    await chrome.downloads.cancel(item.id);
  } catch {
    // 取消失败（下载已经结束）就别再转发了，否则用户会看到两条。
    canceling.delete(item.id);
    return;
  }

  // 从浏览器下载队列里抹掉这条记录，否则下载面板会留一条「已取消」。
  try {
    await chrome.downloads.erase({ id: item.id });
  } catch {
    // erase 失败只是留个痕迹，不影响主流程
  }

  // 用浏览器已经解析出的文件名：它比我们从 URL 猜的准得多
  // （Content-Disposition 里的名字只有浏览器知道）。
  const filename = pickFilename(item);
  const ok = await send({ url: item.url, filename, dir: cfg.dir, threads: cfg.threads });

  if (cfg.notify) {
    notify(
      ok ? "已转给 VGrab" : "VGrab 未响应",
      ok ? filename || item.url : "请确认 VGrab 正在运行（托盘图标可见）"
    );
  }
});

/** 从 DownloadItem 里挑一个像文件名的值。 */
function pickFilename(item) {
  const raw = item.filename || "";
  if (!raw) return guessName(item.url);
  // filename 是完整路径，取最后一段
  const base = raw.split(/[\\/]/).filter(Boolean).pop();
  return base || guessName(item.url);
}

// ---------- 配置 ----------

function getConfig() {
  return new Promise((resolve) => {
    chrome.storage.sync.get(Object.keys(DEFAULTS), (s) => {
      resolve({ ...DEFAULTS, ...s });
    });
  });
}

// ---------- 发送 ----------

/**
 * 把一条下载请求交给 VGrab。
 *
 * 顺序很重要：**先桥后协议**。桥是同步返回成败的，能明确告诉用户
 * 「发出去了」还是「VGrab 没开」；协议导航永远是「成功」的假象
 * （Chrome 拦掉也不报错），只能当兜底。
 */
async function send(target) {
  if (!target || !target.url) return false;

  // 1. 本地桥
  try {
    const ctrl = new AbortController();
    // 桥是本机进程，正常几毫秒就回；2 秒还没回说明 VGrab 没在跑。
    const timer = setTimeout(() => ctrl.abort(), 2000);
    const res = await fetch(`${BRIDGE}/add`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        url: target.url,
        name: target.filename || "",
        dir: target.dir || "",
        threads: target.threads || 0,
      }),
      signal: ctrl.signal,
    });
    clearTimeout(timer);
    if (res.ok) {
      const j = await res.json().catch(() => ({}));
      if (j && j.ok) return true;
    }
  } catch {
    // 桥不可用 → 走协议
  }

  // 2. vgrab:// 协议导航（兜底）
  const link = buildLink(target);
  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (tab && tab.id) {
      await chrome.tabs.update(tab.id, { url: link });
      return true;
    }
  } catch {
    // 继续落到剪贴板
  }

  return fallback(link);
}

/** 拼 `vgrab://add?url=...&name=...`。 */
function buildLink(target) {
  const parts = ["url=" + encodeURIComponent(target.url)];
  if (target.filename) parts.push("name=" + encodeURIComponent(target.filename));
  if (target.dir) parts.push("dir=" + encodeURIComponent(target.dir));
  if (target.threads) parts.push("threads=" + encodeURIComponent(String(target.threads)));
  return `${SCHEME}://add?${parts.join("&")}`;
}

/** 最后一道退路：至少让用户能把链接粘进 VGrab。 */
async function fallback(link) {
  try {
    await navigator.clipboard.writeText(link);
    notify("VGrab 没有响应", "下载链接已复制到剪贴板，可粘贴到 VGrab 的「添加URL」");
  } catch {
    notify("发送失败", link);
  }
  return false;
}

function guessName(url) {
  try {
    const u = new URL(url);
    const last = u.pathname.split("/").filter(Boolean).pop();
    return last ? decodeURIComponent(last) : u.hostname;
  } catch {
    return "";
  }
}

function notify(title, message) {
  try {
    chrome.notifications.create({
      type: "basic",
      iconUrl: "icon128.png",
      title,
      message: String(message || "").slice(0, 200),
    });
  } catch {
    // 通知权限被拒不影响主流程
  }
}

// ---------- 右键菜单 ----------

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const cfg = await getConfig();
  const base = { threads: cfg.threads, dir: cfg.dir };

  if (info.menuItemId === "vgrab-link" && info.linkUrl) {
    send({ ...base, url: info.linkUrl, filename: guessName(info.linkUrl) });
    return;
  }
  if (info.menuItemId === "vgrab-media" && info.srcUrl) {
    send({ ...base, url: info.srcUrl, filename: guessName(info.srcUrl) });
    return;
  }
  if (info.menuItemId === "vgrab-page") {
    const url = info.pageUrl || (tab && tab.url);
    if (url) send({ ...base, url, filename: guessName(url) });
  }
});

// ---------- popup 消息 ----------

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (!msg || msg.type !== "vgrab-send") return;

  (async () => {
    const cfg = await getConfig();
    const ok = await send({
      url: msg.url,
      filename: msg.filename || guessName(msg.url),
      threads: cfg.threads,
      dir: cfg.dir,
    });
    sendResponse({ ok });
  })();

  // 异步响应必须返回 true，否则通道会被立刻关掉
  return true;
});
