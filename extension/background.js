// VeloxGrab 扩展的后台脚本（MV3 service worker）
//
// ## 这个文件负责两件事
//
// 1. **拦截网页下载**：`chrome.downloads.onCreated` 一响就取消浏览器自己的
//    下载，转给 VeloxGrab。这是「网页点下载就调用下载软件」的实现——没有它，
//    扩展就只是个右键菜单，用户点链接仍然走浏览器下载器。
// 2. **可靠地把链接送出去**：优先走本地回环 HTTP 桥
//    （`http://127.0.0.1:58943/add`），失败才退回 `veloxgrab://` 协议导航。
//
// ## 为什么不再只靠 veloxgrab://
//
// Chrome/Edge 会**静默拦截**扩展发起的自定义协议导航：
// `chrome.tabs.update({url: "veloxgrab://..."})` 既不报错也不发生，标签页原地不动。
// 于是「点了没反应」——这正是用户报的故障。协议本身是通的（注册表、单实例
// 回调、冷启动 argv 都验过），断的是「扩展 → 协议」这一步。
//
// 回环 HTTP 桥由 VeloxGrab 自己监听（`src-tauri/src/bridge.rs`），扩展
// `fetch` 一下就行，不需要固定扩展 ID、不需要注册表、不需要额外二进制。

const SCHEME = "veloxgrab";

/** VeloxGrab 的本地桥端口，与 `src-tauri/src/bridge.rs` 的 `PORT` 必须一致。 */
const BRIDGE = "http://127.0.0.1:58943";

/** 默认配置。`intercept` 是「接管浏览器下载」总开关。 */
const DEFAULTS = { threads: 4, dir: "", intercept: true, notify: true };

// ---------- 安装 / 启动 ----------

chrome.runtime.onInstalled.addListener(() => {
  setupMenus();
  ensureAlarm();
  chrome.storage.sync.get(Object.keys(DEFAULTS), (s) => {
    chrome.storage.sync.set({ ...DEFAULTS, ...s });
  });
});

// service worker 会被浏览器随时回收，重启后菜单和状态都得重建。
chrome.runtime.onStartup.addListener(() => {
  setupMenus();
  ensureAlarm();
});

// ---------- 心跳（alarms）----------
//
// 只在 SW 醒来时 ping 有一个盲区：浏览器长期不关、桌面程序却重启了——
// 这时 SW 早已休眠、不会再醒，VeloxGrab 永远收不到 ping，就会反复弹
// 「扩展未安装」引导（用户明明装好了）。alarms 是 MV3 里唯一能定时唤醒
// SW 的合法机制：每 5 分钟醒一次打一发 ping，本机回环开销可以忽略。
// 桌面程序侧配合把「见过扩展」落盘（bridge.rs persist_seen），双保险。
const PING_ALARM = "veloxgrab-ping";

function ensureAlarm() {
  chrome.alarms.get(PING_ALARM, (a) => {
    if (!a) chrome.alarms.create(PING_ALARM, { periodInMinutes: 5 });
  });
}

chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === PING_ALARM) ping();
});

// 每次 SW 醒来都向 VeloxGrab 报一次「我在」。
//
// 这是 VeloxGrab 判断扩展到底有没有被加载的唯一硬证据：安装器把文件拷进
// $INSTDIR\extension 只说明「装了一份副本」，用户可能从没在 chrome://extensions
// 里加载过它——那正是「装上去了但浏览器里没有扩展」的成因。
// SW 每次被唤醒（含浏览器启动、扩展刚加载）都会跑这一行，所以只要加载成功，
// VeloxGrab 那边最多几十秒内就能知道。
function ping() {
  // 必须用 POST + JSON 而不是裸 GET。
  //
  // 裸 GET 在 Chromium 里属于「简单请求」，**不带 Origin 头**；VeloxGrab 那边正是
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
    .then((r) => console.log("[veloxgrab] ping", r.status))
    .catch((e) => console.warn("[veloxgrab] ping failed:", String(e)));
}
ping();

