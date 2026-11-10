// VeloxGrab 扩展的后台脚本（MV3 service worker）
//
// ## 这个文件负责两件事
//
// 1. **拦截网页下载**：`chrome.downloads.onCreated` 一响就取消浏览器自己的
//    下载，转给 VeloxGrab。这是「网页点下载就调用下载软件」的实现——没有它，
//    扩展就只是个右键菜单，用户点链接仍然走浏览器下载器。
// 2. **可靠地把链接送出去**：走本地回环 HTTP 桥
//    （`http://127.0.0.1:58943/add`）。桥不通时**不做任何页面导航**——
//    拦截到的下载原样交回浏览器，右键/弹窗路径如实提示。
//
// ## 为什么不再只靠 veloxgrab://
//
// Chrome/Edge 会**静默拦截**扩展发起的自定义协议导航：
// `chrome.tabs.update({url: "veloxgrab://..."})` 既不报错也不发生，标签页原地不动。
// 于是「点了没反应」——这正是用户报的故障。协议本身是通的（注册表、单实例
// 回调、冷启动 argv 都验过），断的是「扩展 → 协议」这一步。
//
// 进一步实测（Edge 157，CDP 三组对照）：不止 tabs API，扩展页面里**一切
// 无用户手势的协议导航**（含 offscreen 文档）都被静默吞掉；只有带
// transient user activation 的导航（真实点击）才触发外部协议启动。所以
// 「SW/offscreen 自动拉起桌面端」在架构上不成立——本文件里没有也不该有
// 自动拉起代码，拉起入口只有 popup 的「启动 VeloxGrab」按钮。
//
// 回环 HTTP 桥由 VeloxGrab 自己监听（`src-tauri/src/bridge.rs`），扩展
// `fetch` 一下就行，不需要固定扩展 ID、不需要注册表、不需要额外二进制。

/** VeloxGrab 的本地桥端口，与 `src-tauri/src/bridge.rs` 的 `PORT` 必须一致。 */
const BRIDGE = "http://127.0.0.1:58943";

/** 默认配置。`intercept` 是「接管浏览器下载」总开关。 */
const DEFAULTS = { threads: 4, dir: "", intercept: true, notify: true, sniff: true };

// ---------- 深度嗅探 ----------
//
// ## 为什么需要它
//
// 只看 DOM 的 `<video src>` 抓不到真正在播的流：B 站 / 抖音 / YouTube 的
// 播放器都用 MSE（Media Source Extensions），`src` 是 `blob:`，
// `<video>` 的地址在页面内存里，任何"读页面"的方式都拿不到。但这些站点的
// **网络请求**里一定有真正的分片清单：`.m3u8`（HLS）或 `.mpd`（DASH）。
//
// 监听 `webRequest.onResponseStarted` 而不是 onCompleted：流媒体请求往往
// 是长连接 / 分片轮询，onCompleted 可能等到用户关掉页面才来；响应头一到
// 就能判定类型了。
//
// 存量按 tabId 记账、只留最近的若干条：用户可能开几十个标签页，每个页面
// 几十个分片请求，不设上限就是内存泄漏。
const SNIFF_MAX_PER_TAB = 40;
/** tabId -> [{url, kind, at}]，最近的在前 */
const sniffed = new Map();

/** 从响应 URL + content-type 判定是否是流媒体清单。 */
function classifyStream(url, contentType) {
  const u = url.split("?")[0].toLowerCase();
  const ct = (contentType || "").toLowerCase();
  if (u.endsWith(".m3u8") || ct.includes("mpegurl") || ct.includes("vnd.apple.mpegurl")) {
    return "hls";
  }
  if (u.endsWith(".mpd") || ct.includes("dash+xml")) {
    return "dash";
  }
  // 明确的分片（.ts/.m4s）不单列为候选：它们只是清单的下属分片，
  // 单独下载没有意义，交给 yt-dlp 按清单拉。
  return null;
}

/**
 * 嗅探开关。存在 storage.sync（跟账号走），默认开。
 * 读失败时按开处理——嗅探只是"多看到几个候选"，误开不会造成下载。
 */
