// 下载监视框：逐任务列表（对齐 GD3 浏览器插件的监视面板）。
//
// 显示的是**所有非终态任务**，按状态排序（下载中 → 合并中 → 暂停 → 排队），
// 每行：类型图标 + 名称 + 状态徽标 + 进度条 + 速度/大小 + 行内操作
// （下载中可暂停，暂停/失败可继续，已完成可打开/定位）。
//
// 数据来源与主窗口完全一致：`list_tasks` 全量快照 + `task-progress` 增量事件。
// 本窗口没有自己的状态，一切以 Rust 侧任务表为准——所以这里不做乐观更新，
// 按钮点完等下一次快照/事件刷新，避免「界面显示已暂停、实际还在跑」。
(function () {
  "use strict";

  const invoke = window.__TAURI__?.core?.invoke;
  const listen = window.__TAURI__?.event?.listen;

  if (!invoke) {
    document.body.textContent = "无法连接 Tauri 运行时。";
    return;
  }

  const $ = (id) => document.getElementById(id);

  // 与主窗口同一套状态文案；监视窗里用短标签，避免一个小窗放不下
  const STATE_TEXT = {
    queued: "排队中",
    downloading: "下载中",
    merging: "合并中",
    paused: "已暂停",
    failed: "失败",
    done: "已完成",
    canceled: "已取消",
  };

  // 列表里出现的状态。
  //
  // 「已完成」只在**本批次**里保留：用户要从窗里点「打开」看刚下完的文件。
  // 但不能把所有历史 done 都列出来——重启后回灌的历史任务会把监视窗塞满，
  // 而且它并不是「这次下载」的一部分。判据见 `settledAt`：任务进入 done
  // 之后超过 RETIRE_MS 就移出列表（留足时间让用户看到并点打开）。
  const VISIBLE = new Set(["downloading", "merging", "paused", "queued", "failed", "done"]);
  const RETIRE_MS = 60_000;

  // 监视窗打开的时刻：早于它创建的 done 任务是「历史回灌」的，
  // 不属于本次下载，直接不进列表（否则重启后窗口会被旧记录塞满）。
  const SESSION_STARTED_AT = Date.now();

  // id -> 首次看到它处于 done 的时间戳
  const doneSeenAt = new Map();

  // 排序权重：正在干活的排最前，其次等用户处理的，再次排队，完成态垫底
  const ORDER = { downloading: 0, merging: 1, paused: 2, failed: 3, queued: 4, done: 5, canceled: 6 };

  function fmtBytes(b) {
    if (!b || b <= 0) return "0 B";
    const u = ["B", "KB", "MB", "GB", "TB"];
    let i = 0,
      v = b;
    while (v >= 1024 && i < u.length - 1) {
      v /= 1024;
      i++;
    }
    return v.toFixed(v >= 100 || i === 0 ? 0 : 1) + " " + u[i];
  }

  /** 从文件名/URL 取扩展名（小写），用于选图标。 */
  function extOf(t) {
    const src = t.output_path || t.title || t.url || "";
    const clean = String(src).split(/[?#]/)[0];
    const m = /\.([a-z0-9]{1,5})$/i.exec(clean);
    return m ? m[1].toLowerCase() : "";
  }

  const EXT_GROUP = {
    video: ["mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "m4v", "ts", "mpg", "mpeg"],
    music: ["mp3", "flac", "wav", "aac", "m4a", "ogg", "opus", "wma"],
    program: ["exe", "msi", "apk", "msix", "dmg", "pkg", "deb", "rpm"],
    archive: ["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso"],
    document: ["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "epub"],
  };

  /** 类型图标：与主窗口任务表同一套符号，保持两处一致。 */
  function iconFor(t) {
    if (t.kind === "video") return ["v", "▶"];
    const ext = extOf(t);
    for (const [g, list] of Object.entries(EXT_GROUP)) {
      if (list.includes(ext)) {
        return { v: ["v", "▶"], music: ["m", "♪"], program: ["p", "⚙"], archive: ["a", "▣"], document: ["d", "▤"] }[g];
      }
    }
    return ["", "⬇"];
  }

  let tasks = [];
  let lastBytes = new Map(); // id -> { bytes, at }，用于逐任务速度估算
  let refreshInFlight = false;
  let refreshAgain = false;

  /** 逐任务速度：字节差分 + 平滑。后端只给字符串速度，且合并/排队时为空。 */
  function speedOf(t) {
    if (t.state !== "downloading") return 0;
    const now = Date.now();
    const prev = lastBytes.get(t.id);
    const bytes = t.downloaded_bytes || 0;
    lastBytes.set(t.id, { bytes, at: now });
    if (!prev || now <= prev.at) return 0;
    const inst = Math.max(0, (bytes - prev.bytes) / ((now - prev.at) / 1000));
    // 平滑：避免分块落盘的脉冲把数字打得来回跳
    const smooth = prev.smooth ? prev.smooth * 0.6 + inst * 0.4 : inst;
    lastBytes.set(t.id, { bytes, at: now, smooth });
    return smooth;
  }

  function visibleTasks() {
    const now = Date.now();
    return tasks
      .filter((t) => {
        if (!VISIBLE.has(t.state)) return false;
        if (t.state === "done") {
          if (t.file_missing) return false;
          // 历史回灌的 done（创建时间早于本窗口）不列：它们不是「这次下载」。
          if ((t.created_at || 0) < SESSION_STARTED_AT - 5_000) return false;
          // 刚完成的留一段时间给用户点「打开」；之后退出列表。
          const seen = doneSeenAt.get(t.id);
          if (seen === undefined) {
            doneSeenAt.set(t.id, now);
            return true;
          }
          return now - seen < RETIRE_MS;
        }
        doneSeenAt.delete(t.id);
        return true;
      })
      .sort(
        (a, b) =>
          (ORDER[a.state] ?? 9) - (ORDER[b.state] ?? 9) ||
          (b.created_at || 0) - (a.created_at || 0)
      );
  }

  /** 一行任务。DOM 结构与样式类见 styles.css 的 .m-row 段。 */
  function renderRow(t) {
    const row = document.createElement("div");
    row.className = "m-row st-" + t.state;
    row.dataset.id = t.id;
    row.setAttribute("role", "listitem");

    const [ic, ch] = iconFor(t);
    const icon = document.createElement("span");
    icon.className = "m-ic" + (ic ? " " + ic : "");
    icon.textContent = ch;
    row.appendChild(icon);

    const body = document.createElement("div");
    body.className = "m-rbody";

    const line1 = document.createElement("div");
    line1.className = "m-r1";
    const name = document.createElement("span");
    name.className = "m-rname";
    name.textContent = t.title || t.url || "下载";
    name.title = t.output_path || t.url || t.title || "";
    line1.appendChild(name);

    const badge = document.createElement("span");
    badge.className = "m-badge";
    badge.textContent = STATE_TEXT[t.state] || t.state;
    line1.appendChild(badge);

    // 行内操作（DG3 同款：下载中给暂停，暂停/失败给继续，完成给打开）
    const acts = document.createElement("span");
    acts.className = "m-acts";
    const mkBtn = (label, title, fn, danger) => {
      const b = document.createElement("button");
      b.className = "m-btn" + (danger ? " danger" : "");
      b.textContent = label;
      b.title = title;
      b.addEventListener("click", (e) => {
        e.stopPropagation();
        fn(t, b);
      });
      acts.appendChild(b);
    };

    if (t.state === "downloading" || t.state === "merging") {
      mkBtn("❚❚", "暂停", pauseOne);
    } else if (t.state === "paused" || t.state === "failed") {
      mkBtn("▶", "继续", resumeOne);
    } else if (t.state === "done" && t.output_path) {
      mkBtn("↗", "打开文件", openOne);
    }
    // 打开所在文件夹对任何有产出路径的任务都有意义
    if (t.output_path && t.state === "done") {
      mkBtn("⌖", "打开所在文件夹", revealOne);
    }
    if (acts.childElementCount) line1.appendChild(acts);

    body.appendChild(line1);

    // 进度条 + 数值行
    const bar = document.createElement("div");
    bar.className = "m-rbar";
    const fill = document.createElement("div");
    fill.className = "m-rfill";
    const pct =
      t.state === "done" ? 100 : Math.max(0, Math.min(100, t.progress || 0));
    fill.style.width = pct.toFixed(1) + "%";
    bar.appendChild(fill);
    body.appendChild(bar);

    const line2 = document.createElement("div");
    line2.className = "m-r2";

    const left = document.createElement("span");
    left.className = "m-rmeta";
    if (t.state === "downloading") {
      const sp = speedOf(t);
      left.textContent =
        (sp > 0 ? fmtBytes(sp) + "/s" : "—") +
        (t.total_bytes ? `  ·  ${fmtBytes(t.downloaded_bytes || 0)} / ${fmtBytes(t.total_bytes)}` : "");
    } else if (t.state === "merging") {
      left.textContent = "音视频合成中…";
    } else if (t.state === "failed" && t.error) {
      left.textContent = String(t.error).split("\n")[0];
      left.classList.add("bad");
      left.title = t.error;
    } else if (t.state === "done") {
      left.textContent = t.total_bytes ? fmtBytes(t.total_bytes) : "已完成";
    } else if (t.state === "paused") {
      left.textContent =
        (t.total_bytes
          ? `${fmtBytes(t.downloaded_bytes || 0)} / ${fmtBytes(t.total_bytes)}`
          : "已暂停") + " · 断点已保留";
    } else {
      left.textContent = t.start_at ? "等待定时开始" : "排队中";
    }
    line2.appendChild(left);

    const right = document.createElement("span");
    right.className = "m-rpct";
    right.textContent = t.state === "done" ? "" : pct > 0 ? pct.toFixed(0) + "%" : "";
    line2.appendChild(right);

    body.appendChild(line2);
    row.appendChild(body);
    return row;
  }

  async function pauseOne(t, btn) {
    btn.disabled = true;
    btn.textContent = "…";
    try {
      await invoke("pause_task", { id: t.id });
    } catch (e) {
      btn.disabled = false;
      btn.textContent = "❚❚";
      notifyErr(e);
      return;
    }
    scheduleRefresh();
  }

  async function resumeOne(t, btn) {
    btn.disabled = true;
    btn.textContent = "…";
    try {
      await invoke("resume_task", { id: t.id });
    } catch (e) {
      btn.disabled = false;
      btn.textContent = "▶";
      notifyErr(e);
      return;
    }
    scheduleRefresh();
  }

  async function openOne(t) {
    try {
      await invoke("open_task_file", { path: t.output_path });
    } catch (e) {
      notifyErr(e);
    }
  }

  async function revealOne(t) {
    try {
      await invoke("plugin:opener|reveal_item_in_dir", { paths: [t.output_path] });
    } catch (e) {
      notifyErr(e);
    }
  }

  /** 行内错误提示：直接挂在顶栏，3 秒后自动消失（监视窗很小，不做 toast 层）。 */
  let errTimer = null;
  function notifyErr(e) {
    const el = $("mFoot");
    $("mFootText").textContent = "操作失败：" + String(e).split("\n")[0];
    el.classList.remove("hidden");
    el.classList.add("bad");
    clearTimeout(errTimer);
    errTimer = setTimeout(() => el.classList.add("hidden"), 4000);
  }

  function render() {
    const list = visibleTasks();
    const active = list.filter((t) => t.state === "downloading" || t.state === "merging");
    const queued = list.filter((t) => t.state === "queued").length;

    document.body.classList.toggle("live", active.length > 0);
    $("mDot").classList.toggle("on", active.length > 0);

    if (!list.length) {
      $("mTitle").textContent = "待机";
      $("mAgg").textContent = "";
      $("mEmpty").classList.remove("hidden");
      $("mList").classList.add("hidden");
      return;
    }

    $("mTitle").textContent =
      active.length > 1 ? `${active.length} 个任务进行中` : active.length === 1 ? "下载中" : "任务列表";
    const aggSpeed = active.reduce((a, t) => a + speedOfCached(t), 0);
    $("mAgg").textContent = aggSpeed > 0 ? fmtBytes(aggSpeed) + "/s" : queued > 0 ? `${queued} 排队` : "";

    $("mEmpty").classList.add("hidden");
    $("mList").classList.remove("hidden");

    // 复用已有行：只有 id 序列变化时才整表重建，否则原地更新文本/宽度。
    // 监视窗每 500ms 刷一次，整表重建会让按钮焦点、悬停态跟着丢。
    const host = $("mList");
    const ids = list.map((t) => t.id).join("\u0001");
    if (host.dataset.ids !== ids) {
      host.dataset.ids = ids;
      host.innerHTML = "";
      for (const t of list) host.appendChild(renderRow(t));
      return;
    }
    // 结构未变：更新进度条与数值（速度行由 render 的下一次 tick 重算）
    for (const t of list) {
      const row = host.querySelector(`.m-row[data-id="${CSS.escape(t.id)}"]`);
      if (!row) continue;
      const pct = t.state === "done" ? 100 : Math.max(0, Math.min(100, t.progress || 0));
      const fill = row.querySelector(".m-rfill");
      if (fill) fill.style.width = pct.toFixed(1) + "%";
      const rp = row.querySelector(".m-rpct");
      if (rp) rp.textContent = t.state === "done" ? "" : pct > 0 ? pct.toFixed(0) + "%" : "";
      const meta = row.querySelector(".m-rmeta");
      if (meta && t.state === "downloading") {
        const sp = speedOfCached(t);
        meta.textContent =
          (sp > 0 ? fmtBytes(sp) + "/s" : "—") +
          (t.total_bytes
            ? `  ·  ${fmtBytes(t.downloaded_bytes || 0)} / ${fmtBytes(t.total_bytes)}`
            : "");
      }
    }
  }

  /** 速度缓存：render 与 agg 都会问速度，同一帧只算一次差分。 */
  let speedTick = 0;
  const speedCache = new Map();
  function speedOfCached(t) {
    if (!speedCache.has(t.id) || speedCache.get(t.id).tick !== speedTick) {
      speedCache.set(t.id, { v: speedOf(t), tick: speedTick });
    }
    return speedCache.get(t.id).v;
  }

  async function refresh() {
    if (refreshInFlight) {
      refreshAgain = true;
      return;
    }
    refreshInFlight = true;
    try {
      const list = await invoke("list_tasks");
      tasks = Array.isArray(list) ? list : [];
      speedTick++;
      render();
    } catch {
      // 窗口关闭过程中会失败，静默
    } finally {
      refreshInFlight = false;
      if (refreshAgain) {
        refreshAgain = false;
        refresh();
      }
    }
  }

  let refreshTimer = null;
  function scheduleRefresh() {
    if (refreshTimer) return;
    refreshTimer = setTimeout(() => {
      refreshTimer = null;
      refresh();
    }, 200);
  }

  // ---------- 关闭 ----------
  $("pwClose").addEventListener("click", () => {
    // close 走 core:window|close，需要 capabilities/progress.json 里的
    // core:window:allow-close。权限缺失时会 reject——静默吞掉的话症状只是
    // 「点了没反应」，所以失败时直接把原因写在窗口里。
    savePosition();
    window.__TAURI__.window
      .getCurrentWindow()
      .close()
      .catch((e) => {
        document.body.textContent = "关闭监视窗失败：" + String(e);
      });
  });

  // ---------- 拖拽 ----------
  //
  // data-tauri-drag-region 只命中「被按住的那个元素自带属性」；这里给整条
  // 顶栏与列表空白处统一兜底，除按钮外的任何左键按下都进入系统拖拽。
  function bindDrag(el) {
    el.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return;
      if (e.target.closest("button")) return;
      window.__TAURI__.window
        .getCurrentWindow()
        .startDragging()
        .catch(() => {});
    });
  }
  bindDrag($("mHead"));
  bindDrag($("monitor"));

  // ---------- 尺寸与位置持久化 ----------
  //
  // 窗口现在 resizable：用户拖右下角手柄决定多大。位置与尺寸一起存
  // localStorage（隐私模式写不进去就退回默认，不值得为它上 IPC）。
  const POS_KEY = "veloxgrab.monitorPos";
  const SIZE_KEY = "veloxgrab.monitorSize";

  function savePosition() {
    try {
      const w = window.__TAURI__.window.getCurrentWindow();
      const p = w.outerPosition();
      if (p && Number.isFinite(p.x) && Number.isFinite(p.y)) {
        localStorage.setItem(POS_KEY, JSON.stringify({ x: p.x, y: p.y }));
      }
      const inner = w.innerSize();
      if (inner && Number.isFinite(inner.width)) {
        localStorage.setItem(SIZE_KEY, JSON.stringify({ w: inner.width, h: inner.height }));
      }
    } catch (e) {
      // 记忆失败不影响任何主流程
    }
  }

  function restorePosition() {
    try {
      const raw = localStorage.getItem(POS_KEY);
      if (raw) {
        const { x, y } = JSON.parse(raw);
        if (Number.isFinite(x) && Number.isFinite(y)) {
          window.__TAURI__.window
            .getCurrentWindow()
            .setPosition(new window.__TAURI__.dpi.PhysicalPosition(x, y))
            .catch(() => {});
        }
      }
    } catch (e) {
      // 坏数据/权限缺失：退回默认位置
    }
  }

  restorePosition();

  let posTimer = null;
  if (window.__TAURI__?.window) {
    const cur = window.__TAURI__.window.getCurrentWindow();
    const queueSave = () => {
      if (posTimer) return;
      posTimer = setTimeout(() => {
        posTimer = null;
        savePosition();
      }, 500);
    };
    cur.onMoved(queueSave).catch(() => {});
    cur.onResized(queueSave).catch(() => {});
  }

  // ---------- 事件 ----------
  if (listen) {
    listen("task-progress", (evt) => {
      const p = evt.payload;
      const i = tasks.findIndex((t) => t.id === p.id);
      if (i < 0) {
        // 新任务：补一次快照（事件载荷不含 kind/title 等字段）
        scheduleRefresh();
        return;
      }
      const merged = { ...tasks[i] };
      for (const [k, v] of Object.entries(p)) {
        if (v !== null && v !== undefined) merged[k] = v;
      }
      tasks[i] = merged;
      speedTick++;
      render();
      // 状态跃迁（开始/暂停/完成）会改变行内按钮，走一次快照重建
      if (tasks[i].state !== merged.state) scheduleRefresh();
    });
    listen("tasks-changed", () => scheduleRefresh());
  }

  refresh();
  // 500ms 心跳：任务表在无进度事件时（比如暂停/排队）也要保持新鲜
  setInterval(refresh, 1000);
  setInterval(() => {
    speedTick++;
    render();
  }, 500);
})();
