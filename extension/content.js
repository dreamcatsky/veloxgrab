// VeloxGrab 悬浮下载按钮：跟随页面里的 <video> 元素。
//
// ## 为什么用 content script 而不是右键菜单
//
// 右键菜单要求用户「先知道 VeloxGrab 能下视频」；嗅探面板只认网络层
// 抓到的 .m3u8/.mpd 清单，页面里直接内嵌的 <video> 不经过清单请求，
// 嗅探抓不到。悬浮按钮把入口放到视频旁边——看见就能点，与
// Ghost-Downloader-3 的跟随式按钮同一形态。
//
// ## 样式隔离
//
// 按钮整体挂在 **Shadow DOM** 里：页面的 CSS 摸不到按钮（不被站点样式
// 污染成不可见/错位），按钮的样式也漏不出去污染页面。
//
// ## 点击后发什么 URL
//
// 与右键菜单同一套判据的镜像版：
// - `src` 是 http(s) 且带媒体扩展名（.mp4/.webm/…）→ 真实的渐进式视频
//   文件，**直发 src**（最快，不必解析页面）；
// - `src` 是 blob:/data:（MSE 播放，B 站/YouTube/抖音都是）→ 发**页面
//   地址**走 yt-dlp 解析，拿完整清晰度与音轨。blob: 出不了浏览器进程，
//   直发必被桌面端拒绝——右键菜单里踩过的坑，这里直接分流。
//
// ## 不做什么
//
// - 不做全屏（按钮宿主在 documentElement 下，不在 fullscreenElement
//   子树里，全屏时天然不可见——看视频的人本来也不在下载）
// - 不碰 <audio>（右键菜单「直接下载此媒体文件」已覆盖）
// - 尺寸过小（<200×150）的 video 视为广告位/埋点，跳过

