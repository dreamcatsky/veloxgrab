// VeloxGrab 扩展弹窗逻辑
//
// 打开 popup 时做三件事：
//   1. 把当前标签页的 URL 填进输入框；
//   2. 扫描页面里的 <video>/<audio>/<img>/<a download>，列出来供一键发送；
//   3. 读 storage 里保存过的目录与线程数。
//
// 发送统一走 chrome.runtime.sendMessage → background.js：
// service worker 才有 tabs.update 权限，popup 自己导航不可靠。

const $ = (id) => document.getElementById(id);

/** 当前页面的地址（作为 Referer 交给桌面端）。 */
let currentPageUrl = "";
let currentTabId = null;

function msg(text, isErr) {
  const m = $("msg");
  m.textContent = text || "";
  m.className = isErr ? "err" : "";
}

/** 渲染嗅探到的流媒体清单。 */
async function refreshSniff() {
  if (!currentTabId || currentTabId < 0) return;
  let list = [];
  try {
    const r = await chrome.runtime.sendMessage({
      type: "veloxgrab-sniff-query",
      tabId: currentTabId,
    });
    list = (r && r.list) || [];
  } catch {
    list = [];
  }
  const wrap = $("sniffWrap");
  const host = $("sniffList");
  host.innerHTML = "";
  if (!list.length) {
    wrap.style.display = "none";
    return;
  }
  wrap.style.display = "";
  list.forEach((it) => {
    const row = document.createElement("div");
    const tag = it.kind === "dash" ? "DASH" : "HLS";
    row.textContent = `[${tag}] ${it.url.length > 84 ? it.url.slice(0, 84) + "…" : it.url}`;
    row.title = it.url;
    row.onclick = () => send(it.url);
    host.appendChild(row);
  });
}

async function init() {
  // 恢复上次的目录/线程/接管开关。dir 存 storage.local（本机路径不上云），
  // threads/intercept 存 storage.sync。
  const [cfgSync, cfgLocal] = await Promise.all([
    chrome.storage.sync.get(["threads", "intercept", "sniff"]),
    chrome.storage.local.get(["dir"]),
  ]);
  if (cfgLocal.dir) $("dir").value = cfgLocal.dir;
  if (cfgSync.threads) $("threads").value = String(cfgSync.threads);
  // 默认接管：这是用户装扩展最想要的行为，未设置过就当开启。
  $("intercept").checked = cfgSync.intercept !== false;
  $("intercept").onchange = () => {
    chrome.storage.sync.set({ intercept: $("intercept").checked });
    msg($("intercept").checked ? "已开启：网页下载将转给 VeloxGrab" : "已关闭：网页下载走浏览器");
  };
  // 深度嗅探开关（默认开）：拦截网络请求里的 .m3u8/.mpd 清单
  $("sniff").checked = cfgSync.sniff !== false;
  $("sniff").onchange = () => {
    chrome.storage.sync.set({ sniff: $("sniff").checked });
    msg($("sniff").checked ? "已开启流媒体嗅探" : "已关闭流媒体嗅探");
    if (!$("sniff").checked) $("sniffWrap").style.display = "none";
    else refreshSniff();
  };

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab && tab.url) $("url").value = tab.url;
  // 记住当前页面的地址作为 Referer：防盗链直链必须带它
  currentPageUrl = (tab && tab.url) || "";
  currentTabId = tab && tab.id;

  // 嗅探到的流媒体清单（HLS/DASH）：优先用这一栏，它看不到 DOM 的 blob: 流
  refreshSniff();
  // 页面播起来之后清单才会出现：popup 开着时跟着后台推送刷新
  chrome.runtime.onMessage?.addListener?.((m) => {
    if (m && m.type === "veloxgrab-sniff-update") refreshSniff();
  });

  // 媒体列表：需要 scripting 权限之外的折中——用 executeScript 在页面上下文里抓
  if (tab && tab.id && /^https?:/.test(tab.url || "")) {
    try {
      const [res] = await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        func: () => {
          const out = [];
          document.querySelectorAll("video, audio").forEach((v) => {
            const src = v.currentSrc || v.src;
            if (src) out.push(src);
          });
          document.querySelectorAll("img").forEach((i) => {
            if (i.src && i.src.startsWith("http")) out.push(i.src);
          });
          document.querySelectorAll("a[download], a[href]").forEach((a) => {
            const href = a.href || "";
            // 只收看起来像文件直链的，避免把导航链接全塞进来
            if (/\.(zip|rar|7z|mp4|mkv|mov|webm|mp3|flac|wav|pdf|iso|exe|msi|apk|dmg|jpg|png|gif|webp)(\?|#|$)/i.test(href)) {
              out.push(href);
            }
          });
          return [...new Set(out)].slice(0, 60);
        },
      });
      const list = (res && res.result) || [];
      if (list.length) {
        $("mediaWrap").style.display = "";
        const host = $("mediaList");
        list.forEach((src) => {
          const item = document.createElement("div");
          item.textContent = src.length > 90 ? src.slice(0, 90) + "…" : src;
          item.title = src;
          item.onclick = () => send(src);
          host.appendChild(item);
        });
      }
    } catch {
      // 页面受限（chrome://、Web Store 等）抓不到，不影响手动输入
    }
  }
}

async function send(url) {
  const target = ($("url").value || "").trim();
  const dir = $("dir").value.trim();
  const threads = Number($("threads").value) || 4;

  // dir 本机存 local，threads 同步存 sync（与 background.getConfig 对应）
  await Promise.all([
    chrome.storage.local.set({ dir }),
    chrome.storage.sync.set({ threads }),
  ]);

  const btn = $("send");
  btn.disabled = true;
  msg("发送中…");
  try {
    const res = await chrome.runtime.sendMessage({
      type: "veloxgrab-send",
      url: url || target,
      dir,
      threads,
      // 防盗链直链必须带页面 Referer；用户点的是本页里的链接/流
      referer: currentPageUrl,
    });
    if (res && res.ok === true) {
      msg("已发送给 VeloxGrab");
      setTimeout(() => window.close(), 700);
    } else {
      msg("VeloxGrab 没有响应，请确认它正在运行（托盘图标可见）", true);
    }
  } catch (e) {
    msg("发送失败：" + String(e), true);
  } finally {
    btn.disabled = false;
  }
}

$("send").onclick = () => send();
$("url").addEventListener("keydown", (e) => {
  if (e.key === "Enter") send();
});

init();
