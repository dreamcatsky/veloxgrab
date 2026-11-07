// 下载监视框：小方窗，速度 / 总进度每 2.5 秒轮流显示。
// 与主窗口只共享 Rust 侧任务表与 task-progress 事件；聚合所有活跃任务。
(function () {
  "use strict";

  const invoke = window.__TAURI__?.core?.invoke;
  const listen = window.__TAURI__?.event?.listen;

  if (!invoke) {
    document.body.textContent = "无法连接 Tauri 运行时。";
    return;
  }

  // 主题跟随主窗口
  invoke("get_settings")
    .then((s) => window.VeloxGrabTheme?.apply(s.theme || "system"))
    .catch(() => {});

  const $ = (id) => document.getElementById(id);
  const ACTIVE = new Set(["downloading", "merging"]);

  function fmtBytes(b) {
    if (!b || b <= 0) return "0 B";
    const u = ["B", "KB", "MB", "GB", "TB"];
    let i = 0, v = b;
    while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
    return v.toFixed(v >= 100 || i === 0 ? 0 : 1) + " " + u[i];
  }

  let tasks = [];
  let mode = 0;              // 0 = 速度, 1 = 进度
  let aggSpeed = 0;          // 字节/秒（自采样，带平滑）
  let lastBytes = -1;
  let lastT = 0;

  // 聚合所有活跃任务
  function agg() {
    const act = tasks.filter((t) => ACTIVE.has(t.state));
    let done = 0, total = 0, known = true, pctSum = 0;
    for (const t of act) {
      done += t.downloaded_bytes || 0;
      if (t.total_bytes) total += t.total_bytes;
      else known = false;
      pctSum += t.state === "merging" ? 100 : t.progress || 0;
    }
    const pct = known && total > 0 ? (done / total) * 100 : pctSum / (act.length || 1);
    return { act, done, total, known, pct: Math.max(0, Math.min(100, pct)) };
  }

  // 自采样速度：对 downloading 任务的已下字节差分。不解析速度字符串——
  // 各任务速度单位不一，求和容易错；字节差分对单任务/多任务都对。
  function sampleSpeed() {
    const bytes = tasks
      .filter((t) => t.state === "downloading")
      .reduce((a, t) => a + (t.downloaded_bytes || 0), 0);
    const now = Date.now();
    if (lastBytes >= 0 && now > lastT) {
      const inst = Math.max(0, (bytes - lastBytes) / ((now - lastT) / 1000));
      aggSpeed = aggSpeed > 0 ? aggSpeed * 0.55 + inst * 0.45 : inst;
    }
    lastBytes = bytes;
    lastT = now;
  }

  function render() {
    const { act, done, total, known, pct } = agg();
    const queued = tasks.filter((t) => t.state === "queued").length;

    if (!act.length) {
      $("mName").textContent = queued > 0 ? `${queued} 个任务排队中` : "待机";
      $("mValue").textContent = "—";
      $("mSub").textContent = "";
      $("mFill").style.width = "0%";
      return;
    }

    const merging = act.some((t) => t.state === "merging");
    const name =
      act.length === 1
        ? act[0].title || act[0].url || "下载中"
        : `${act.length} 个下载中`;
    $("mName").textContent = name + (queued > 0 ? ` · ${queued} 排队` : "");

    if (merging && mode === 0) {
      // 合并阶段没有速度可言，显示固定文案避免 0 B/s 的困惑
      $("mValue").textContent = "合并中…";
      $("mSub").textContent = "音视频正在合成";
    } else if (mode === 0) {
      $("mValue").textContent = fmtBytes(aggSpeed) + "/s";
      $("mSub").textContent = "下载速度";
    } else {
      $("mValue").textContent = pct.toFixed(1) + "%";
      $("mSub").textContent = known ? `${fmtBytes(done)} / ${fmtBytes(total)}` : "";
    }
    $("mFill").style.width = pct.toFixed(1) + "%";
  }

  async function refresh() {
    try {
      const list = await invoke("list_tasks");
      tasks = list || [];
      sampleSpeed();
      render();
    } catch {
      // 窗口关闭过程中会失败，静默
    }
  }

  $("pwClose").addEventListener("click", () => {
    // close 走的是 core:window|close IPC，需要 capabilities 里的
    // core:window:allow-close（见 progress-window capability）。权限缺失时
    // 这里的 promise 会 reject——静默吞掉的话，症状只是「点了没反应」，
    // 和权限被拒时一模一样，无法排查。
    window.__TAURI__.window
      .getCurrentWindow()
      .close()
      .catch((e) => {
        document.body.textContent = "关闭监视窗失败：" + String(e);
      });
  });

  if (listen) {
    listen("task-progress", (evt) => {
      const p = evt.payload;
      const i = tasks.findIndex((t) => t.id === p.id);
      if (i < 0) { refresh(); return; }
      const merged = { ...tasks[i] };
      for (const [k, v] of Object.entries(p)) {
        if (v !== null && v !== undefined) merged[k] = v;
      }
      tasks[i] = merged;
      sampleSpeed();
      render();
    });
  }

  refresh();
  // 采样/重绘 500ms；视角轮流 2.5s
  setInterval(() => { sampleSpeed(); render(); }, 500);
  setInterval(() => { mode = (mode + 1) % 2; render(); }, 2500);
})();