async function sniffEnabled() {
  try {
    const s = await chrome.storage.sync.get(["sniff"]);
    return s.sniff !== false;
  } catch {
    return true;
  }
}

chrome.webRequest.onResponseStarted.addListener(
  (details) => {
    if (!details.tabId || details.tabId < 0) return;
    const kind = classifyStream(details.url, "");
    if (!kind) return;
    const list = sniffed.get(details.tabId) || [];
    // 同一条流会重复请求（分片轮询 / 重试）：只按「同 URL」去重，
    // 不同清晰度/不同码流的清单地址不同，都会保留。
    if (list.some((x) => x.url === details.url)) return;
    list.unshift({ url: details.url, kind, at: Date.now() });
    if (list.length > SNIFF_MAX_PER_TAB) list.length = SNIFF_MAX_PER_TAB;
    sniffed.set(details.tabId, list);
    // 通知 popup（如果开着）刷新候选列表
    chrome.runtime
      .sendMessage({ type: "veloxgrab-sniff-update", tabId: details.tabId })
      .catch(() => {
        // popup 没开时没有接收方，正常
      });
  },
  { urls: ["http://*/*", "https://*/*"], types: ["xmlhttprequest", "media", "other", "object"] }
);

/** 标签页关闭/导航时清掉它的嗅探记录。 */
chrome.tabs.onRemoved.addListener((tabId) => sniffed.delete(tabId));
// webNavigation 需要额外权限，这里没有申请：改用 tabs.onUpdated 的
// 「URL 变化」近似「顶层导航」——对清理嗅探记录这个用途已经足够。
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.url) sniffed.delete(tabId);
});

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
  if (a.name === PING_ALARM) {
    ping();
    checkPendingTask();
  }
});

// 每次 SW 醒来都向 VeloxGrab 报一次「我在」。
//
// 这是 VeloxGrab 判断扩展到底有没有被加载的唯一硬证据：安装器把文件拷进
// $INSTDIR\extension 只说明「装了一份副本」，用户可能从没在 chrome://extensions
// 里加载过它——那正是「装上去了但浏览器里没有扩展」的成因。
// SW 每次被唤醒（含浏览器启动、扩展刚加载）都会跑这一行，所以只要加载成功，
// VeloxGrab 那边最多几十秒内就能知道。
async function ping() {
  // 必须用 POST + JSON 而不是裸 GET。
  //
  // 裸 GET 在 Chromium 里属于「简单请求」，**不带 Origin 头**；VeloxGrab 那边正是
  // 拿 Origin 判断「这请求到底是不是扩展发来的」，于是 ping 虽然 200 了，
  // 却没被记成「扩展已加载」——用户装好了扩展，程序还在提示没装。
  // POST 带 Content-Type: application/json 会触发 CORS 预检，浏览器必定附上
  // Origin: chrome-extension://<id>，门禁才认得出它。
  //
  // body 里带上扩展版本：桌面端据此检测「浏览器里挂的还是旧版扩展」并
  // 弹提醒（旧版扩展在 VeloxGrab 未运行时会跳系统协议确认页，开新标签）。
  try {
    const res = await fetch(`${BRIDGE}/ping`, {
      method: "POST",
      headers: await bridgeHeaders(),
      body: JSON.stringify({
        ext: chrome.runtime.id,
        version: chrome.runtime.getManifest().version,
      }),
      cache: "no-store",
    });
    console.log("[veloxgrab] ping", res.status);
  } catch (e) {
    console.warn("[veloxgrab] ping failed:", String(e));
  }
}
ping();
checkPendingTask();

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
    // 深度嗅探：页面用 MSE 播放时 <video src> 是 blob:，只能从网络请求
    // 里拿到真正的流媒体清单（.m3u8 / .mpd）。嗅探开启时给一条显式入口。
    chrome.contextMenus.create({
      id: "veloxgrab-sniff-open",
      title: "下载嗅探到的流媒体（HLS/DASH）",
      contexts: ["page", "video"],
    });
  });
}