function setupMenus() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "veloxgrab-link",
      title: "用 VeloxGrab 下载此链接",
      contexts: ["link"],
    });
    chrome.contextMenus.create({
      id: "veloxgrab-page",
      title: "把当前页面链接发给 VeloxGrab",
      contexts: ["page"],
    });
    // 媒体右键发给**页面地址**而不是 srcUrl。
    //
    // 原因（实测）：视频站的 <video> 元素 src 多是 `blob:`（B 站、抖音、
    // YouTube 的 MSE 播放都是），VeloxGrab 只接 http/https，发过去必然被拒；
    // 即便 srcUrl 是真 https（个别站点的直链），它也只是**一段没有音轨的
    // 视频流**，下下来是个哑片。
    // 发页面地址则走 yt-dlp 解析，能拿到完整清晰度列表和音轨——这才是
    // 用户右键「下载这个视频」真正想要的东西。
    chrome.contextMenus.create({
      id: "veloxgrab-media",
      title: "用 VeloxGrab 解析并下载此视频",
      contexts: ["image", "video", "audio"],
    });
    // 图片/音频的 src 常常就是可直接下载的原文件（没有 blob 化），
    // 给它留一条「按媒体地址直下」的路，否则下图片会变成去解析图片页。
    chrome.contextMenus.create({
      id: "veloxgrab-media-direct",
      title: "直接下载此媒体文件",
      contexts: ["image", "audio"],
    });
  });
}

// ---------- 下载拦截 ----------

/**
 * 正在被我们取消的下载 id。
 *
 * `chrome.downloads.cancel()` 是异步的，取消过程中 `onCreated` 可能因为
 * 重定向再触发一次；不记账就会把同一条下载转给 VeloxGrab 两遍。
 */
const canceling = new Set();

