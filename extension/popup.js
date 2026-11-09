// VeloxGrab 扩展弹窗逻辑
//
// 打开 popup 时做四件事：
//   1. 渲染桌面端正在进行的下载任务（监视面板，2 秒轮询，可暂停/继续）；
//   2. 把当前标签页的 URL 填进输入框；
//   3. 扫描页面里的 <video>/<audio>/<img>/<a download> 与嗅探到的流媒体，列出来供一键发送；
//   4. 读 storage 里保存过的目录与线程数。
//
// 发送统一走 chrome.runtime.sendMessage → background.js：
// service worker 才有 tabs.update 权限，popup 自己导航不可靠。
// 任务面板的数据来自本地桥的 /tasks 与 /task-action（带共享 token）。

const $ = (id) => document.getElementById(id);

/** 当前页面的地址（作为 Referer 交给桌面端）。 */
let currentPageUrl = "";
let currentTabId = null;

function msg(text, isErr) {
  const m = $("msg");
  m.textContent = text || "";
  m.className = isErr ? "err" : "";
}

// ---------- 桥访问（/tasks 与 /task-action） ----------
//
// 弹窗直接 fetch 本地桥（background.js 的 send() 也走桥，但那是 fire-and-forget
// 的投递；监视面板要拿返回值，所以在 popup 里发）。token 与后台共享同一份
// bridge-token.txt——fetch chrome.runtime.getURL 读的是打包资源，与 SW 一致。

let bridgeTokenPromise = null;
function getBridgeToken() {
  if (!bridgeTokenPromise) {
    bridgeTokenPromise = fetch(chrome.runtime.getURL("bridge-token.txt"), { cache: "no-store" })
      .then((r) => r.text())
      .then((t) => t.trim())
      .catch(() => "");
  }
  return bridgeTokenPromise;
}

async function bridgeHeaders() {
  const h = { "Content-Type": "application/json" };
  const t = await getBridgeToken();
  if (t) h["X-VeloxGrab-Token"] = t;
  return h;
}

/** 桌面端是否可达（2 秒判定，与 send() 的超时一致）。 */
async function bridgeAlive() {
  try {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 2000);
    const res = await fetch("http://127.0.0.1:58943/ping", {
      method: "POST",
      headers: await bridgeHeaders(),
      body: "{}",
      cache: "no-store",
      signal: ctrl.signal,
    });
    clearTimeout(timer);
    return res.ok;
  } catch {
    return false;
  }
}

const STATE_TEXT = {
  downloading: "下载中",
  merging: "合并中",
  paused: "已暂停",
  queued: "排队中",
  failed: "失败",
  done: "已完成",
};

function fmtBytes(b) {
  if (!b || b <= 0) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0, v = b;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return v.toFixed(v >= 100 || i === 0 ? 0 : 1) + " " + u[i];
}