// ---------- 下载拦截 ----------

/**
 * 正在被我们取消的下载 id。
 *
 * `chrome.downloads.cancel()` 是异步的，取消过程中 `onCreated` 可能因为
 * 重定向再触发一次；不记账就会把同一条下载转给 VeloxGrab 两遍。
 *
 * 账本存 storage.session 而不是内存 Set：MV3 SW 可能在「取消 + 转发」的
 * 长 await 期间被回收，重启后内存集合清空，重定向触发的第二次 onCreated
 * 会把同一条下载再转发一遍。storage.session 的生命周期 = 浏览器会话，
 * 恰好覆盖「拦截 → 转发」窗口。
 */
async function isCanceling(id) {
  const { canceling = [] } = await chrome.storage.session.get("canceling");
  return canceling.includes(id);
}

async function setCanceling(id, on) {
  const { canceling = [] } = await chrome.storage.session.get("canceling");
  const next = on ? [...new Set([...canceling, id])] : canceling.filter((x) => x !== id);
  await chrome.storage.session.set({ canceling: next });
}

chrome.downloads.onCreated.addListener(async (item) => {
  if (!item || !item.id) return;

  // 扩展自己发起的下载不拦（将来若做「用浏览器下载」按钮需要）。
  if (item.byExtensionId) return;

  // 只接管 http/https。blob:/data: 这类交给浏览器——VeloxGrab 拿不到内容。
  if (!/^https?:/i.test(item.url || "")) return;

  const cfg = await getConfig();
  if (!cfg.intercept) return;

  if (await isCanceling(item.id)) return;
  await setCanceling(item.id, true);

  try {
    await chrome.downloads.cancel(item.id);
  } catch {
    // 取消失败（下载已经结束）就别再转发了，否则用户会看到两条。
    await setCanceling(item.id, false);
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
    const referer = referrerFor(item, "");
    const result = await send({
      url: item.url,
      filename,
      dir: cfg.dir,
      threads: cfg.threads,
      referer,
    });

    if (result.ok === false) {
      // VeloxGrab 彻底没接住（桌面端缺席且拉起失败/超时）：浏览器那条已经
      // 被取消并抹掉了，不还回去的话用户点一次下载会「什么都没发生」。
      // 重新交给浏览器下载。扩展发起的下载带 byExtensionId，上面的监听器
      // 会跳过它，不会成环。
      const opts = { url: item.url, conflictAction: "uniquify" };
      if (filename) opts.filename = filename;
      try {
        await chrome.downloads.download(opts);
      } catch {
        // 仍然失败就只能靠 send() 里的剪贴板兜底了
      }
    }
    if (cfg.notify) {
      if (result.ok === true) {
        notify("已转给 VeloxGrab", filename || item.url);
      } else {
        // 诊断串跟通知走：用户截一张通知就能定位断在哪一环
        notify(
          "VeloxGrab 未响应，已交回浏览器下载",
          `${result.diag}；可手动启动 VeloxGrab 后再试`
        );
      }
    }
  } finally {
    // 转发结束才解账：发送期间若因重定向再触发一次 onCreated，仍会被拦下。
    await setCanceling(item.id, false);
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

/**
 * 给浏览器下载补上 Referer。
 *
 * 浏览器自己发起下载时天然带页面 Referer；我们取消后转给 VeloxGrab，
 * 那条请求由桌面端重发——很多防盗链站点（尤其是对象存储的签名直链）
 * 只认「Referer 在白名单里」。`item.referrer` 是 Chrome 从
 * `chrome.downloads.DownloadItem` 给出的下载来源页，MS 直链下载
 * 必须原样带上。
 */
function referrerFor(item, fallbackPageUrl) {
  const r = (item && item.referrer) || "";
  if (/^https?:/i.test(r)) return r;
  return /^https?:/i.test(fallbackPageUrl || "") ? fallbackPageUrl : "";
}

/** 嗅探到的流媒体候选（按 tabId 取，最近的在前）。 */
async function sniffedFor(tabId) {
  if (!tabId || tabId < 0) return [];
  if (!(await sniffEnabled())) return [];
  return sniffed.get(tabId) || [];
}

// ---------- 配置 ----------

function getConfig() {
  // threads/intercept/notify 跟账号走 storage.sync；dir 是本机路径，
  // 放 storage.local——同步到别的设备毫无意义，还把本机目录结构外泄上云。
  return Promise.all([
    new Promise((resolve) => chrome.storage.sync.get(["threads", "intercept", "notify"], resolve)),
    new Promise((resolve) => chrome.storage.local.get(["dir"], resolve)),
  ]).then(([s, l]) => ({ ...DEFAULTS, ...s, dir: l.dir || "" }));
}

// ---------- 桥 token ----------
//
// 桥要求请求携带共享 token（bridge-token.txt 随扩展打包，桌面端从同一文件读）。
// Origin 门禁只认 scheme 前缀——任何已装扩展都能通过；token 挡的是「照抄公开
// 值就能驱动下载器」的抄写型扩展：包里的这份只是公开引导值，VeloxGrab 首次
// 运行会把它替换成每安装随机值（本扩展沙箱读不到桌面端的配置目录，抄不到）。
// 每次运行都现读文件不缓存：桌面端自更新覆盖本文件后，SW 下一轮醒来拿到的
// 就是新值。旧版扩展不带这个头：桌面端回 403 + 更新提示，此时只剩协议/剪贴板兜底。
let bridgeToken = null;

async function getBridgeToken() {
  if (bridgeToken !== null) return bridgeToken;
  try {
    const res = await fetch(chrome.runtime.getURL("bridge-token.txt"), { cache: "no-store" });
    bridgeToken = (await res.text()).trim();
  } catch {
    bridgeToken = "";
  }
  return bridgeToken;
}

/** 桥请求的公共头：JSON + 共享 token。 */
async function bridgeHeaders() {
  const h = { "Content-Type": "application/json" };
  const t = await getBridgeToken();
  if (t) h["X-VeloxGrab-Token"] = t;
  return h;
}

// ---------- 发送 ----------

/**
 * 把一条下载请求交给 VeloxGrab。
 *
 * 返回 `{ ok, diag }`：
 * - `ok: true`  桥确认送达，可以放心告诉用户「发出去了」；
 * - `ok: false` 没送达。拦截路径会把下载交回浏览器，右键/弹窗路径如实提示
 *   ——**绝不**开新标签页、不改当前页。
 * - `diag` 失败链路的一行诊断（空格分隔 token，如
 *   `bridge:down poll:never-up needs-manual-launch`）：bridge = 首次直发
 *   结果，poll = 20 秒后台轮询结果。它同时进通知与弹窗错误面板——
 *   「点了没反应」这类故障必须能从面板读出断在哪一环，否则每轮排查都
 *   只能靠猜。
 *
 * 顺序很重要：**先桥后协议**。桥是同步返回成败的；协议只是兜底。
 */
async function send(target) {
  if (!target || !target.url) return { ok: false, diag: "no-url" };

  // 0. 拦在源头：VeloxGrab 只处理 http/https。
  //    blob: 是页面内存里的临时地址（MSE 播放），出不了浏览器进程；
  //    data: 同理。发过去只会得到一句「必须以 http 或 https 开头」的后端错误，
  //    而且用户还看不到（右键菜单是 fire-and-forget）。这里直接判否，
  //    交给调用方给出可操作的提示。
  if (!/^https?:/i.test(target.url)) return { ok: false, diag: "scheme-not-http" };

  // 1. 本地桥
  try {
    const ctrl = new AbortController();
    // 桥是本机进程，正常几毫秒就回；2 秒还没回说明 VeloxGrab 没在跑。
    const timer = setTimeout(() => ctrl.abort(), 2000);
    const res = await fetch(`${BRIDGE}/add`, {
      method: "POST",
      headers: await bridgeHeaders(),
      body: JSON.stringify({
        url: target.url,
        name: target.filename || "",
        dir: target.dir || "",
        threads: target.threads || 0,
        // 防盗链直链需要页面 Referer；空串由桌面端忽略
        referer: target.referer || "",
      }),
      signal: ctrl.signal,
    });
    clearTimeout(timer);
    if (res.ok) {
      const j = await res.json().catch(() => ({}));
      if (j && j.ok) return { ok: true, diag: "bridge:direct" };
      // 403 + token mismatch 说明桌面端比扩展新（要求共享 token），把原因
      // 带给用户，别让「点了没反应」重演。
      if (j && j.error && /token/i.test(String(j.error))) {
        notify("VeloxGrab 已更新", "请到 chrome://extensions 刷新 VeloxGrab 扩展后再试");
        return { ok: false, diag: "bridge:token-mismatch" };
      }
      // 桥在跑但业务层拒了这条（2xx + ok:false）：进程活着，再拉起无济于事，
      // 直接带诊断返回。
      return { ok: false, diag: `bridge:http-${res.status}-reject` };
    }
    // 桥在跑但回了非 2xx（Origin 门禁等）：进程明明活着，拉起毫无意义。
    return { ok: false, diag: `bridge:http-${res.status}` };
  } catch {
    // 桥不可用 → 走协议拉起
  }

  // 2. 桥不通：VeloxGrab 没在运行。
  //
  //    **扩展无法自动拉起桌面端**。实测（Edge 157，2026-11-11，隔离实例 +
  //    CDP 三组对照）：offscreen 文档导航 `veloxgrab://launch` 返回成功但
  //    协议分发静默不发生；可见扩展页的程序化导航同样被吞；只有**用户手势
  //    （真实点击）**触发的外部协议启动才放行——Chromium 对扩展页面发起的
  //    外部协议要求 transient user activation，offscreen 永远没有手势，
  //    「SW 自动拉起」在架构上不成立。
  //
  //    所以这里的兜底是：20 秒后台轮询（覆盖「用户看到通知后顺手启动了
  //    VeloxGrab」的情况——起来即自动补交，用户无感）；窗口内没起来，把
  //    任务暂存进 storage.local 并如实上报 diag，弹窗给出「启动 VeloxGrab
  //    并补交」按钮（点击有 user activation，协议放行）。暂存任务另由
  //    5 分钟一次的 ping 闹钟兜底：之后任何时刻桥通了都自动补交。
  const poll = await waitAndResend(target, 20_000);
  if (poll.ok) {
    await unstashIfSame(target.url); // 上一轮失败留下的同名暂存不能再补交一遍
    return { ok: true, diag: "bridge:after-launch" };
  }
  await stashTask(target);
  return {
    ok: false,
    diag: `bridge:down poll:${poll.detail} needs-manual-launch`,
  };
}

// ---------- 任务暂存（发送失败后留给「之后桥通了」的兜底） ----------
//
// 暂存只服务长尾场景：send() 的 20 秒轮询窗口内用户没启动桌面端、之后才
// 启动（手动或点弹窗按钮）。storage.local 比 session 存活期长（浏览器重启
// 后仍在）；目录/线程等参数随任务固化，可接受。消费方有两处：
//  - popup 的「启动 VeloxGrab 并补交」按钮（立即补交）；
//  - checkPendingTask()（ping 闹钟顺带检查，5 分钟粒度的兜底）。
const PENDING_KEY = "pendingTask";
const PENDING_MAX_AGE_MS = 24 * 3600 * 1000;

async function stashTask(target) {
  try {
    await chrome.storage.local.set({ [PENDING_KEY]: { ...target, at: Date.now() } });
  } catch {
    // 暂存失败不影响主流程：用户手动重试即可
  }
}

async function unstashTask() {
  try {
    await chrome.storage.local.remove(PENDING_KEY);
  } catch {
    // 忽略
  }
}

/** 成功送达时清理暂存——只清与当前任务同 URL 的，别把用户更早的另一条吞掉。 */
async function unstashIfSame(url) {
  try {
    const { [PENDING_KEY]: pending } = await chrome.storage.local.get(PENDING_KEY);
    if (pending && pending.url === url) await chrome.storage.local.remove(PENDING_KEY);
  } catch {
    // 忽略
  }
}

/**
 * 桥恢复时补交暂存任务。消费方：
 *  - popup 的「启动 VeloxGrab 并补交」按钮点击后（resendPendingLoop 密集轮询）；
 *  - ping 闹钟（5 分钟粒度的长尾兜底）。
 * 返回 true = 已送达并清暂存。任务超过 24 小时作废（URL 里的一次性签名
 * 直链多半已过期，补交只会换来一个下载失败）。
 */
async function tryDeliverPending() {
  try {
    const { [PENDING_KEY]: pending } = await chrome.storage.local.get(PENDING_KEY);
    if (!pending || !pending.url) return false;
    if (Date.now() - (pending.at || 0) > PENDING_MAX_AGE_MS) {
      await chrome.storage.local.remove(PENDING_KEY);
      return false;
    }
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 1500);
    const res = await fetch(`${BRIDGE}/add`, {
      method: "POST",
      headers: await bridgeHeaders(),
      body: JSON.stringify({
        url: pending.url,
        name: pending.filename || "",
        dir: pending.dir || "",
        threads: pending.threads || 0,
        referer: pending.referer || "",
      }),
      signal: ctrl.signal,
    });
    clearTimeout(timer);
    if (res.ok) {
      const j = await res.json().catch(() => ({}));
      if (j && j.ok) {
        await chrome.storage.local.remove(PENDING_KEY);
        notify("已补交上次未送出的任务", pending.filename || pending.url);
        return true;
      }
    }
  } catch {
    // 桥不通：调用方按未送达处理
  }
  return false;
}

async function checkPendingTask() {
  await tryDeliverPending();
}

/**
 * 密集补交：popup 点了「启动」按钮后调。桌面端冷启动 1~3 秒监听桥，
 * 1 秒间隔 + 90 秒上限足够慢机器；期间 SW 被这条消息唤醒，不会睡过去。
 */
let resendPendingRunning = false;
async function resendPendingLoop(timeoutMs) {
  if (resendPendingRunning) return;
  resendPendingRunning = true;
  try {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (await tryDeliverPending()) return;
      await new Promise((r) => setTimeout(r, 1000));
    }
  } finally {
    resendPendingRunning = false;
  }
}

/**
 * 拉起桌面端后轮询桥，就绪即补交同一条任务。
 *
 * 轮询间隔 800ms（桥监听后的 /add 是毫秒级往返，不值得更密）；到上限仍
 * 不通才放弃。发送复用 /add 的 JSON 通道——与首次尝试完全同一参数，
 * 避免「重试的下载丢字段」这类分叉。
 *
 * 返回 `{ ok, detail }`；`detail` 供 diag 使用：`delivered` = 补交成功；
 * `up-http-<status>` = 轮询期间桥回过 HTTP（进程活着、端口在监听，没送达
 * 是应用侧拒绝而非拉起失败）；`never-up` = 整个窗口桥都没起来。
 */
async function waitAndResend(target, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let sawStatus = 0;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 800));
    try {
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), 1500);
      const res = await fetch(`${BRIDGE}/add`, {
        method: "POST",
        headers: await bridgeHeaders(),
        body: JSON.stringify({
          url: target.url,
          name: target.filename || "",
          dir: target.dir || "",
          threads: target.threads || 0,
          referer: target.referer || "",
        }),
        signal: ctrl.signal,
      });
      clearTimeout(timer);
      if (res.ok) {
        const j = await res.json().catch(() => ({}));
        if (j && j.ok) return { ok: true, detail: "delivered" };
      }
      sawStatus = sawStatus || res.status;
    } catch {
      // 还没起：继续等
    }
  }
  return { ok: false, detail: sawStatus ? `up-http-${sawStatus}` : "never-up" };
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
  } else if (info.menuItemId === "veloxgrab-sniff-open") {
    const list = await sniffedFor(tab && tab.id);
    if (!list.length) {
      notify("没有嗅探到流媒体", "打开页面并开始播放后再试一次");
      return;
    }
    // 最近的优先；把清单地址发给桌面端「另存为」走一轮确认
    const first = list[0];
    target = { ...base, url: first.url, filename: guessName(first.url) };
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

  let result = { ok: false, diag: "no-target" };
  try {
    result = await send(target);
  } catch (e) {
    notify("发送失败", String(e).slice(0, 200));
    return;
  }
  if (result.ok === true) {
    // 桥已确认（含拉起后自动补交成功），不用打扰用户
  } else {
    // send() 的 20 秒轮询窗口内用户没启动桌面端：把诊断串交给用户。
    // 弹窗会显示「启动 VeloxGrab」按钮（点击带 user activation，协议启动
    // 放行——见 send() 里的实验记录）；右键菜单路径只能靠通知提示手动启动。
    notify(
      "VeloxGrab 未能启动",
      `${result.diag}；点击扩展图标，用弹窗里的「启动 VeloxGrab」按钮`
    );
  }
}

