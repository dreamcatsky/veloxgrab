// 独立进度窗口的前端逻辑
//
// 与主窗口的 app.js 完全独立：它不共享 JS 全局，只共享 Rust 侧的 task 列表和
// 同一个 "task-progress" 事件。因此这里用最省的做法——定期拉一次 list_tasks
// 拿到进行中的任务，再靠事件即时刷新。窗口只在用户主动打开时才存在，
// 所以轮询的代价可以接受（1s 一次，且只在有活动任务时才开着）。
(function () {
  "use strict";

  const invoke = window.__TAURI__?.core?.invoke;
  const listen = window.__TAURI__?.event?.listen;

  if (!invoke) {
    document.body.innerHTML =
      '<div style="padding:24px;font-family:sans-serif;color:var(--err)">' +
      "无法连接 Tauri 运行时。</div>";
    return;
  }

  // 主题跟着主窗口走：用户在主窗口改成浅色后，进度窗口下次打开就是浅色的。
  // 首帧的缓存值由 theme.js 上好色，这里拿到 Rust 侧真值后纠正一次。
  // 用 .then 而不是 await：外层是普通 IIFE，不是 async 函数。
  invoke("get_settings")
    .then((s) => {
      if (window.VeloxGrabTheme) window.VeloxGrabTheme.apply(s.theme || "system");
    })
    .catch(() => {
      // 拿不到就保持 theme.js 的缓存值，不值得为配色让窗口打不开
    });

  const $ = (id) => document.getElementById(id);
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
  };

  function fmtBytes(b) {
    if (!b || b <= 0) return "";
    const u = ["B", "KB", "MB", "GB"];
    let i = 0,
      v = b;
    while (v >= 1024 && i < u.length - 1) {
      v /= 1024;
      i++;
    }
    return v.toFixed(v >= 100 || i === 0 ? 0 : 1) + " " + u[i];
  }

  const STATE_TEXT = {
    queued: "排队中",
    downloading: "下载中",
    paused: "已暂停",
    merging: "合并中",
  };

  // 与主窗口一致：这些都算「还在进行中」，要出现在进度窗口里
  const ACTIVE = new Set(["queued", "downloading", "paused", "merging"]);

  let tasks = [];

  function render() {
    const list = $("pwList");
    const active = tasks.filter((t) => ACTIVE.has(t.state));

    list.innerHTML = "";
    if (!active.length) {
      list.appendChild(el("div", "pw-empty", "当前没有正在下载的任务"));
    } else {
      for (const t of active) {
        const row = el("div", "pw-row");

        const top = el("div", "pw-row-top");
        top.appendChild(el("span", "pw-name", t.title || t.url || "未命名"));
        top.appendChild(el("span", "pw-state pw-" + t.state, STATE_TEXT[t.state] || t.state));
        row.appendChild(top);

        const bar = el("div", "bar");
        const fill = el("div", "bar-fill");
        const pct = t.state === "queued" ? 0 : Math.max(0, Math.min(100, t.progress || 0));
        fill.style.width = pct.toFixed(1) + "%";
        if (t.state === "paused") fill.classList.add("paused");
        bar.appendChild(fill);
        row.appendChild(bar);

        const bits = [];
        if (t.state === "queued") {
          bits.push("等待空闲槽位");
        } else {
          if (pct) bits.push(pct.toFixed(1) + "%");
          if (t.downloaded_bytes) {
            bits.push(
              t.total_bytes
                ? fmtBytes(t.downloaded_bytes) + " / " + fmtBytes(t.total_bytes)
                : fmtBytes(t.downloaded_bytes)
            );
          }
          // speed / eta 在 Rust 侧已经是格式化好的字符串（"1.2 MiB/s"、"00:42"），
          // 这里不能再套一遍 fmtBytes，否则会输出 "1.2 MiB/s/s"。
          // 只在真正跑着的时候显示：暂停后速度字段会停在「0 B/s」这个残值上，
          // 摆一行「已暂停 · 0 B/s」比不写更让人困惑。
          if (t.state === "downloading") {
            if (t.speed) bits.push(t.speed);
            if (t.eta) bits.push("剩余 " + t.eta);
          }
        }
        row.appendChild(el("div", "pw-detail", bits.join("  ·  ")));

        list.appendChild(row);
      }
    }

    const running = active.filter((t) => t.state === "downloading").length;
    const paused = active.filter((t) => t.state === "paused").length;
    const queued = active.filter((t) => t.state === "queued").length;
    const parts = [];
    if (running) parts.push(running + " 个下载中");
    if (queued) parts.push(queued + " 个排队");
    if (paused) parts.push(paused + " 个已暂停");
    $("pwSummary").textContent = parts.length ? parts.join("，") : "暂无进行中的任务";
  }

  async function refresh() {
    try {
      const list = await invoke("list_tasks");
      tasks = list || [];
      render();
    } catch (e) {
      // 窗口关闭过程中调用会失败，静默即可
    }
  }

  $("pwClose").addEventListener("click", async () => {
    try {
      const win = window.__TAURI__.window.getCurrentWindow();
      await win.close();
    } catch (e) {
      // 关不掉也不能崩
    }
  });

  if (listen) {
    listen("task-progress", (evt) => {
      const p = evt.payload;
      const i = tasks.findIndex((t) => t.id === p.id);
      if (i < 0) {
        refresh();
        return;
      }
      // 与主窗口同样的合并规则：null 表示「本轮没有这一项」，不能覆盖
      const merged = { ...tasks[i] };
      for (const [k, v] of Object.entries(p)) {
        if (v !== null && v !== undefined) merged[k] = v;
      }
      tasks[i] = merged;
      render();
    });
  }

  refresh();
  // 事件只在有进度变化时才来；任务刚入队/结束的瞬间可能没有事件，
  // 用低频轮询兜住状态跃迁，避免窗口停在过期的画面上。
  setInterval(refresh, 1000);
})();