/** 渲染任务列表。DOM 结构见 popup.html 的 .tk 段。 */
function renderTasks(list, alive) {
  const host = $("taskList");
  host.innerHTML = "";

  if (!alive) {
    host.innerHTML = '<div class="tk-empty">VeloxGrab 未运行 — 启动后这里会显示下载进度</div>';
    $("taskAgg").textContent = "";
    return;
  }
  if (!list.length) {
    host.innerHTML = '<div class="tk-empty">暂无进行中的下载</div>';
    $("taskAgg").textContent = "";
    return;
  }

  const active = list.filter((t) => t.state === "downloading" || t.state === "merging");
  $("taskAgg").textContent = active.length ? `（${active.length} 个进行中）` : "";

  for (const t of list) {
    const row = document.createElement("div");
    row.className = "tk " + t.state;

    const r1 = document.createElement("div");
    r1.className = "tk-r1";
    const name = document.createElement("span");
    name.className = "tk-name";
    name.textContent = t.title || t.host || "下载";
    name.title = t.title || "";
    r1.appendChild(name);

    const badge = document.createElement("span");
    badge.className = "tk-badge " + t.state;
    badge.textContent = STATE_TEXT[t.state] || t.state;
    r1.appendChild(badge);

    // 行内操作：下载中/合并中 → 暂停；暂停/失败 → 继续；已完成 → 打开
    const act = t.state === "downloading" || t.state === "merging" ? "pause"
      : (t.state === "paused" || t.state === "failed") ? "resume" : null;
    if (act) {
      const btn = document.createElement("button");
      btn.className = "tk-btn";
      btn.textContent = act === "pause" ? "❚❚" : "▶";
      btn.title = act === "pause" ? "暂停" : "继续";
      btn.onclick = async () => {
        btn.disabled = true;
        try {
          const res = await fetch("http://127.0.0.1:58943/task-action", {
            method: "POST",
            headers: await bridgeHeaders(),
            body: JSON.stringify({ id: t.id, action: act }),
            cache: "no-store",
          });
          if (!res.ok) {
            const j = await res.json().catch(() => ({}));
            msg("操作失败：" + (j.error || res.status), true);
            btn.disabled = false;
          }
        } catch {
          msg("VeloxGrab 没有响应", true);
          btn.disabled = false;
        }
        refreshTasks();
      };
      r1.appendChild(btn);
    } else if (t.state === "done" && t.output_path && !t.file_missing) {
      // 已完成：给一个「打开」按钮（走桌面端的 open_task_file，经后台 SW）
      const btn = document.createElement("button");
      btn.className = "tk-btn";
      btn.textContent = "打开";
      btn.title = "打开文件";
      btn.onclick = () => {
        chrome.runtime.sendMessage({ type: "veloxgrab-open", path: t.output_path }, () => {
          void chrome.runtime.lastError; // 后台会通知失败；这里不阻塞面板
        });
      };
      r1.appendChild(btn);
    }
    row.appendChild(r1);

    const bar = document.createElement("div");
    bar.className = "tk-bar";
    const fill = document.createElement("div");
    fill.className = "tk-fill";
    const pct = t.state === "done" ? 100 : Math.max(0, Math.min(100, t.progress || 0));
    fill.style.width = pct.toFixed(1) + "%";
    bar.appendChild(fill);
    row.appendChild(bar);

    const r2 = document.createElement("div");
    r2.className = "tk-r2";
    const meta = document.createElement("span");
    meta.className = "tk-meta" + (t.state === "failed" ? " tk-err" : "");
    if (t.state === "downloading") {
      meta.textContent =
        (t.speed ? t.speed : "—") +
        (t.total_bytes ? `  ·  ${fmtBytes(t.downloaded_bytes)} / ${fmtBytes(t.total_bytes)}` : "") +
        (t.host ? `  ·  ${t.host}` : "");
    } else if (t.state === "failed") {
      meta.textContent = t.error || "下载失败";
    } else if (t.state === "done") {
      meta.textContent = t.total_bytes ? fmtBytes(t.total_bytes) : "已完成";
    } else if (t.state === "merging") {
      meta.textContent = "音视频合成中…";
    } else if (t.state === "paused") {
      meta.textContent = t.total_bytes
        ? `${fmtBytes(t.downloaded_bytes)} / ${fmtBytes(t.total_bytes)} · 断点已保留`
        : "已暂停";
    } else {
      meta.textContent = t.host || "";
    }
    r2.appendChild(meta);

    if (t.state !== "done") {
      const p = document.createElement("span");
      p.className = "tk-pct";
      p.textContent = pct > 0 ? pct.toFixed(0) + "%" : "";
      r2.appendChild(p);
    }
    row.appendChild(r2);
    host.appendChild(row);
  }
}

/** 拉一次任务快照。桌面端没起时显示占位，不报错——面板是「顺带看一眼」的。 */
async function refreshTasks() {
  let alive = false;
  let list = [];
  try {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 2500);
    const res = await fetch("http://127.0.0.1:58943/tasks", {
      method: "POST",
      headers: await bridgeHeaders(),
      body: "{}",
      cache: "no-store",
      signal: ctrl.signal,
    });
    clearTimeout(timer);
    if (res.ok) {
      const j = await res.json();
      if (j && j.ok) {
        alive = true;
        list = Array.isArray(j.tasks) ? j.tasks : [];
      }
    }
  } catch {
    // 桥不通：alive=false，渲染占位
  }
  renderTasks(list, alive);
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
      // 桌面端没在运行：让后台用 offscreen 文档拉起它（协议导航），
      // 提示用户稍后重试——桌面端起来需要几秒，立刻重发大概率还是失败。
      try {
        await chrome.runtime.sendMessage({ type: "veloxgrab-launch" });
      } catch {}
      msg("VeloxGrab 未运行，已尝试启动它——几秒后重试", true);
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

// 任务监视面板：弹窗打开期间每 2 秒刷新一次。popup 关闭时定时器随之销毁
//（MV3 的 popup 页面销毁即 JS 停止），不需要后台 SW 常驻轮询。
refreshTasks();
setInterval(refreshTasks, 2000);