chrome.contextMenus.onClicked.addListener((info, tab) => {
  handleMenuClick(info, tab).catch((e) => {
    notify("发送失败", String(e).slice(0, 200));
  });
});

// ---------- popup 消息 ----------

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (!msg) return;
  // popup 的「启动 VeloxGrab」按钮：先唤醒 SW 起密集补交循环（按钮点击
  // 的导航会顶掉 popup 页面，弹窗自己等不了），按钮再同步导航到协议 URL。
  // 拉起必须由**带 user activation 的导航**触发（实验：Edge 157 对扩展页面
  // 的外部协议启动要求用户手势；offscreen/程序化导航一律被静默吞掉），
  // 所以导航只能发生在 popup 的点击事件栈里，SW 代劳不了。
  if (msg.type === "veloxgrab-resend-pending") {
    resendPendingLoop(90_000).catch(() => {});
    sendResponse({ ok: true });
    return true;
  }
  // popup 查询当前页签嗅探到的流媒体候选
  if (msg.type === "veloxgrab-sniff-query") {
    sniffedFor(msg.tabId).then((list) => sendResponse({ list }));
    return true;
  }
  // popup 监视面板的「打开文件」：桌面端 open_task_file 是 Tauri 命令，
  // 扩展页面拿不到 invoke，只能经后台转一个专用桥请求。桌面端没有直接
  // 暴露这条 REST——用 /task-action 之外的路径会 404，所以走 add 的
  // JSON 通道之外新开一条 /open-file（见 bridge.rs）。失败静默：面板是顺带功能。
  if (msg.type === "veloxgrab-open") {
    (async () => {
      try {
        await fetch("http://127.0.0.1:58943/open-file", {
          method: "POST",
          headers: await bridgeHeaders(),
          body: JSON.stringify({ path: msg.path }),
          cache: "no-store",
        });
      } catch {
        // 桌面端没起：面板下次轮询会显示占位
      }
      sendResponse({ ok: true });
    })();
    return true;
  }
  if (msg.type !== "veloxgrab-send") return;
  // 只认自家 popup 的消息。当前 manifest 没有 externally_connectable /
  // content_scripts，网页与第三方扩展本就触不到这里；这行是纵深——
  // 日后放开任何一条通道时，这个「URL → 本地下载器」的投递口不该裸奔。
  if (sender.id !== chrome.runtime.id) return;

  (async () => {
    const cfg = await getConfig();
    const result = await send({
      url: msg.url,
      filename: msg.filename || guessName(msg.url),
      threads: cfg.threads,
      dir: cfg.dir,
      referer: msg.referer || "",
    });
    // diag 一并回给 popup：错误面板要显示「断在哪一环」，不只是「失败」
    sendResponse({ ok: result.ok, diag: result.diag });
  })();

  // 异步响应必须返回 true，否则通道会被立刻关掉
  return true;
});