chrome.downloads.onCreated.addListener(async (item) => {
  if (!item || !item.id) return;

  // 扩展自己发起的下载不拦（将来若做「用浏览器下载」按钮需要）。
  if (item.byExtensionId) return;

  // 只接管 http/https。blob:/data: 这类交给浏览器——VeloxGrab 拿不到内容。
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

  try {
    // 用浏览器已经解析出的文件名：它比我们从 URL 猜的准得多
    // （Content-Disposition 里的名字只有浏览器知道）。
    const filename = pickFilename(item);
    const ok = await send({ url: item.url, filename, dir: cfg.dir, threads: cfg.threads });

    if (!ok) {
      // VeloxGrab 没接住：浏览器那条已经被取消并抹掉了，这里不还回去的话
      // 用户点一次下载会「什么都没发生」。重新交给浏览器下载。
      // 扩展发起的下载带 byExtensionId，上面的监听器会跳过它，不会成环。
      const opts = { url: item.url, conflictAction: "uniquify" };
      if (filename) opts.filename = filename;
      try {
        await chrome.downloads.download(opts);
      } catch {
        // 仍然失败就只能靠 send() 里的剪贴板兜底了
      }
    }

    if (cfg.notify) {
      notify(
        ok ? "已转给 VeloxGrab" : "VeloxGrab 未响应，已交回浏览器下载",
        ok ? filename || item.url : "请确认 VeloxGrab 正在运行（托盘图标可见）"
      );
    }
  } finally {
    // 转发结束才解账：发送期间若因重定向再触发一次 onCreated，仍会被拦下。
    // 不删的话这个集合会在后台页生命周期内无限增长。
    canceling.delete(item.id);
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
 * 把一条下载请求交给 VeloxGrab。
 *
 * 顺序很重要：**先桥后协议**。桥是同步返回成败的，能明确告诉用户
 * 「发出去了」还是「VeloxGrab 没开」；协议导航永远是「成功」的假象
 * （Chrome 拦掉也不报错），只能当兜底。
 */
async function send(target) {
  if (!target || !target.url) return false;

  // 0. 拦在源头：VeloxGrab 只处理 http/https。
  //    blob: 是页面内存里的临时地址（MSE 播放），出不了浏览器进程；
  //    data: 同理。发过去只会得到一句「必须以 http 或 https 开头」的后端错误，
  //    而且用户还看不到（右键菜单是 fire-and-forget）。这里直接判否，
  //    交给调用方给出可操作的提示。
  if (!/^https?:/i.test(target.url)) return false;

  // 1. 本地桥
  try {
    const ctrl = new AbortController();
    // 桥是本机进程，正常几毫秒就回；2 秒还没回说明 VeloxGrab 没在跑。
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

  // 2. veloxgrab:// 协议导航（兜底）
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

/** 拼 `veloxgrab://add?url=...&name=...`。 */
function buildLink(target) {
  const parts = ["url=" + encodeURIComponent(target.url)];
  if (target.filename) parts.push("name=" + encodeURIComponent(target.filename));
  if (target.dir) parts.push("dir=" + encodeURIComponent(target.dir));
  if (target.threads) parts.push("threads=" + encodeURIComponent(String(target.threads)));
  return `${SCHEME}://add?${parts.join("&")}`;
}

/** 最后一道退路：至少让用户能把链接粘进 VeloxGrab。 */
async function fallback(link) {
  try {
    await navigator.clipboard.writeText(link);
    notify("VeloxGrab 没有响应", "下载链接已复制到剪贴板，可粘贴到 VeloxGrab 的「添加URL」");
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

/**
 * 右键菜单的统一出口。
 *
 * `send()` 是异步的，而 `contextMenus.onClicked` 不会等它——原来这里直接
 * 裸调 `send(...)` 不接 Promise，桥不通/协议被拦时的失败**完全静默**，
 * 用户只觉得「点了没反应」。这里统一 await + catch + 通知。
 */
async function handleMenuClick(info, tab) {
  const cfg = await getConfig();
  const base = { threads: cfg.threads, dir: cfg.dir };
  const pageUrl = info.pageUrl || (tab && tab.url) || "";

  let target = null;

  if (info.menuItemId === "veloxgrab-link" && info.linkUrl) {
    target = { ...base, url: info.linkUrl, filename: guessName(info.linkUrl) };
  } else if (info.menuItemId === "veloxgrab-media") {
    // 媒体右键 → 发页面地址，交给 VeloxGrab 用 yt-dlp 解析。
    // 见 setupMenus 里关于 blob: 的说明。
    if (pageUrl) {
      target = { ...base, url: pageUrl, filename: "" };
    }
  } else if (info.menuItemId === "veloxgrab-media-direct" && info.srcUrl) {
    // 只接受可直接下载的 http/https 地址；blob:/data: 明确拒绝并告诉用户
    if (/^https?:/i.test(info.srcUrl)) {
      target = { ...base, url: info.srcUrl, filename: guessName(info.srcUrl) };
    } else {
      notify(
        "这个媒体无法直接下载",
        "它是指向内存的临时地址（blob:/data:），请改用「解析并下载此视频」"
      );
      return;
    }
  } else if (info.menuItemId === "veloxgrab-page") {
    if (pageUrl) target = { ...base, url: pageUrl, filename: guessName(pageUrl) };
  }

  if (!target) {
    notify("没有可发送的链接", "这个位置没有可下载的地址");
    return;
  }

  let ok = false;
  try {
    ok = await send(target);
  } catch (e) {
    notify("发送失败", String(e).slice(0, 200));
    return;
  }
  if (!ok) {
    notify("VeloxGrab 没有响应", "请确认 VeloxGrab 正在运行（托盘图标可见）");
  }
}

chrome.contextMenus.onClicked.addListener((info, tab) => {
  handleMenuClick(info, tab).catch((e) => {
    notify("发送失败", String(e).slice(0, 200));
  });
});

// ---------- popup 消息 ----------

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (!msg || msg.type !== "veloxgrab-send") return;

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