(() => {
  "use strict";
  if (window.__veloxgrabFloatBtn) return; // 防重入
  window.__veloxgrabFloatBtn = true;

  const MIN_W = 200;
  const MIN_H = 150;
  const DIRECT_EXT =
    /\.(mp4|webm|m4v|mov|mkv|avi|flv|ts|m2ts|mpg|mpeg|3gp|ogv)(\?|#|$)/i;

  const states = new Map(); // video 元素 -> 状态（video/host/btn/shown）
  let rafId = 0;

  // 开关（popup 设置，storage.sync，与 intercept/sniff 同族）。默认开；
  // 关掉立即撤掉所有已亮的按钮。storage 不可用（极老内核）时按钮仍可用，
  // 只是没有开关。
  let enabled = true;
  function applyEnabled() {
    if (!enabled) return;
    for (const st of states.values()) {
      if (st.btn && !st.video.matches(":hover")) hide(st);
    }
  }
  try {
    chrome.storage.sync.get(["floatBtn"]).then((s) => {
      enabled = s.floatBtn !== false;
      applyEnabled();
    });
    chrome.storage.onChanged.addListener((changes) => {
      if (changes.floatBtn) {
        enabled = changes.floatBtn.newValue !== false;
        applyEnabled();
      }
    });
  } catch {}

  function isDirectFile(src) {
    return /^https?:/i.test(src) && DIRECT_EXT.test(src);
  }

  function createButton(video) {
    const host = document.createElement("div");
    host.style.cssText =
      "all:initial;position:fixed;top:0;left:0;width:0;height:0;z-index:2147483647;pointer-events:none;";
    const root = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      .vgx-btn {
        all: initial; position: fixed; display: flex; align-items: center; gap: 5px;
        padding: 6px 12px; border-radius: 999px; cursor: pointer; pointer-events: auto;
        background: rgba(15, 18, 26, .88); color: #fff; font: 600 12.5px/1 system-ui, sans-serif;
        border: 1px solid rgba(255,255,255,.18); box-shadow: 0 4px 14px rgba(0,0,0,.35);
        opacity: 0; visibility: hidden; transform: translateY(3px);
        transition: opacity .15s ease, transform .15s ease, visibility .15s;
        white-space: nowrap; user-select: none; -webkit-user-select: none;
      }
      .vgx-btn.show { opacity: 1; visibility: visible; transform: none; }
      .vgx-btn:hover { background: rgba(88, 101, 236, .92); }
      .vgx-btn.sent { background: rgba(46, 160, 67, .92); }
      .vgx-btn.fail { background: rgba(218, 54, 51, .92); }
    `;
    const btn = document.createElement("button");
    btn.className = "vgx-btn";
    btn.textContent = "⬇ VeloxGrab";
    btn.title = "用 VeloxGrab 下载此视频";
    btn.addEventListener("pointerenter", () => btn.classList.add("show"));
    btn.addEventListener("pointerleave", () => {
      const st = states.get(video);
      if (st && !video.matches(":hover")) hide(st);
    });
    btn.addEventListener("click", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      submit(video, btn);
    });
    root.append(style, btn);
    document.documentElement.appendChild(host);
    return { host, btn };
  }

  function show(video) {
    if (!enabled) return;
    let st = states.get(video);
    if (!st) return; // scan() 负责注册；未注册的悬停等下一轮 MutationObserver
    if (!st.host) Object.assign(st, createButton(video));
    st.shown = true;
    st.btn.textContent = isDirectFile(video.currentSrc || video.src || "")
      ? "⬇ 直链下载"
      : "⬇ 用 VeloxGrab 解析";
    place(st);
    st.btn.classList.add("show");
    startLoop();
  }

  function hide(st) {
    if (!st || !st.btn) return;
    st.shown = false;
    st.btn.classList.remove("show");
  }

  // 把按钮放到视频右上角内侧，fixed 定位跟随视口坐标。rAF 循环驱动
  // （滚动/布局变化/其他元素挤压都会改变 rect），只在至少一个按钮可见
  // 时轮询，成本可忽略。
  function place(st) {
    const r = st.video.getBoundingClientRect();
    const vw = innerWidth, vh = innerHeight;
    if (r.width < MIN_W || r.height < MIN_H || r.bottom < 0 || r.top > vh ||
        r.right < 0 || r.left > vw) {
      st.btn.style.opacity = "0";
      st.btn.style.pointerEvents = "none";
      return;
    }
    st.btn.style.opacity = "";
    st.btn.style.pointerEvents = "";
    const pad = 10;
    const bw = st.btn.offsetWidth || 120;
    const bh = st.btn.offsetHeight || 30;
    let x = r.right - bw - pad;
    let y = r.top + pad;
    if (x < 4) x = Math.max(4, r.left + pad);
    if (y < 4) y = Math.max(4, r.bottom - bh - pad);
    st.btn.style.left = `${Math.round(x)}px`;
    st.btn.style.top = `${Math.round(y)}px`;
  }

  function startLoop() {
    if (rafId) return;
    const tick = () => {
      let any = false;
      for (const st of states.values()) {
        if (st.shown) { any = true; place(st); }
      }
      rafId = any ? requestAnimationFrame(tick) : 0;
    };
    rafId = requestAnimationFrame(tick);
  }

  async function submit(video, btn) {
    const src = video.currentSrc || video.src || "";
    // blob:/data: 与直链分流，判据见文件头注释
    const url = isDirectFile(src) ? src : location.href;
    const prev = btn.textContent;
    btn.textContent = "发送中…";
    try {
      const res = await chrome.runtime.sendMessage({
        type: "veloxgrab-send",
        url,
        referer: location.href,
      });
      if (res && res.ok) {
        btn.classList.add("sent");
        btn.textContent = "已发送 ✓";
      } else {
        btn.classList.add("fail");
        btn.textContent = "未送达";
      }
    } catch {
      btn.classList.add("fail");
      btn.textContent = "未送达";
    }
    setTimeout(() => {
      const st = states.get(video);
      btn.className = "vgx-btn" + (st && st.shown ? " show" : "");
      btn.textContent = prev;
    }, 1400);
  }

  // 视频集合维护：初次扫描 + DOM 变化监听。移除的 video 连按钮一起回收。
  function scan() {
    document.querySelectorAll("video").forEach((v) => {
      if (!states.has(v)) {
        states.set(v, { video: v, host: null, btn: null, shown: false });
        if (v.matches(":hover")) show(v);
      }
    });
    for (const [v, st] of states) {
      if (!v.isConnected) {
        if (st.host) st.host.remove();
        states.delete(v);
      }
    }
  }
  scan();
  new MutationObserver(scan).observe(document.documentElement, {
    childList: true, subtree: true,
  });

  // 悬停入口：pointerenter 在 video 上亮按钮，离开（且指针不在按钮上）后
  // 收回。用捕获式委托挂 document 而不是逐 video 挂监听：SPA 动态换视频
  // 元素的站点不用重新绑。
  document.addEventListener(
    "pointerenter",
    (ev) => {
      const v = ev.target && ev.target.tagName === "VIDEO" ? ev.target : null;
      if (v) show(v);
    },
    true,
  );
  document.addEventListener(
    "pointerleave",
    (ev) => {
      const v = ev.target && ev.target.tagName === "VIDEO" ? ev.target : null;
      if (!v) return;
      const st = states.get(v);
      // 指针滑到按钮上不算离开：按钮贴着视频右上角，短暂重叠由 120ms
      // 宽限吸收；离开超时后按钮仍在 :hover 则说明指针在按钮上
      setTimeout(() => {
        if (st && st.shown && !v.matches(":hover") &&
            !(st.btn && st.btn.matches(":hover"))) hide(st);
      }, 120);
    },
    true,
  );
})();
