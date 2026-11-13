// VeloxGrab 前端逻辑
// 注意：这里不 import任何打包器，直接用全局 window.__TAURI__（Tauri 2 的 withGlobalTauri 行为）
(function () {
  "use strict";

  const invoke = window.__TAURI__?.core?.invoke;
  const listen = window.__TAURI__?.event?.listen;

  // 纯函数工具集来自 core.js（index.html 在本脚本之前加载）：
  // 同一份实现被 node:test 直接测（tests/core.test.mjs），这里只是解构别名。
  const { fmtBytes, fmtDuration, hostOf, extOfName, nameLooksLikeUrl, escHtml, parseSpeed } =
    window.VeloxGrabCore || {};

  if (!invoke) {
    // 用 var(--err) 而不是写死的 #e5544b：那串红是按深色底调的，
    // 浅色主题下对比度不够，看不清。
    document.body.innerHTML =
      '<div style="padding:40px;font-family:sans-serif;color:var(--err)">' +
      "无法连接 Tauri 运行时。请通过 VeloxGrab 桌面程序启动，而不是用浏览器打开本页面。</div>";
    return;
  }

  // ---------- 小工具 ----------
  const $ = (id) => document.getElementById(id);
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
  };

  // ---------- 主题 ----------
  //
  // 首帧的落定在 theme.js（<head> 里的同步脚本）做完了，这里只负责
  // 「拿到 Rust 侧真值之后再对齐一次」，以及用户改选时的即时切换。
  //
  // 分工的理由：get_settings 是一次 IPC，等它回来再上色，窗口会先闪一帧
  // 错误主题。所以先用 localStorage 的缓存值上色（可能旧一帧但绝不闪），
  // 等真值到了再纠正。二者一致时这个纠正完全看不出来。
  function applyTheme(theme) {
    if (!window.VeloxGrabTheme) return;
    window.VeloxGrabTheme.apply(theme);
  }

  let toastTimer = null;
  function toast(msg, isErr) {
    const t = $("toast");
    t.textContent = msg;
    t.className = "toast" + (isErr ? " err" : "");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => t.classList.add("hidden"), 3200);
  }

  /**
   * 带一个动作按钮的 toast（目前只有「剪贴板里有链接」用）。
   *
   * 为什么不给 toast() 加参数：普通 toast 是「一条消息，3.2 秒消失」，全程序
   * 有几十处调用；带按钮的版本要活得更久、要能点、还要清掉上一次的 DOM。
   * 混在一起会让每条消息都背上这段逻辑。
   */
  function toastAction(msg, label, onAction, ms) {
    const t = $("toast");
    t.className = "toast action";
    t.innerHTML = "";
    const span = document.createElement("span");
    span.textContent = msg;
    const btn = document.createElement("button");
    btn.className = "toast-act";
    btn.textContent = label;
    btn.onclick = () => {
      t.classList.add("hidden");
      onAction();
    };
    t.appendChild(span);
    t.appendChild(btn);
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => t.classList.add("hidden"), ms || 8000);
  }

  /**
   * 显示关机/重启倒计时，带一个「取消」。
   *
   * 不复用 toastAction：toast 是单例，下一条普通提示会把它顶掉——倒计时
   * 提示被顶掉，用户就没有取消关机的入口了。这里用独立元素，只有取消
   * 或倒计时结束才会消失。
   */
  let countdownTimer = null;
  function showCountdown(text, onCancel) {
    const el = $("planCountdown");
    $("planCountdownText").textContent = text;
    el.classList.remove("hidden");
    $("planCountdownCancel").onclick = () => {
      el.classList.add("hidden");
      clearTimeout(countdownTimer);
      onCancel();
    };
    clearTimeout(countdownTimer);
    // 比关机延迟多留 2 秒：让系统先执行，避免提示先消失造成「取消了」的错觉。
    countdownTimer = setTimeout(() => el.classList.add("hidden"), 62000);
  }

  /**
   * 把字符串转义成可安全拼进 innerHTML 的文本。
   *
   * 后端返回的目录名来自安装/解压位置，用户完全可能把它放在 `D:\a&b\<c>`
   * 这种路径下；直接拼进模板会让路径里的标签被当成 HTML 解析。
   */
  /**
   * 统一的确认入口：Tauri 2 把 window.confirm 重写成返回 Promise<boolean>
   * 且**不阻塞**——把返回值当布尔用的话 Promise 恒为真值，`if (!ok) return`
   * 永远拦不住，「确认框还开着、文件已经删了」就是这个（1.4.1 实测事故）。
   * 这里统一 await：Promise 形态等用户作答，同步布尔形态（非 Tauri 环境）
   * 原样透传，两种形态都安全。
   */
  async function askConfirm(message) {
    return Boolean(await confirm(message));
  }

  function showError(msg) {
    const b = $("parseError");
    if (!msg) {
      b.classList.add("hidden");
      return;
    }
    b.textContent = msg;
    b.classList.remove("hidden");
  }

  // ---------- 状态 ----------
  let currentMedia = null;
  let currentDirect = null;
  let currentPlaylist = null;
  let taskMap = new Map();
  let appSettings = null;

  // ---------- 界面语言 ----------
  // 中文就是 HTML 里的原文：首次应用前把每个标记节点的 innerHTML 原样留存，
  // 切英文时覆盖、切回中文时还原。字典里只放英文，省一半体积。
  // 脚本动态生成的文本（toast、任务卡片）不在此列——它们跟着操作现场走。
  const I18N_EN = {
    // 工具栏
    tbAddUrl: "Add URL",
    tbResume: "Resume",
    tbPause: "Pause",
    tbPauseAll: "Pause all",
    tbDelete: "Delete",
    tbOpen: "Open",
    tbFolder: "Folder",
    tbOptions: "Options",
    // 侧边栏
    sbCategory: "Categories",
    sbAll: "All downloads",
    sbActive: "Downloading",
    sbDone: "Finished",
    sbType: "Types",
    catVideo: "Video",
    catMusic: "Music",
    catProgram: "Programs",
    catArchive: "Archives",
    catDocument: "Documents",
    sbSettings: "Settings",
    // 列表表头
    colName: "Name",
    colSize: "Size",
    colState: "Status",
    colProgress: "Progress",
    colSpeed: "Speed",
    colEta: "Time left",
    listEmpty: "No downloads yet. Click “Add URL” or use the browser sniffer.",
    totalSpeed: "Total speed",
    addUrlTitle: "Add URL",
    parse: "Parse",
    ndSummary: "New download",
    ndSummaryHint: "(when you already have a direct URL)",
    ndUrlLabel: "URL",
    filenameLabel: "File name",
    filenameOptionalHint: "(optional, auto-detected)",
    threadsLabel: "Threads",
    saveToLabel: "Save to",
    pickDirBtn: "Browse…",
    pickFileBtn: "Browse…",
    startDownload: "Start download",
    ndHint: "Download without parsing — for known direct links",
    qualityLabel: "Quality",
    directHint: "Multi-thread chunked download, resumable",
    urlPlaceholder: "Paste a link: video page (Douyin / Bilibili / YouTube…) or any direct link (.zip / .mp4 / .iso…)",
    ndDirPlaceholder: "empty = default folder from Settings",
    emptyTip1: "Two kinds of links are supported: ① video pages — quality and audio track resolved automatically; ② any HTTP/HTTPS direct link — multi-thread chunked download.",
    emptyTip2: "Some platforms (e.g. Douyin) need login cookies. If parsing fails, configure browser cookies in Settings.",
    noTasks: "No tasks",
    clearFinished: "Clear finished",
    secDownload: "Download",
    secBehavior: "Popups & behavior",
    secVideo: "Video & cookies",
    secProxy: "Proxy",
    secMisc: "Integration & UI",
    downloadDir: "Default download folder",
    maxConcurrent: "Concurrent tasks",
    defaultThreads: "Default threads",
    defaultThreadsHint: "(1-64, direct-link chunks)",
    fragments: "Fragments per video",
    fragmentsHint: "(1-64, yt-dlp fragments)",
    hfCap: "HuggingFace thread cap",
    hfCapHint: "(without token; 0 = unlimited)",
    hfToken: "HuggingFace Access Token",
    hfTokenHint: "(optional, hf_xxx)",
    rateLimit: "Global speed limit",
    rateLimitHint: "(KB/s, 0 = off)",
    mirrorPrefix: "Download mirror prefix",
    mirrorPrefixHint: "(GitHub / HuggingFace only; empty = direct)",
    githubToken: "GitHub Token",
    githubTokenHint: "(optional, raises Release API rate limit)",
    mirrorNote: "A mirror is a third-party relay that can see your requests and file contents; credentialed requests and task SHA-256 checks are unaffected.",
    releaseTag: "Release",
    categoriesEnabled: "Sort downloads into subfolders by extension",
    categoriesHint: "(only when no save folder is chosen for the task)",
    catFolder: "Subfolder",
    catExts: "Extensions (comma separated)",
    catAdd: "Add rule",
    categoriesNote: "Subfolders are single-level names; rules for the same folder are merged, max 24.",
    nameConflict: "Same-name files",
    nameConflictHint: "(overwrite only applies to files this app downloaded from the same link)",
    nameKeepBoth: "Keep both (auto rename)",
    nameOverwrite: "Overwrite existing file",
    secHeaders: "Custom request headers",
    headersNote: "For hotlink-protected sites add Referer / User-Agent; one Name: value per line. Empty host means all sites, where Authorization is not allowed. Host, Range, Cookie and other managed headers are ignored.",
    headerAdd: "Add preset",
    popupSaveAs: 'Manual veloxgrab:// links ask "Save as" first',
    popupSaveAsHint: "(off: save straight to the default folder; tasks sent by the browser extension always enqueue directly)",
    popupQuality: "Video sites: popup quality chooser",
    popupQualityHint: "(off: auto best quality)",
    autoProgress: "Switch to task view when a download starts",
    mergeMp4: "Always merge output to MP4",
    autostart: "Start with Windows",
    autostartHint: "(writes HKCU Run key on save)",
    clipboardWatch: "Watch clipboard for download links",
    clipboardWatchHint: "(shows a hint in the window; never downloads automatically)",
    notifyComplete: "Notify when a download finishes",
    notifyCompleteHint: "(flashes the taskbar and updates the tray tooltip)",
    closeMode: "When closing the main window",
    closeBackground: "Minimize to tray and keep downloading",
    closeAsk: "Ask every time",
    closeQuit: "Quit the app",
    closeAskTitle: "Close VeloxGrab?",
    closeAskBody: "Minimizing to the tray keeps downloads running. Quitting pauses all active tasks (they can resume next launch).",
    closeToTray: "Minimize to tray",
    closeQuitNow: "Quit",
    cookieBrowser: "Cookies source",
    cookieNone: "None",
    cookieBrowserHint: "Read login state from the browser for content that needs it",
    cookieFile: "Cookies file",
    cookieFileHint: "(optional, Netscape format, takes precedence)",
    cookieFilePlaceholder: "empty = use the browser option above",
    proxyEnable: "Enable proxy (direct downloads and video parsing both use it)",
    proxyType: "Type",
    proxyHost: "Host",
    proxyPort: "Port",
    proxyUser: "User",
    proxyPass: "Password",
    optionalHint: "(optional)",
    proxyHint: "SOCKS5 uses socks5h (remote DNS); works with v2rayN port 10808",
    browserInt: "Browser integration",
    aria2Enable: "aria2 JSON-RPC compatible service",
    aria2Hint: "(127.0.0.1:6800, token required; turning it off needs a restart)",
    aria2TokenLabel: "RPC Token",
    aria2Usage: "Point third-party tools at http://127.0.0.1:6800/jsonrpc with the token above. Supports addUri / tellStatus / getVersion and more.",
    openExtDir: "Open extension folder",
    browserIntHint: "Load the veloxgrab-extension folder there into Chrome / Edge extensions",
    browserIntHint2: 'Then right-click a page → "Download with VeloxGrab" sends the link to this app (veloxgrab:// protocol).',
    language: "Language",
    theme: "Appearance",
    themeHint: "(dark / light / follow system — applies immediately)",
    themeSystem: "Follow system",
    themeDark: "Dark",
    themeLight: "Light",
    save: "Save settings",
    restoreDefaults: "Restore defaults",
    cancel: "Cancel",
    close: "Close",
    saveAsTitle: "Save as",

    // 菜单栏
    menuTask: "Task(T)",
    menuFile: "File(F)",
    menuDownload: "Download(D)",
    menuView: "View(V)",
    menuHelp: "Help(H)",
    miAddDownload: "New download",
    miStartAll: "Start all",
    miPauseAll: "Pause all",
    miOpenDir: "Open download folder",
    miFromClipboard: "Add URL from clipboard",
    miResumeSel: "Resume selected",
    miRedoSel: "Redownload selected",
    miPauseSel: "Pause selected",
    miDeleteSel: "Delete selected",
    miClearAll: "Clear all tasks",
    miPlanAfterAll: "When all finish…",
    planTitle: "When all downloads finish",
    planHint: "Applies to this run only. It will not fire while any task has failed; retrying the failed task and succeeding still triggers it.",
    planNone: "Do nothing",
    planShutdown: "Shut down",
    planRestart: "Restart",
    planSleep: "Sleep",
    planOpenFile: "Open the finished file",
    planOpenFileHint: "(executables are only revealed in Explorer)",
    planDelayHint: "(60-second countdown, can be cancelled)",
    planSave: "Save",
    miOptions: "Options",
    miRefresh: "Refresh",
    miUsage: "How to use",
    miSetupExt: "Enable browser extension…",
    miCheckUpdate: "Check for updates…",
    miAbout: "About VeloxGrab",
    cxOpen: "Open file",
    cxReveal: "Show in folder",
    cxCopyPath: "Copy file path",
    cxCopyUrl: "Copy download link",
    cxPause: "Pause",
    cxResume: "Resume",
    cxRedo: "Redownload",
    cxEdit: "Edit…",
    cxBump: "Move to top of queue",
    editRate: "Speed limit for this task (KB/s)",
    editSha: "Expected SHA-256 (optional)",
    searchPh: "Search name / URL / path…",
    monitorNote: "The task monitor now lives in the browser extension popup: click the VeloxGrab toolbar icon to see each task's progress and pause/resume it.",
    ndStartAt: "Start at",
    ndStartAtHint: "(optional; empty = start now)",
    ndSha: "Expected SHA-256",
    ndShaHint: "(optional, 64 hex chars)",
    plSelectAll: "All",
    plDownload: "Download selected",
    plHint: "Each entry downloads at yt-dlp's best quality",
    cxRemove: "Remove task",
    cxRemoveFile: "Delete task and file",
    editTitle: "Edit task",
    editHint: "Only queued, paused or failed tasks can be edited; changing the file name, folder or thread count does not move existing partial data.",
    editSaveBtn: "Save",
    editUrl: "Download URL",
    editUrlHint: "(changing it restarts the download from scratch)",
    upInstall: "Update now",
    upLater: "Later",
    upAvailable: "New version available",
    upInstalling: "Installing…",
    selectAll: "All",
    paused: "Paused",
  };
  let i18nOriginal = null;
  let i18nPhOriginal = null;

  function applyLanguage(lang) {
    // 本窗口的实现只支持 data-i18n（内容）与 data-i18n-ph（placeholder）；
    // 曾经的 data-i18n-attr 分支语义写错且无任何元素使用，已删除。
    const nodes = document.querySelectorAll("[data-i18n]");
    const phNodes = document.querySelectorAll("[data-i18n-ph]");
    if (!i18nOriginal) {
      i18nOriginal = new Map();
      nodes.forEach((n) => i18nOriginal.set(n, n.innerHTML));
      i18nPhOriginal = new Map();
      phNodes.forEach((n) => i18nPhOriginal.set(n, n.placeholder));
    }
    nodes.forEach((n) => {
      const key = n.dataset.i18n;
      if (lang === "en" && I18N_EN[key]) n.innerHTML = I18N_EN[key];
      else n.innerHTML = i18nOriginal.get(n);
    });
    // placeholder 属性不走 innerHTML：单独留存/还原
    phNodes.forEach((n) => {
      const key = n.dataset.i18nPh;
      if (lang === "en" && I18N_EN[key]) n.placeholder = I18N_EN[key];
      else n.placeholder = i18nPhOriginal.get(n);
    });
    document.documentElement.lang = lang === "en" ? "en" : "zh-CN";
  }

  // ---------- 视图路由 ----------
  //
  // 侧边栏项分两类：类别/分类项都落在同一张任务表里（只是筛选条件不同），
  // 「下载记录 / 设置」是真正的另一个视图。用一次 showView 统一管，
  // 免得菜单、工具栏、快捷键各写一套切换逻辑后状态对不上。
  const TASK_FILTERS = new Set([
    "all", "active", "done", "video", "music", "program", "archive", "document",
  ]);
  let currentView = "all";

  function showView(name) {
    if (!name) return;
    const isFilter = TASK_FILTERS.has(name);
    currentView = name;

    document.querySelectorAll(".sidebar .sb-item").forEach((b) => {
      const key = b.dataset.view || b.dataset.cat;
      b.classList.toggle("active", key === name);
    });

    const showTasks = isFilter;
    $("taskView").classList.toggle("active", showTasks);
    $("settingsView").classList.toggle("active", name === "settings");

    if (name === "settings") loadSettings();
    else refreshTasks();
  }

  // 兼容旧调用点：原来的「切到某个页签」等价于切到对应视图
  function switchTab(name) {
    if (name === "home") {
      openAddUrl();
      return;
    }
    showView(name === "tasks" ? currentView : name);
  }

  // ---------- 添加 URL 弹窗 ----------
  function openAddUrl() {
    $("addOverlay").classList.remove("hidden");
    // 弹窗可能被复用：上次留下的解析结果还在，先按当前状态把入口收拢
    syncStartButtons();
    setTimeout(() => $("urlInput").focus(), 30);
  }

  // ---------- 「开始下载」入口互斥 ----------
  //
  // 弹窗里本来有三个开始按钮：手动新建(#ndStart)、媒体卡片(#downloadBtn)、
  // 直链卡片(#dDownloadBtn)。前两个语义不同（「别解析，直接下」vs「按这个
  // 解析结果下」），合并成一个会丢信息；但同屏出现两个一模一样的按钮，
  // 用户不知道该点哪个。
  //
  // 做法：同一时刻只让一个能用——
  //   · 解析出结果（媒体/直链卡片可见）→ 收起并禁用手动新建区
  //   · 没有解析结果 → 手动新建区可用
  // 收起用 <details> 的 open 属性；禁用用 disabled（遮住「按钮还在但点了
  // 没反应」的困惑——禁用态至少视觉上能看出不可点）。
  function syncStartButtons() {
    const resolved =
      !$("mediaCard").classList.contains("hidden") ||
      !$("directCard").classList.contains("hidden") ||
      !$("playlistCard").classList.contains("hidden");

    const nd = $("newDownload");
    const ndStart = $("ndStart");
    const ndHint = $("ndDisabledHint");

    if (resolved) {
      // 解析出结果了：收起手动新建，避免两个「开始下载」并排
      if (nd) nd.open = false;
      if (ndStart) ndStart.disabled = true;
      if (ndHint) ndHint.classList.remove("hidden");
    } else {
      if (ndStart) ndStart.disabled = false;
      if (ndHint) ndHint.classList.add("hidden");
      // 没有解析结果时，手动新建就是主入口，展开它省一次点击。
      // 直接在解析失败后自动展开会让用户以为「解析没生效」，
      // 所以只在弹窗刚打开（无解析状态）时做——由 openAddUrl 调用。
    }
  }

  // 用户一旦在手动新建区里动手（展开它、填地址），说明他想走直链路径，
  // 就把解析结果清掉——否则又回到「两个按钮都能点」的状态。
  function preferManualEntry() {
    $("mediaCard").classList.add("hidden");
    $("directCard").classList.add("hidden");
    $("playlistCard").classList.add("hidden");
    currentMedia = null;
    currentDirect = null;
    currentPlaylist = null;
    syncStartButtons();
  }

  function closeAddUrl() {
    $("addOverlay").classList.add("hidden");
  }
  function onDownloadAccepted(name) {
    toast(appSettings && appSettings.language === "en"
      ? "Added to download queue" : "已加入下载队列");
    closeAddUrl();
    // 自动切到「所有下载」而不是当前视图：用户刚加完任务，
    // 若当前停在「已完成」或某个分类下，新任务根本不会出现在列表里。
    // showView 内部会刷新列表；只有不切视图时才需要补一次刷新。
    // 旧写法两个都发，每次入队两趟 list_tasks。
    if (appSettings && appSettings.auto_progress_popup) showView("all");
    else refreshTasks();
  }

  const THREAD_OPTIONS = [1, 2, 4, 8, 16, 32, 64];
  function fillThreadSelect(sel, current) {
    sel.innerHTML = "";
    const en = appSettings && appSettings.language === "en";
    // 当前值不在枚举里（比如旧配置存了 6）：就近吸附，避免 select 显示空白
    const near = THREAD_OPTIONS.reduce((a, b) =>
      Math.abs(b - current) < Math.abs(a - current) ? b : a, THREAD_OPTIONS[0]);
    THREAD_OPTIONS.forEach((n) => {
      const o = el("option");
      o.value = String(n);
      o.textContent = en ? n + " threads" : n + " 线程";
      if (n === near) o.selected = true;
      sel.appendChild(o);
    });
  }

  // ---------- 环境检查 ----------
  async function checkHealth() {
    try {
      const h = await invoke("health_check");
      const dot = $("healthDot");
      const banner = $("healthBanner");

      if (h.ytdlp_ok && h.ffmpeg_ok) {
        dot.className = "health ok";
        dot.title = `yt-dlp ${h.ytdlp_version || ""} · ffmpeg 就绪`;
        banner.classList.add("hidden");
      } else {
        dot.className = "health bad";
        dot.title = h.message || "依赖缺失";
        banner.textContent = h.message || "依赖缺失";
        banner.className = "banner warn";
        if (!h.ytdlp_ok) banner.classList.add("error");
      }
      return h;
    } catch (e) {
      $("healthDot").className = "health bad";
      return null;
    }
  }

  // ---------- 解析（自动分流：媒体站 → yt-dlp，其他 → 直链） ----------
  async function doParse() {
    // in-flight 守卫：解析进行中（按钮已禁用）时直接忽略重复触发
    if ($("parseBtn").disabled) return;
    const url = $("urlInput").value.trim();
    if (!url) {
      toast("请先粘贴链接", true);
      return;
    }

    showError("");
    $("mediaCard").classList.add("hidden");
    $("directCard").classList.add("hidden");
    $("playlistCard").classList.add("hidden");
    currentMedia = null;
    currentDirect = null;
    currentPlaylist = null;
    syncStartButtons();

    const btn = $("parseBtn");
    btn.disabled = true;
    // 按钮里包着 data-i18n 的 span：只改 span，别把标记节点整个换掉
    const btnLabel = btn.querySelector("span");
    if (btnLabel) btnLabel.textContent = "识别中…";

    try {
      // 播放列表优先：合集/频道地址走 yt-dlp 的扁平解析，一次拿到全部条目。
      // 不是列表时返回错误（或条目过少），静默回落到单条解析——对单条视频
      // 地址来说这是一次多余但无害的推断。
      let pl = null;
      if (looksLikePlaylistSource(url)) {
        try {
          pl = await invoke("probe_playlist", { url });
        } catch (e) {
          pl = null;
        }
      }
      if (pl && pl.entries && pl.entries.length > 1) {
        currentPlaylist = pl;
        renderPlaylist(pl);
        return;
      }

      const r = await invoke("probe_auto", { url });

      if (r.kind === "video") {
        currentMedia = r.media;
        renderMedia(r.media);
      } else if (r.kind === "direct") {
        currentDirect = r.direct;
        currentDirect.sourceUrl = url;
        // GitHub 仓库地址被翻译成了 release 资产：把来源标签带上，
        // 让用户看清「下的不是这个页面，而是最新版里的某个文件」。
        currentDirect.release = r.release || null;
        renderDirect(r.direct);
      } else {
        showError(r.message || "无法识别该链接");
      }
    } catch (e) {
      showError(String(e));
    } finally {
      btn.disabled = false;
      // 只改 span：直接 btn.textContent 会抹掉 data-i18n 标记节点
      if (btnLabel) btnLabel.textContent =
        ((appSettings && appSettings.language) === "en" && I18N_EN.parse) ? I18N_EN.parse : "解析";
    }
  }

  // ---------- 播放列表 ----------
  //
  // 「先猜是不是列表，不是就回落到单条解析」：判断只看 URL 形状（list=、
  // /playlist、/sets/、@频道、/channel/ 等常见形态），猜错只是多花一次
  // 扁平解析（单条视频上它会很快失败或用一条 entry 表示），不会误把单条
  // 视频当成列表——真正的判据是解析结果里条目数 > 1。
  function looksLikePlaylistSource(url) {
    try {
      const u = new URL(url);
      const p = u.pathname.toLowerCase();
      const q = u.searchParams;
      if (q.get("list")) return true;
      return (
        /\/playlist(\/|$)/.test(p) ||
        /\/sets?\//.test(p) ||
        /\/channel\//.test(p) ||
        /\/user\//.test(p) ||
        /^\/@[^/]+\/?$/.test(p)
      );
    } catch (e) {
      return false;
    }
  }

  function renderPlaylist(pl) {
    $("plTitle").textContent = pl.title || "播放列表";
    $("plUploader").textContent = pl.uploader || "";
    $("plCount").textContent = `${pl.count} 个条目`;
    $("plSelectAll").checked = true;
    $("plDir").value = $("ndDir").value.trim() || "";

    const host = $("plList");
    host.innerHTML = "";
    pl.entries.forEach((e) => {
      const row = el("label", "pl-row");
      const cb = el("input");
      cb.type = "checkbox";
      cb.checked = true;
      cb.dataset.idx = String(e.index);
      cb.onchange = updatePlSelected;
      row.appendChild(cb);
      row.appendChild(el("span", "pl-idx", String(e.index)));
      const name = el("span", "pl-name", e.title || e.url);
      name.title = e.url;
      row.appendChild(name);
      row.appendChild(el("span", "pl-dur", fmtDuration(e.duration)));
      host.appendChild(row);
    });
    updatePlSelected();
    $("playlistCard").classList.remove("hidden");
    syncStartButtons();
  }

  function updatePlSelected() {
    const boxes = [...$("plList").querySelectorAll('input[type="checkbox"]')];
    const n = boxes.filter((b) => b.checked).length;
    $("plSelected").textContent = `已选 ${n} / ${boxes.length}`;
    $("plSelectAll").checked = n > 0 && n === boxes.length;
    $("plSelectAll").indeterminate = n > 0 && n < boxes.length;
    $("plDownloadBtn").disabled = n === 0;
  }

  async function doPlaylistDownload() {
    if (!currentPlaylist) return;
    const picked = new Set(
      [...$("plList").querySelectorAll('input[type="checkbox"]')]
        .filter((b) => b.checked)
        .map((b) => Number(b.dataset.idx))
    );
    const entries = currentPlaylist.entries
      .filter((e) => picked.has(e.index))
      .map((e) => ({
        url: e.url,
        title: e.title,
        thumbnail: e.thumbnail || null,
        duration: e.duration || null,
      }));
    if (!entries.length) return toast("请先勾选要下载的条目", true);

    const btn = $("plDownloadBtn");
    btn.disabled = true;
    try {
      const r = await invoke("enqueue_playlist", {
        args: { entries, save_dir: $("plDir").value.trim() || null },
      });
      const created = (r && r.created) || 0;
      const errors = (r && r.errors) || [];
      toast(
        errors.length
          ? `已加入 ${created} 个任务，${errors.length} 条被跳过：${errors[0]}`
          : `已加入 ${created} 个任务`,
        errors.length > 0
      );
      onDownloadAccepted();
    } catch (e) {
      toast(String(e), true);
    } finally {
      btn.disabled = false;
    }
  }

  // ---------- 直链卡片 ----------
  // 描述这个直链能用几线程。
  //
  // 只写「支持多线程分块」会误导：example.com 那种 577 B 的小文件确实支持
  // Range，但分块下限是 1 MiB，实际只能 1 线程跑。用户看到「支持多线程」
  // 却发现线程数被锁在 1，会以为程序坏了。这里按真实可用的线程数说话。
  function describeRange(d) {
    if (!d.range_supported) return "不支持 Range，单线程下载";
    const n = d.suggested_threads || 1;
    if (n <= 1) return "支持 Range，但文件太小，单线程下载";
    return `支持多线程分块，建议 ${n} 线程`;
  }

  function renderDirect(d) {
    $("dTitle").textContent = d.filename || "（未提供文件名）";
    $("dMime").textContent = d.content_type || "未知类型";

    const parts = [];
    if (d.release) {
      // GitHub Release 资产：来源标签代替「未知类型」，并给出体积
      const rel = d.release;
      parts.push(`GitHub Release ${rel.repo}${rel.tag ? " " + rel.tag : ""}`);
      $("dMime").textContent = "release";
    }
    if (d.total_bytes) parts.push(`大小: ${fmtBytes(d.total_bytes)}`);
    parts.push(describeRange(d));
    $("dDetail").textContent = parts.join("   ·   ");

    // 线程数下拉：与手动新建同源（1..64）；不支持 Range 时只有 1 线程
    const sel = $("dThreads");
    if (!d.range_supported) {
      sel.innerHTML = "";
      const o = el("option");
      o.value = "1";
      o.textContent = "1 线程";
      o.selected = true;
      sel.appendChild(o);
    } else {
      fillThreadSelect(sel, d.suggested_threads || (appSettings && appSettings.default_threads) || 8);
    }

    $("dFilenameInput").value = d.filename || "";
    $("directCard").classList.remove("hidden");
    syncStartButtons();
  }

  async function doDirectDownload() {
    if (!currentDirect) return;
    const btn = $("dDownloadBtn");
    btn.disabled = true;
    try {
      const threads = Number($("dThreads").value || 1);
      const nameOverride = $("dFilenameInput").value.trim();
      // GitHub Release 资产：任务地址要用**资产下载地址**，不是仓库页面；
      // 否则重下/续传会再去解析一遍 release，而且断点 URL 对不上。
      const taskUrl =
        (currentDirect.release && currentDirect.release.download_url) ||
        currentDirect.final_url ||
        currentDirect.sourceUrl;
      await invoke("enqueue_download", {
        args: {
          url: taskUrl,
          title: nameOverride || currentDirect.filename || "文件",
          thumbnail: null,
          duration: null,
          format_id: "direct",
          quality_label: currentDirect.range_supported
            ? `${threads} 线程`
            : "单线程",
          has_audio: false,
          best_audio_format: null,
          kind: "direct",
          threads,
          // 解析出的直链也允许指定目录：与「手动新建」共用同一选择框
          save_dir: $("ndDir").value.trim() || null,
          // 分类目录只认扩展名；优先用用户改过的文件名，其次服务器给的名字/URL
          expected_ext: extOfName(
            nameOverride || currentDirect.filename || currentDirect.sourceUrl
          ),
          expected_bytes: currentDirect.total_bytes || null,
        },
      });
      $("enqueueHint").textContent = "可在「任务」页查看进度";
      onDownloadAccepted();
    } catch (e) {
      toast(String(e), true);
    } finally {
      btn.disabled = false;
    }
  }

  // ---------- 手动新建下载（跳过解析） ----------
  async function doNewDownload() {
    const url = $("ndUrl").value.trim();
    if (!url) {
      toast("请先填写下载地址", true);
      $("ndUrl").focus();
      return;
    }

    // 定时开始：datetime-local → Unix 毫秒；空/过去 = 立即（后端同样钳）
    let startAt = null;
    const sa = $("ndStartAt").value;
    if (sa) {
      const ms = new Date(sa).getTime();
      if (Number.isFinite(ms) && ms > Date.now()) startAt = ms;
    }
    // SHA-256 预期：给了就先做形状校验，省一次 IPC 往返
    const sha = $("ndSha").value.trim();
    if (sha && !/^[0-9a-fA-F]{64}$/.test(sha)) {
      toast("SHA-256 必须是 64 位十六进制", true);
      return;
    }

    const btn = $("ndStart");
    btn.disabled = true;
    try {
      await invoke("new_download", {
        args: {
          url,
          filename: $("ndFilename").value.trim() || null,
          threads: Number($("ndThreads").value || 1),
          save_dir: $("ndDir").value.trim() || null,
          start_at: startAt,
          sha256: sha || null,
        },
      });
      // 建完把地址清掉，方便连续粘下一个
      $("ndUrl").value = "";
      $("ndFilename").value = "";
      $("ndStartAt").value = "";
      $("ndSha").value = "";
      onDownloadAccepted();
    } catch (e) {
      toast(String(e), true);
    } finally {
      btn.disabled = false;
    }
  }

  function pickNewDir() {
    invoke("plugin:dialog|open", {
      options: { directory: true, multiple: false, title: "选择保存目录" },
    })
      .then((picked) => {
        if (picked) $("ndDir").value = picked;
      })
      .catch((e) => toast("选择目录失败：" + String(e), true));
  }

  function renderMedia(info) {
    $("mediaTitle").textContent = info.title || "（无标题）";
    $("mediaExtractor").textContent = info.extractor || "unknown";
    $("mediaUploader").textContent = info.uploader || "";
    $("mediaDuration").textContent = fmtDuration(info.duration);

    const thumb = $("mediaThumb");
    if (info.thumbnail) {
      // CSP 的 img-src 不含 http:：http 封面会静默裂图。升级到 https 尝试
      // 一次（主流站点都有 https）；仍失败的站点宁可裂图也不放松 CSP。
      thumb.src = info.thumbnail.replace(/^http:\/\//i, "https://");
      thumb.style.display = "";
    } else {
      thumb.removeAttribute("src");
      thumb.style.display = "none";
    }

    const sel = $("formatSelect");
    sel.innerHTML = "";
    info.formats.forEach((f, i) => {
      const o = el("option");
      const size = f.filesize ? " — " + fmtBytes(f.filesize) : "";
      o.value = String(i);
      o.textContent = f.label + size;
      // 后端已经算好了推荐项（最高分辨率下的 H.264），默认选中它
      if (f.recommended) o.selected = true;
      sel.appendChild(o);
    });

    sel.onchange = () => updateFmtDetail(info);
    updateFmtDetail(info);

    $("mediaCard").classList.remove("hidden");
    syncStartButtons();
  }

  function updateFmtDetail(info) {
    const idx = Number($("formatSelect").value || 0);
    const f = info.formats[idx];
    if (!f) return;
    const parts = [`format_id: ${f.format_id}`];
    if (f.ext) parts.push(`容器: ${f.ext}`);
    if (f.resolution) parts.push(`分辨率: ${f.resolution}`);
    if (f.fps) parts.push(`${Math.round(f.fps)}fps`);
    if (f.filesize) parts.push(`大小: ${fmtBytes(f.filesize)}`);
    if (!f.has_audio) {
      parts.push(
        info.best_audio_format
          ? `无音轨 → 将自动合并音轨 ${info.best_audio_format}`
          : "无音轨 → 该视频将无声"
      );
    }
    $("fmtDetail").textContent = parts.join("   ·   ");
  }

  // ---------- 加入下载 ----------
  async function doDownload() {
    if (!currentMedia) return;
    const idx = Number($("formatSelect").value || 0);
    const f = currentMedia.formats[idx];
    if (!f) return;

    const btn = $("downloadBtn");
    btn.disabled = true;
    try {
      await invoke("enqueue_download", {
        args: {
          url: currentMedia.webpage_url,
          title: currentMedia.title,
          thumbnail: currentMedia.thumbnail || null,
          duration: currentMedia.duration || null,
          format_id: f.format_id,
          quality_label: f.label,
          has_audio: f.has_audio,
          best_audio_format: currentMedia.best_audio_format || null,
          // 媒体任务也支持单独指定目录（IDM 的核心体验之一）
          save_dir: $("ndDir").value.trim() || null,
          // 用所选格式的容器扩展名做分类，而不是网页标题里的后缀
          expected_ext: f.ext || null,
          expected_bytes: f.filesize || null,
        },
      });
      $("enqueueHint").textContent = "可在「任务」页查看进度";
      onDownloadAccepted();
    } catch (e) {
      toast(String(e), true);
    } finally {
      btn.disabled = false;
    }
  }

  // ---------- 任务列表 ----------
  const STATE_TEXT = {
    queued: "排队中",
    downloading: "下载中",
    merging: "合并中",
    done: "已完成",
    failed: "失败",
    canceled: "已取消",
    paused: "已暂停",
  };

  // 多选：只存 id 集合，卡片渲染时据此还原勾选态。
  // 用 Set 而不是数组：菜单里「暂停选中」可能在几千条任务上跑，includes 是 O(n)。
  const selectedIds = new Set();

  const ACTIVE_STATES = new Set(["queued", "downloading", "merging"]);

  function toggleSelect(id, on) {
    if (on) selectedIds.add(id);
    else selectedIds.delete(id);
    updateSelHint();
  }

  // ---------- 任务行右键菜单 ----------
  //
  // 设计要点（三条都踩过坑才这么定）：
  // 1. **右键作用于「这一行」还是「选中集合」**：IDM/浏览器下载器都是「右键
  //    未选中项 → 只操作它；右键已选中项 → 操作整个选中集合」。这里照抄：
  //    右键落在未选中行上会先把它变成唯一选中项，再按选中集合操作。
  // 2. **菜单项按状态置灰而不是隐藏**：位置固定的菜单比忽长忽短的菜单好点。
  // 3. **关闭时机**：点任意处 / 滚动 / 窗口失焦 / Esc 都要关，否则会有一个
  //    永远挂在屏幕上的幽灵菜单（webview 里尤其明显）。

  const ctxMenu = $("ctxMenu");
  // 这次右键的目标任务 id（单条）。批量操作时仍以 selectedIds 为准。
  let ctxTargetId = null;

  function hideCtxMenu() {
    if (ctxMenu) ctxMenu.classList.add("hidden");
  }

  // ---------- 编辑任务 ----------
  let editTargetId = null;

  function fillEditThreads(sel, n) {
    fillThreadSelect(sel, n);
    const en = appSettings && appSettings.language === "en";
    // 0 = 自动（后端按文件大小和分块上限自己决定）。保留这个语义，
    // 否则用户只想改文件名，保存时却把「自动」悄悄变成了某个固定线程数。
    if (n === 0) {
      const o = el("option");
      o.value = "0";
      o.textContent = en ? "Auto" : "自动";
      sel.appendChild(o);
      sel.value = "0";
      return;
    }
    // 历史任务可能存了枚举外的线程数（比如 6）：补一个精确选项，
    // 否则 select 会吸附到 4/8，用户只想改文件名却顺手改了线程。
    if (n > 0 && ![...sel.options].some((o) => o.value === String(n))) {
      const o = el("option");
      o.value = String(n);
      o.textContent = en ? n + " threads" : n + " 线程";
      sel.appendChild(o);
    }
    if (n > 0) sel.value = String(n);
  }

  function openEditDialog(task) {
    if (!task) return;
    editTargetId = task.id;
    $("editUrl").value = task.url || "";
    $("editName").value = task.title || "";
    $("editDir").value = task.save_dir || "";
    $("editDir").placeholder =
      (appSettings && appSettings.download_dir) || "默认下载目录";
    // 任务级限速/校验预期：None = 跟随全局（输入框留空）
    $("editRate").value = typeof task.rate_kb === "number" ? task.rate_kb : "";
    $("editSha").value = task.sha256 || "";
    const direct = task.kind === "direct";
    $("editThreadsRow").classList.toggle("hidden", !direct);
    if (direct) {
      const current =
        typeof task.threads === "number"
          ? task.threads
          : (appSettings && appSettings.default_threads) || 8;
      fillEditThreads($("editThreads"), current);
    }
    $("editOverlay").classList.remove("hidden");
    $("editName").focus();
  }

  function closeEditDialog() {
    $("editOverlay").classList.add("hidden");
    editTargetId = null;
  }

  async function saveEditDialog() {
    const task = editTargetId ? taskMap.get(editTargetId) : null;
    if (!task) return;
    const url = $("editUrl").value.trim();
    if (!url) return toast("下载地址不能为空", true);
    if (!/^(https?|ftp):\/\//i.test(url)) {
      return toast("地址必须以 http://、https:// 或 ftp:// 开头", true);
    }
    const title = $("editName").value.trim();
    if (!title) return toast("文件名不能为空", true);
    const dir = $("editDir").value.trim();
    // SHA-256：空 = 清除；给了就必须是 64 位 hex
    const sha = $("editSha").value.trim();
    if (sha && !/^[0-9a-fA-F]{64}$/.test(sha)) {
      return toast("SHA-256 必须是 64 位十六进制", true);
    }
    const rateRaw = $("editRate").value.trim();
    const rateKb = rateRaw === "" ? null : Math.max(0, Number(rateRaw) || 0);
    // 改目录 / 文件名 / 线程数都会让断点失效：临时文件与分段布局分别绑在
    // 「最终路径」和 .veloxgrab-meta 上，换任何一个都只能从头重下。
    // 限速与校验预期是元数据，不影响断点。
    const dirChanged = dir !== (task.save_dir || "").trim();
    const urlChanged = url !== (task.url || "").trim();
    if (urlChanged) {
      const ok = await askConfirm(
        "换了下载地址后，已有的全部进度和断点都会作废，任务会从头重新下载。\n\n继续吗？"
      );
      if (!ok) return;
    }
    const titleChanged = title !== (task.title || "").trim();
    const threadsChanged =
      task.kind === "direct" && Number($("editThreads").value) !== task.threads;
    // 换 URL 时进度本来就会归零，不再重复提示断点问题
    const otherChange = dirChanged || titleChanged || threadsChanged;
    if (!urlChanged && otherChange && task.downloaded_bytes > 0) {
      const what = [
        dirChanged ? "保存目录" : null,
        titleChanged ? "文件名" : null,
        threadsChanged ? "线程数" : null,
      ]
        .filter(Boolean)
        .join("、");
      const ok = await askConfirm(
        `这个任务已经下载了一部分。修改${what}后，已有断点不会迁移，任务会从头重新开始。\n\n继续吗？`
      );
      if (!ok) return;
    }
    const args = { id: task.id, title, save_dir: dir, rate_kb: rateKb, sha256: sha || null };
    if (urlChanged) args.url = url;
    if (task.kind === "direct") {
      args.threads = Number($("editThreads").value) || 0;
    }
    try {
      await invoke("edit_task", { args });
      closeEditDialog();
      toast("任务已更新");
      refreshTasks();
    } catch (e) {
      toast(String(e), true);
    }
  }

  function showCtxMenu(x, y, task) {
    if (!ctxMenu || !task) return;
    ctxTargetId = task.id;

    // 右键菜单的「选中语义」：落在未选中行上 → 只选它。
    // 落在已选中行上 → 保留现有选中集合（用户可能刚 Ctrl+A 想批量操作）。
    if (!selectedIds.has(task.id)) {
      selectedIds.clear();
      selectedIds.add(task.id);
      updateSelHint();
      // 行高亮要跟着变：这里不能 refreshTasks 全量重建（会丢右键上下文），
      // 只同步所有行的 selected 类。
      document.querySelectorAll(".task-row").forEach((r) => {
        r.classList.toggle("selected", selectedIds.has(r.dataset.id));
        const cb = r.querySelector(".task-check");
        if (cb) cb.checked = selectedIds.has(r.dataset.id);
      });
    }

    // 菜单项可用性：按**主目标**（右键那一行）的状态决定，因为批量操作里
    // 「打开文件/复制路径」只对已完成任务有意义，而主目标最能代表用户意图。
    const done = task.state === "done" && !!task.output_path;
    // 实物被删的 done：打开/定位没意义（点了报错），重下反而成立；
    // 复制路径仍有用（用户可能想看看它原来的位置）。
    const gone = task.state === "done" && !!task.file_missing;
    const acting = ACTIVE_STATES.has(task.state);
    const paused = task.state === "paused" || task.state === "failed";
    const setEnabled = (name, on) => {
      const b = ctxMenu.querySelector(`[data-ctx="${name}"]`);
      if (b) b.disabled = !on;
    };
    setEnabled("open", done && !gone);
    setEnabled("reveal", done && !gone);
    setEnabled("copyPath", done);
    setEnabled("copyUrl", !!task.url);
    setEnabled("pause", acting);
    setEnabled("resume", paused);
    setEnabled("redo", !acting);
    // 编辑只对排队/暂停/失败开放；运行中先暂停，终态请重建
    setEnabled(
      "edit",
      ["queued", "paused", "failed"].includes(task.state)
    );
    setEnabled("remove", true);
    setEnabled("bump", task.state === "queued");
    // 「删除任务和文件」只在主目标是已完成且有成品时可用：其余情况删无可删，
    // 灰掉比点了弹「没有可删的文件」更清楚。
    setEnabled("removeWithFile", done);

    // 先显示再量尺寸，才能按视口边界收拢（否则贴右下角的菜单会被切掉）
    ctxMenu.classList.remove("hidden");
    const rect = ctxMenu.getBoundingClientRect();
    const pad = 6;
    const maxX = window.innerWidth - rect.width - pad;
    const maxY = window.innerHeight - rect.height - pad;
    ctxMenu.style.left = Math.max(pad, Math.min(x, maxX)) + "px";
    ctxMenu.style.top = Math.max(pad, Math.min(y, maxY)) + "px";
  }

  // 双击行 = 打开文件（IDM 习惯）。只对已完成且有实物（未被删）的任务生效；
  // 其余状态双击不做任何事，避免误触发。
  // 搜索框：输入即过滤（输入防抖 150ms，避免每个字符一次全量重建）；
  // Esc 清空（全局 Esc 分支之外单独挂，输入框里的 Esc 不抢弹窗逻辑）。
  let searchTimer = null;
  $("taskSearch").addEventListener("input", (e) => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      searchQuery = e.target.value.trim().toLowerCase();
      refreshTasks();
    }, 150);
  });
  $("taskSearch").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.target.value = "";
      searchQuery = "";
      refreshTasks();
    }
  });

  $("taskList").addEventListener("dblclick", (e) => {
    if (e.target.closest(".task-check")) return;
    const row = e.target.closest(".task-row");
    if (!row) return;
    const task = taskMap.get(row.dataset.id);
    if (!task || task.state !== "done" || !task.output_path || task.file_missing) return;
    invoke("open_task_file", { path: task.output_path }).catch((err) =>
      toast(`打开失败：${String(err)}`, true)
    );
  });

  // 事件委托：行是动态重建的，逐行挂 contextmenu 会随 refreshTasks 反复增删监听。
  $("taskList").addEventListener("contextmenu", (e) => {
    const row = e.target.closest(".task-row");
    if (!row) return;
    const task = taskMap.get(row.dataset.id);
    if (!task) return;
    e.preventDefault();
    showCtxMenu(e.clientX, e.clientY, task);
  });

  ctxMenu.addEventListener("click", async (e) => {
    const btn = e.target.closest("[data-ctx]");
    if (!btn || btn.disabled) return;
    const action = btn.dataset.ctx;
    const main = ctxTargetId ? taskMap.get(ctxTargetId) : null;
    const picked = [...selectedIds]
      .map((id) => taskMap.get(id))
      .filter(Boolean);
    hideCtxMenu();

    try {
      if (action === "open") {
        const targets = picked.filter((t) => t.state === "done" && t.output_path);
        if (!targets.length) return toast("选中的任务还没有可打开的文件", true);
        // 多个文件时全开；Windows 默认程序会各自起窗口
        for (const t of targets) {
          await invoke("open_task_file", { path: t.output_path });
        }
      } else if (action === "reveal") {
        const targets = picked.filter((t) => t.state === "done" && t.output_path);
        if (!targets.length) return toast("选中的任务还没有完成的文件", true);
        for (const t of targets) {
          await invoke("reveal_task_file", { path: t.output_path });
        }
      } else if (action === "copyPath") {
        const paths = picked
          .filter((t) => t.state === "done" && t.output_path)
          .map((t) => t.output_path);
        if (!paths.length) return toast("没有可复制的文件路径", true);
        // 多条时换行分隔：粘进资源管理器地址栏/记事本都能直接用
        await invoke("copy_text", { text: paths.join("\n") });
        toast(paths.length > 1 ? `已复制 ${paths.length} 个文件路径` : "已复制文件路径");
      } else if (action === "copyUrl") {
        const urls = picked.map((t) => t.url).filter(Boolean);
        if (!urls.length) return toast("没有可复制的链接", true);
        await invoke("copy_text", { text: urls.join("\n") });
        toast(urls.length > 1 ? `已复制 ${urls.length} 个下载链接` : "已复制下载链接");
      } else if (action === "pause") {
        const n = await invoke("pause_tasks", { args: { ids: picked.map((t) => t.id) } });
        toast(n > 0 ? `已暂停 ${n} 个任务` : "没有可暂停的任务");
        refreshTasks();
      } else if (action === "resume") {
        const n = await invoke("resume_tasks", { args: { ids: picked.map((t) => t.id) } });
        toast(n > 0 ? `已恢复 ${n} 个任务` : "没有可恢复的任务");
        refreshTasks();
      } else if (action === "redo") {
        const n = await invoke("redo_tasks", { args: { ids: picked.map((t) => t.id) } });
        toast(n > 0 ? `已重新下载 ${n} 个任务` : "没有可重下的任务");
        refreshTasks();
      } else if (action === "edit") {
        openEditDialog(main);
      } else if (action === "bump") {
        // 只对「还在队列里」的任务有效；后端返回实际挪动数
        const n = await invoke("bump_tasks", { args: { ids: picked.map((t) => t.id) } });
        toast(n > 0 ? `已把 ${n} 个任务挪到队列最前` : "选中任务不在队列中（运行中或已结束）");
        refreshTasks();
      } else if (action === "remove") {
        const ids = picked.map((t) => t.id);
        const ok = await askConfirm(
          `确定移除选中的 ${ids.length} 个任务吗？\n\n正在下载的任务会被中止，未完成的临时文件会被删除。\n（已下载完成的文件会保留在硬盘上；如需连文件一起删，请用「删除任务和文件」）`
        );
        if (!ok) return;
        const r = await invoke("remove_tasks", { args: { ids } });
        ids.forEach((id) => selectedIds.delete(id));
        updateSelHint();
        toast(`已移除 ${r.removed} 个任务`);
        refreshTasks();
      } else if (action === "removeWithFile") {
        // 只对「有成品文件」的已完成任务生效，其余任务退回普通移除：
        // 让用户在混合选区里点这一项时不必先筛一遍。
        const ids = picked.map((t) => t.id);
        const done = picked.filter((t) => t.state === "done" && t.output_path);
        if (!done.length) {
          toast("选中的任务里没有已下载完成的文件");
          return;
        }
        const skip = ids.length - done.length;
        const ok = await askConfirm(
          `确定删除 ${done.length} 个任务的下载文件吗？\n\n` +
            `文件会从硬盘上永久删除，无法恢复。\n` +
            (skip > 0 ? `另有 ${skip} 个未完成的任务只移除记录（没有成品文件）。\n` : "") +
            `\n下载目录：${done[0].save_dir || "默认下载目录"}`
        );
        if (!ok) return;
        // 返回值是 { removed, files_deleted, errors }：文件删除可能失败
        // （被占用 / 判据拒绝）。后端现在**文件没删成就不动任务记录**，
        // 所以失败时任务仍在列表里，用户能直接重试。
        const r = await invoke("remove_tasks", { args: { ids, delete_files: true } });
        if (r.errors && r.errors.length) {
          // 失败的留着不勾掉，方便用户对着这几行再点一次
          toast(
            `${r.errors.length} 个文件未能删除，任务已保留，可稍后重试：\n${r.errors.join("\n")}`,
            true
          );
        } else {
          ids.forEach((id) => selectedIds.delete(id));
          updateSelHint();
          toast(`已删除 ${r.removed} 个任务及其文件（共 ${r.files_deleted} 个文件）`);
        }
        refreshTasks();
      }
    } catch (err) {
      toast(String(err), true);
    }
  });

  // 关闭时机：点击别处 / Esc / 滚动 / 窗口失焦 / 尺寸变化
  document.addEventListener("mousedown", (e) => {
    if (ctxMenu && !ctxMenu.contains(e.target)) hideCtxMenu();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") hideCtxMenu();
  });
  window.addEventListener("blur", hideCtxMenu);
  window.addEventListener("resize", hideCtxMenu);
  // 用捕获阶段：滚动事件不冒泡，挂在 document 上默认收不到
  document.addEventListener("scroll", hideCtxMenu, true);
  // 右键本身在别处按下时也要关（否则会同时开出两个菜单）
  document.addEventListener(
    "contextmenu",
    (e) => {
      if (ctxMenu && !e.target.closest(".task-row")) hideCtxMenu();
    },
    true
  );

  function updateSelHint() {
    const n = selectedIds.size;

    // 状态栏的「已选 N 项」和菜单栏右侧的提示是同一份信息，
    // 都放在这里更新：勾选框的 onchange 只调 toggleSelect，不会走 updateSummary。
    const st = $("statusSelected");
    if (st) st.textContent = n > 0 ? `已选 ${n} 项` : "";
    const sep = $("selSep");
    if (sep) sep.classList.toggle("hidden", n === 0);

    const hint = $("menuSelHint");
    if (!hint) return;
    hint.textContent = n > 0 ? `已选 ${n} 项` : "";
    hint.classList.toggle("hidden", n === 0);

    // 全选框跟着走：只剩部分选中时用 indeterminate，
    // 让用户一眼看出「不是全选也不是没选」。基数用**当前视图可见集**——
    // 用 taskMap 全量的话，在「已完成」里全选完勾选框会一直卡在半选态。
    const all = $("selectAll");
    if (all) {
      const total = visibleIds().length;
      all.checked = n > 0 && n >= total;
      all.indeterminate = n > 0 && n < total;
    }
  }

  // ---------- 文件类型分类 ----------
  //
  // 侧边栏的「分类」是纯展示层的判断：后端 Task 里只有 kind = video/direct，
  // 分不出压缩包和文档。这里按扩展名归堆，认不出的落不进任何一类
  // （宁可不显示，也不要把 .bin 硬塞进「程序」里）。
  const CAT_EXT = {
    video: ["mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "m4v", "ts", "mpg", "mpeg", "rmvb"],
    music: ["mp3", "flac", "wav", "aac", "m4a", "ogg", "opus", "wma", "ape"],
    program: ["exe", "msi", "apk", "dmg", "pkg", "deb", "rpm", "appx", "msix", "ipa"],
    archive: ["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso", "cab", "tgz"],
    document: ["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "epub", "mobi", "csv", "md", "rtf"],
  };

  function fileCategory(t) {
    // 视频站解析出来的任务不必看扩展名：后端已经标了 kind
    if (t.kind === "video") return "video";
    const src = t.output_path || t.title || t.url || "";
    // 先去掉 ?query / #hash，否则 "a.zip?token=x" 取不到 zip
    const clean = src.split(/[?#]/)[0];
    const m = /\.([a-z0-9]{1,5})$/i.exec(clean);
    if (!m) return null;
    const ext = m[1].toLowerCase();
    for (const [cat, exts] of Object.entries(CAT_EXT)) {
      if (exts.includes(ext)) return cat;
    }
    return null;
  }

  // 任务行左侧的类型图标：颜色由 .tr-icon.v/.m/.p/.a/.d 决定，
  // 认不出扩展名时退回中性的下载箭头。
  function fileIcon(t) {
    const map = {
      video: ["v", "▶"],
      music: ["m", "♪"],
      program: ["p", "⚙"],
      archive: ["a", "▣"],
      document: ["d", "▤"],
    };
    return map[fileCategory(t)] || ["", "⬇"];
  }

  // 当前视图是否接受这条任务。分类项与状态项是两套维度，
  // 但都作用在同一张表上，所以用同一个 currentView 决定。
  // 搜索框：匹配名称 / URL / 输出路径（大小写不敏感的子串）。空串 = 不过滤。
  // 与视图过滤是 AND 关系；清空后立即恢复当前视图的全集。
  let searchQuery = "";
  function matchesSearch(t) {
    if (!searchQuery) return true;
    const q = searchQuery;
    return (
      (t.title || "").toLowerCase().includes(q) ||
      (t.url || "").toLowerCase().includes(q) ||
      (t.output_path || "").toLowerCase().includes(q)
    );
  }

  function matchView(t) {
    if (!matchesSearch(t)) return false;
    switch (currentView) {
      case "active":
        return t.state !== "done" && t.state !== "canceled";
      case "done":
        return t.state === "done";
      case "all":
        return true;
      default:
        return fileCategory(t) === currentView;
    }
  }

  function renderTask(t) {
    // 只挂 task-row，不挂 .task：.task 是历史页卡片的样式（圆角、内边距、
    // 边框），套到表格行上会把 grid 布局撑变形。
    const row = el("div", "task-row");
    row.dataset.id = t.id;
    if (selectedIds.has(t.id)) row.classList.add("selected");

    // 勾选框：菜单里「恢复选中 / 暂停选中 / 删除选中」都依赖它
    const check = el("input", "task-check");
    check.type = "checkbox";
    check.checked = selectedIds.has(t.id);
    check.onchange = () => {
      toggleSelect(t.id, check.checked);
      row.classList.toggle("selected", check.checked);
    };
    row.appendChild(check);

    // 名称列：类型图标 + 两行文字（主行文件名，副行来源/清晰度/错误摘要）。
    // 单独一列放不下路径，用 title 兜住，悬停能看全。
    const nameCell = el("div", "tr-name");
    const [iconCls, iconText] = fileIcon(t);
    nameCell.appendChild(el("span", "tr-icon" + (iconCls ? " " + iconCls : ""), iconText));

    const textCell = el("div", "tr-name-text");
    const title = el("div", "tr-title", t.title || t.url);
    title.title = t.output_path || t.url || t.title || "";
    textCell.appendChild(title);
    const subBits = [];
    // 直链任务的 quality_label 是后端填的「手动新建」，写出来等于没说；
    // 换成来源域名，用户一眼能看出这个文件是从哪儿下的。
    const host = hostOf(t.url);
    if (t.kind === "direct" && host) subBits.push(host);
    else if (t.quality_label) subBits.push(t.quality_label);
    const failed = t.state === "failed" && t.error;
    if (failed) subBits.push(t.error);
    else if (t.output_path && t.state === "done") subBits.push(t.output_path);
    if (subBits.length) {
      const sub = el("div", "tr-sub", subBits.join("  ·  "));
      // 失败摘要用错误色；完整堆栈仍在详情/编辑弹窗里
      if (failed) sub.classList.add("bad");
      sub.title = subBits.join("  ·  ");
      textCell.appendChild(sub);
    }
    nameCell.appendChild(textCell);
    row.appendChild(nameCell);

    // 大小：优先总量，没有总量（yt-dlp 解析前）时退回到已下载量
    const size = t.total_bytes || t.downloaded_bytes;
    row.appendChild(el("div", "tr-size", size ? fmtBytes(size) : "—"));

    const stateCell = el("div", "tr-state");
    // 实物被删的已完成任务：状态区别显示（徽标换文案换色），行内按钮
    // 也从「打开」换成「重下」，右键菜单同步（见 showCtxMenu 的 gone）。
    const gone = t.state === "done" && !!t.file_missing;
    stateCell.appendChild(
      el(
        "span",
        "task-state " + (gone ? "gone" : t.state),
        gone ? "文件已删除" : STATE_TEXT[t.state] || t.state
      )
    );
    row.appendChild(stateCell);

    // 进度列：进度条 + 百分比同格，表格里比卡片更需要「一眼看到数字」
    const progCell = el("div", "tr-prog");
    const bar = el("div", "bar");
    const fill = el("div", "bar-fill");
    const done = t.state === "done";
    const pct = done ? 100 : t.progress || 0;
    fill.style.width = pct + "%";
    if (done) fill.classList.add("done");
    if (t.state === "failed") fill.classList.add("failed");
    if (t.state === "paused") fill.classList.add("paused");
    bar.appendChild(fill);
    progCell.appendChild(bar);
    progCell.appendChild(el("span", "tr-pct", done ? "100%" : pct ? pct.toFixed(1) + "%" : "—"));
    row.appendChild(progCell);

    // 速度/剩余时间只在真正下载时有意义。暂停后 speed 字段会停在
    // "0 B/s" 残值上，写出来反而让人以为卡死了，所以一律留空。
    const live = t.state === "downloading";
    row.appendChild(el("div", "tr-speed", live && t.speed ? t.speed : "—"));
    row.appendChild(el("div", "tr-eta", live && t.eta ? t.eta : "—"));


    return row;
  }

  function revealItem(path) {
    return invoke("plugin:opener|reveal_item_in_dir", { paths: [path] }).catch((e) =>
      toast("打开失败：" + String(e), true)
    );
  }

  // 状态不变时的高频路径：只更新本行的进度/速度/剩余，不重建整行。
  // 10Hz × N 个任务重建整表会把按钮和焦点一起销毁。
  function updateCardProgress(row, t) {
    const fill = row.querySelector(".bar-fill");
    const done = t.state === "done";
    const pct = done ? 100 : t.progress || 0;
    if (fill) fill.style.width = pct + "%";
    const pctEl = row.querySelector(".tr-pct");
    if (pctEl) pctEl.textContent = done ? "100%" : pct ? pct.toFixed(1) + "%" : "—";
    const live = t.state === "downloading";
    const sp = row.querySelector(".tr-speed");
    if (sp) sp.textContent = live && t.speed ? t.speed : "—";
    const eta = row.querySelector(".tr-eta");
    if (eta) eta.textContent = live && t.eta ? t.eta : "—";
    const sz = row.querySelector(".tr-size");
    const total = t.total_bytes || t.downloaded_bytes;
    if (sz && total) sz.textContent = fmtBytes(total);
  }

  // 侧边栏计数 + 状态栏。只依赖 taskMap，不必为了更新它们再跑一次 IPC。
  function updateSummary() {
    let total = 0;
    let active = 0;
    let done = 0;
    const cats = { video: 0, music: 0, program: 0, archive: 0, document: 0 };
    let speedSum = 0;

    for (const t of taskMap.values()) {
      total++;
      const acting = ACTIVE_STATES.has(t.state);
      if (acting) active++;
      if (t.state === "done") done++;
      const cat = fileCategory(t);
      if (cat && cats[cat] !== undefined) cats[cat]++;
      if (t.state === "downloading") speedSum += parseSpeed(t.speed);
    }

    // 侧边栏角标。「正在下载」用 active（含排队），因为用户点进去是想看
    // 「还有多少没下完」，「已完成」用 done，其余按类型堆。
    $("cntAll").textContent = String(total);
    $("cntActive").textContent = String(active);
    $("cntDone").textContent = String(done);
    $("cntVideo").textContent = String(cats.video);
    $("cntMusic").textContent = String(cats.music);
    $("cntProgram").textContent = String(cats.program);
    $("cntArchive").textContent = String(cats.archive);
    $("cntDocument").textContent = String(cats.document);

    $("statusTasks").textContent = `${total} 个任务`;
    $("statusSpeed").textContent = speedSum > 0 ? fmtBytes(speedSum) + "/s" : "0 B/s";
    // 分隔符跟着「已选 N 项」一起显隐，否则没选中时状态栏会留一个孤零零的竖线
    const selN = selectedIds.size;
    $("statusSelected").textContent = selN ? `已选 ${selN} 项` : "";
    $("selSep").classList.toggle("hidden", selN === 0);

    // 任务被删掉后，残留的选中 id 会让「删除选中」的计数对不上
    for (const id of [...selectedIds]) {
      if (!taskMap.has(id)) selectedIds.delete(id);
    }
    updateSelHint();
  }

  // 全量同步：只在启动、切换视图、增删任务时调用
  //
  // 与 task-progress 增量通道的竞态防护（两条都要）：
  //  - refreshGen 递增代号：两轮全量刷新并发时，晚发出却先返回的快照先落地，
  //    晚返回的旧快照会被代号检查丢弃，不会把新状态打回去。
  //  - refreshDirty：快照在途时收到过 task-progress 事件，说明这份快照
  //    （invoke 时刻取的数）必然不包含该事件——落地后补刷一轮，否则「暂停」
  //    等状态跃迁会被旧快照回退成「下载中」，而进入暂停/终态的任务不再发
  //    任何事件，错误状态永远无法自愈。
  let refreshGen = 0;
  let refreshInFlight = false;
  let refreshDirty = false;
  let refreshFailures = 0;

  async function refreshTasks() {
    const gen = ++refreshGen;
    refreshInFlight = true;
    try {
      const list = await invoke("list_tasks");
      if (gen === refreshGen) {
        taskMap = new Map(list.map((t) => [t.id, t]));

        const host = $("taskList");
        host.innerHTML = "";
        const visible = list.filter(matchView);
        visible.forEach((t) => host.appendChild(renderTask(t)));

        // 空态提示按视图换措辞：分类里空着说「还没有下载任务」会让人
        // 以为任务没了，其实只是这个分类下没有。
        const empty = $("listEmpty");
        empty.classList.toggle("hidden", visible.length > 0);
        // 列表容器和空提示都带 flex:1，同时可见会各占一半高度，
        // 空提示就跑到屏幕中间偏上了。有任务时把空提示藏掉，没任务时把
        // 列表容器藏掉，让留下的那个吃满高度。
        host.classList.toggle("hidden", visible.length === 0);
        empty.textContent =
          currentView === "all"
            ? "还没有下载任务。点「添加URL」或使用浏览器嗅探。"
            : "这个分类下还没有任务。";

        updateSummary();
      }
    } catch (e) {
      // 静默重试，但连续失败（后端崩溃/重启中）要让用户知道列表已停更
      refreshFailures++;
      if (refreshFailures === 3) {
        toast("任务列表刷新失败，正在重试——若持续失败请重启程序", true);
      }
    } finally {
      // 只有仍是「最新一轮」才清在途标记：旧快照晚到时，在途的是新一轮。
      if (gen === refreshGen) {
        refreshInFlight = false;
        refreshFailures = 0;
        if (refreshDirty) {
          refreshDirty = false;
          scheduleRefresh();
        }
      }
    }
  }

  // 兜底节流：本地缺卡片时才补一次全量同步
  let refreshTimer = null;
  function scheduleRefresh() {
    if (refreshTimer) return;
    refreshTimer = setTimeout(() => {
      refreshTimer = null;
      refreshTasks();
    }, 300);
  }

  // ---------- 设置 ----------
  let settingsLoaded = false;

  function syncCategoryDetail() {
    $("categoryRules").classList.toggle("hidden", !$("setCategoriesEnabled").checked);
  }

  // 一条规则 = 子目录 + 扩展名列表。行是动态生成的：默认五条来自后端，
  // 用户可以增删；保存时后端还会清洗目录名、合并同目录、限制条数。
  function addCategoryRow(rule) {
    const row = el("div", "cat-row");
    const folder = el("input");
    folder.type = "text";
    folder.value = (rule && rule.folder) || "";
    folder.placeholder = "Video";
    folder.spellcheck = false;
    const exts = el("input");
    exts.type = "text";
    exts.value =
      rule && Array.isArray(rule.extensions) ? rule.extensions.join(", ") : "";
    exts.placeholder = "mp4, mkv, webm";
    exts.spellcheck = false;
    const del = el("button", "icon-btn", "×");
    del.type = "button";
    del.title = "删除这条规则";
    del.onclick = () => row.remove();
    row.append(folder, exts, del);
    $("categoryRuleRows").appendChild(row);
  }

  function renderCategoryRules(rules) {
    const box = $("categoryRuleRows");
    box.innerHTML = "";
    const list = Array.isArray(rules) ? rules : [];
    list.forEach((r) => addCategoryRow(r));
    // 用户把规则删光后至少留一条空行，避免开关打开时是一片空白
    if (!list.length) addCategoryRow(null);
  }

  function collectCategories() {
    return Array.from($("categoryRuleRows").querySelectorAll(".cat-row"))
      .map((row) => {
        const inputs = row.querySelectorAll("input");
        const extensions = String(inputs[1].value || "")
          .split(/[,，;；\s]+/)
          .map((s) => s.trim().replace(/^\.+/, "").toLowerCase())
          .filter(Boolean);
        return { folder: inputs[0].value.trim(), extensions };
      })
      .filter((r) => r.folder && r.extensions.length);
  }

  // 一套请求头预设 = 名称 + host 作用域 + 多行 `Name: value`。
  // host 留空表示对所有站点生效；具体白名单/危险头过滤由后端做，
  // 前端只负责把用户输入原样带过去。
  function addHeaderPresetRow(preset) {
    const card = el("div", "hdr-preset");
    const row = el("div", "hdr-row");
    const name = el("input");
    name.type = "text";
    name.className = "hdr-name";
    name.value = (preset && preset.name) || "";
    name.placeholder = "预设名称（可选）";
    name.spellcheck = false;
    const host = el("input");
    host.type = "text";
    host.className = "hdr-host";
    host.value = (preset && preset.host) || "";
    host.placeholder = "example.com（留空 = 所有站点）";
    host.spellcheck = false;
    const del = el("button", "icon-btn", "×");
    del.type = "button";
    del.title = "删除这套预设";
    del.onclick = () => card.remove();
    row.append(name, host, del);

    const headers = el("textarea", "hdr-headers");
    headers.rows = 4;
    headers.spellcheck = false;
    headers.placeholder = "Referer: https://example.com/\nUser-Agent: Mozilla/5.0 ...";
    headers.value = Array.isArray(preset && preset.headers)
      ? preset.headers.map((h) => `${h.name}: ${h.value}`).join("\n")
      : "";

    card.append(row, headers);
    $("headerPresetRows").appendChild(card);
  }

  function renderHeaderPresets(presets) {
    const box = $("headerPresetRows");
    box.innerHTML = "";
    const list = Array.isArray(presets) ? presets : [];
    list.forEach((p) => addHeaderPresetRow(p));
  }

  function collectHeaderPresets() {
    return Array.from($("headerPresetRows").querySelectorAll(".hdr-preset"))
      .map((card) => {
        const name = card.querySelector(".hdr-name").value.trim();
        const host = card.querySelector(".hdr-host").value.trim();
        const headers = String(card.querySelector(".hdr-headers").value || "")
          .split(/\r?\n/)
          .map((line) => line.trim())
          .filter((line) => line && !line.startsWith("#"))
          .map((line) => {
            const i = line.indexOf(":");
            if (i <= 0) return null;
            return {
              name: line.slice(0, i).trim(),
              value: line.slice(i + 1).trim(),
            };
          })
          .filter((h) => h && h.name && h.value);
        return { name, host, headers };
      })
      .filter((p) => p.headers.length);
  }

  // 滑块轨道填充：Chromium 没有 ::-moz-range-progress，只能用 --p 画渐变
  function syncRangeFill(input) {
    const min = Number(input.min) || 0;
    const max = Number(input.max) || 100;
    const pct = max > min ? ((Number(input.value) - min) / (max - min)) * 100 : 0;
    input.style.setProperty("--p", pct + "%");
  }

  function fillSettingsForm(s) {
    $("setDownloadDir").value = s.download_dir || "";
    // 手动新建的默认目录：用户没填时用全局默认目录填空，省得每次都要想
    if (!$("ndDir").value) $("ndDir").placeholder = s.download_dir || "";
    $("setConcurrent").value = s.max_concurrent;
    $("setConcurrentVal").textContent = s.max_concurrent;
    $("setDefaultThreads").value = s.default_threads ?? 8;
    $("setDefaultThreadsVal").textContent = s.default_threads ?? 8;
    $("setFragments").value = s.concurrent_fragments;
    $("setFragmentsVal").textContent = s.concurrent_fragments;
    ["setConcurrent", "setDefaultThreads", "setFragments"].forEach((id) =>
      syncRangeFill($(id))
    );
    $("setHfCap").value = s.hf_thread_cap ?? 4;
    $("setHfToken").value = s.hf_token || "";
    $("setRateLimit").value = s.rate_limit_kb ?? 0;
    $("setMirrorPrefix").value = s.mirror_prefix || "";
    $("setGithubToken").value = s.github_token || "";
    $("setAria2").checked = !!s.aria2_enabled;
    syncAria2Detail();
    $("setCategoriesEnabled").checked = !!s.categories_enabled;
    renderCategoryRules(s.categories || []);
    syncCategoryDetail();
    $("setNameConflict").value = s.name_conflict || "keep_both";
    renderHeaderPresets(s.header_presets || []);
    $("setPopupSaveAs").checked = s.popup_save_as ?? true;
    $("setPopupQuality").checked = s.popup_quality ?? true;
    $("setAutoProgress").checked = s.auto_progress_popup ?? true;
    $("setMergeMp4").checked = !!s.merge_to_mp4;
    $("setAutostart").checked = !!s.autostart;
    $("setClipboardWatch").checked = s.clipboard_watch ?? false;
    $("setNotifyComplete").checked = s.notify_on_complete ?? true;
    $("setCloseMode").value = s.close_mode || "background";
    $("setBrowser").value = s.cookies_from_browser || "none";
    $("setCookiesFile").value = s.cookies_file || "";
    $("setProxyEnabled").checked = !!s.proxy_enabled;
    $("setProxyType").value = s.proxy_type || "http";
    $("setProxyHost").value = s.proxy_host || "";
    $("setProxyPort").value = s.proxy_port || "";
    $("setProxyUser").value = s.proxy_user || "";
    $("setProxyPass").value = s.proxy_pass || "";
    $("setLanguage").value = s.language || "zh";
    $("setTheme").value = s.theme || "system";
    syncProxyDetail();
  }

  function syncProxyDetail() {
    $("proxyDetail").classList.toggle("hidden", !$("setProxyEnabled").checked);
  }

  /** aria2 RPC 的详情区：开启时显示 token 与用法。 */
  async function syncAria2Detail() {
    const box = $("aria2Detail");
    const on = $("setAria2").checked;
    box.classList.toggle("hidden", !on);
    if (!on) return;
    try {
      const info = await invoke("aria2_info");
      $("aria2Token").value = (info && info.token) || "";
    } catch (e) {
      $("aria2Token").value = "";
    }
  }

  async function loadSettings() {
    try {
      const s = await invoke("get_settings");
      appSettings = s;
      fillSettingsForm(s);
      settingsLoaded = true;
    } catch (e) {
      toast("读取设置失败：" + String(e), true);
    }
  }

  function collectSettings() {
    return {
      download_dir: $("setDownloadDir").value.trim(),
      max_concurrent: Number($("setConcurrent").value) || 3,
      default_quality: "best",
      cookies_from_browser: $("setBrowser").value,
      cookies_file: $("setCookiesFile").value.trim(),
      concurrent_fragments: Number($("setFragments").value) || 4,
      merge_to_mp4: $("setMergeMp4").checked,
      default_threads: Number($("setDefaultThreads").value) || 8,
      hf_thread_cap: Number($("setHfCap").value) || 0,
      hf_token: $("setHfToken").value.trim(),
      popup_quality: $("setPopupQuality").checked,
      popup_save_as: $("setPopupSaveAs").checked,
      rate_limit_kb: Number($("setRateLimit").value) || 0,
      mirror_prefix: $("setMirrorPrefix").value.trim(),
      github_token: $("setGithubToken").value.trim(),
      aria2_enabled: $("setAria2").checked,
      categories_enabled: $("setCategoriesEnabled").checked,
      categories: collectCategories(),
      name_conflict: $("setNameConflict").value,
      header_presets: collectHeaderPresets(),
      auto_progress_popup: $("setAutoProgress").checked,
      autostart: $("setAutostart").checked,
      clipboard_watch: $("setClipboardWatch").checked,
      notify_on_complete: $("setNotifyComplete").checked,
      close_mode: $("setCloseMode").value,
      proxy_enabled: $("setProxyEnabled").checked,
      proxy_type: $("setProxyType").value,
      proxy_host: $("setProxyHost").value.trim(),
      proxy_port: Number($("setProxyPort").value) || 0,
      proxy_user: $("setProxyUser").value.trim(),
      proxy_pass: $("setProxyPass").value,
      language: $("setLanguage").value,
      theme: $("setTheme").value,
    };
  }

  /**
   * 主题单独一条窄路径：改了立刻生效、立刻落盘，不等「保存设置」。
   *
   * 语言要重启才完全生效（历史原因，动态文本没进字典），所以它可以跟着
   * 保存按钮走；主题没有这个限制，让用户为了换个颜色先去点保存是不可接受的——
   * 那样还会把表单里没填完的东西一起存了。
   */
  async function changeTheme(value) {
    const prev = (appSettings && appSettings.theme) || "system";
    // 先本地上色：IPC 往返有几十毫秒，等它回来再变会让下拉框看着「卡了一下」
    applyTheme(value);
    try {
      const saved = await invoke("set_theme", { theme: value });
      if (appSettings) appSettings.theme = saved;
      $("setTheme").value = saved;
      applyTheme(saved);
    } catch (e) {
      // 落盘失败就把画面退回原来的主题：不然界面是浅色、磁盘里是深色，
      // 下次启动又变回去，看起来像程序在跟用户作对。
      applyTheme(prev);
      $("setTheme").value = prev;
      toast("保存主题失败：" + String(e), true);
    }
  }

  async function saveSettings() {
    try {
      const saved = await invoke("save_settings", { settings: collectSettings() });
      const langChanged = !appSettings || appSettings.language !== saved.language;
      appSettings = saved;
      applyLanguage(saved.language || "zh");
      // 主题也走保存按钮带一遍（collectSettings 里有它），所以这里必须重新上色：
      // 「恢复默认」会把下拉框改回「跟随系统」，若不上色，界面还是旧的深色，
      // 磁盘里却已经是 system 了，下次启动突然变样。
      applyTheme(saved.theme || "system");
      $("saveHint").textContent = "已保存";
      toast("设置已保存");
      if (langChanged) {
        $("saveHint").textContent = "语言已保存，重启后完全生效";
      }
      setTimeout(() => ($("saveHint").textContent = ""), 2500);
      // 保存完成回主界面：设置页是「进来做个决定」的地方，不是常驻页
      showView("all");
    } catch (e) {
      toast("保存失败：" + String(e), true);
    }
  }

  async function restoreDefaults() {
    try {
      const d = await invoke("default_settings");
      fillSettingsForm(d);
      // 只填表单不上色：主题要等用户真的点了「保存设置」才落盘。
      // 现在就把画面改成跟随系统，而用户接着点「取消」，等于界面骗了他一次。
      $("setDefaultHint").textContent = "已填入默认值，点「保存设置」生效";
      setTimeout(() => ($("setDefaultHint").textContent = ""), 3000);
    } catch (e) {
      toast(String(e), true);
    }
  }

  // ---------- 关闭提示（设置里选「每次询问」） ----------
  $("closeTray").onclick = () => {
    $("closeOverlay").classList.add("hidden");
    invoke("hide_main_window").catch(() => {});
  };
  $("closeQuit").onclick = () => {
    $("closeOverlay").classList.add("hidden");
    // 走 Rust 的 quit_app：它触发 ExitRequested 清理（杀子进程 + 刷历史），
    // 直接关窗口会绕过那一套。
    invoke("quit_app").catch(() => {});
  };
  $("closeCancel").onclick = () => {
    $("closeOverlay").classList.add("hidden");
  };

  // ---------- 全部完成后动作 ----------
  //
  // 计划存在 Rust 内存里（不落盘），这里只负责选、存、显示。真正的触发
  // 由后端在「队列空 + 没有活跃任务 + 至少一个成功 + 没有失败」时做。
  const PLAN_LABELS = {
    shutdown: "关机",
    restart: "重启",
    sleep: "睡眠",
    open_file: "打开文件",
  };
  const PLAN_LABELS_EN = {
    shutdown: "Shut down",
    restart: "Restart",
    sleep: "Sleep",
    open_file: "Open file",
  };

  // 语言以 appSettings 为准：计划动作的提示是前端拼的，后端只发动作名。
  function planLabel(action) {
    const zh = !(appSettings && appSettings.language === "en");
    return (zh ? PLAN_LABELS : PLAN_LABELS_EN)[action] || action;
  }

  function renderPlanChip(action) {
    const chip = $("planChip");
    if (!action || !PLAN_LABELS[action]) {
      chip.classList.add("hidden");
      chip.textContent = "";
      chip.dataset.action = "";
      return;
    }
    const zh = !(appSettings && appSettings.language === "en");
    chip.textContent = zh
      ? `完成后：${planLabel(action)}`
      : `After: ${planLabel(action)}`;
    chip.title = zh ? "点击修改或取消" : "Click to change or cancel";
    chip.dataset.action = action;
    chip.classList.remove("hidden");
  }

  async function openPlanDialog() {
    let current = "none";
    try {
      current = (await invoke("get_plan")) || "none";
    } catch (e) {
      current = "none";
    }
    document.querySelectorAll('input[name="planAction"]').forEach((r) => {
      r.checked = r.value === current;
    });
    $("planOverlay").classList.remove("hidden");
  }

  $("planChip").onclick = () => openPlanDialog();
  $("planCancel").onclick = () => $("planOverlay").classList.add("hidden");
  $("planSave").onclick = async () => {
    const picked = document.querySelector('input[name="planAction"]:checked');
    const action = picked ? picked.value : "none";
    try {
      await invoke("set_plan", { action });
      // 回读而不是直接用 set_plan 的返回值：如果所有任务早就下完了，
      // 后端会在 set_plan 返回的同时立刻触发动作并清空计划。这时再按
      // 「刚保存的值」显示，状态栏就会挂着一个永远不会执行的标记。
      const now = await invoke("get_plan");
      renderPlanChip(now);
      $("planOverlay").classList.add("hidden");
      toast(now ? `已设定：全部完成后${planLabel(now)}` : "已取消完成后动作");
    } catch (e) {
      toast("保存失败：" + String(e), true);
    }
  };

  // ---------- 菜单栏 ----------
  //
  // 交互按 Windows 习惯来：点标题展开、再点收起；展开状态下把鼠标移到
  // 另一个标题会直接切过去（IDM / 记事本都是这样），不用再点一次。

  const menuRoot = $("menubar");

  function closeMenus() {
    document.querySelectorAll(".menu.open").forEach((m) => m.classList.remove("open"));
  }

  function openMenu(menu) {
    closeMenus();
    menu.classList.add("open");
  }

  if (menuRoot) {
    const menus = [...menuRoot.querySelectorAll(".menu")];

    menus.forEach((menu) => {
      const btn = menu.querySelector(".menu-btn");

      btn.addEventListener("click", (e) => {
        // 阻止冒泡到 document 的「点别处关闭」，否则刚开就被关掉
        e.stopPropagation();
        if (menu.classList.contains("open")) closeMenus();
        else openMenu(menu);
      });

      // 已有菜单展开时，鼠标移过来就切过去
      menu.addEventListener("mouseenter", () => {
        if (menuRoot.querySelector(".menu.open")) openMenu(menu);
      });
    });

    // 点页面其他地方收起
    document.addEventListener("click", () => closeMenus());

    // 点了菜单项之后收起。用事件委托：菜单项是静态 HTML，
    // 但新增项时忘了单独绑事件是最常见的疏漏。
    menuRoot.addEventListener("click", (e) => {
      const item = e.target.closest(".menu-item");
      if (!item) return;
      closeMenus();
      runMenuCommand(item.dataset.cmd);
    });
  }

  // 菜单命令统一入口。每个分支都自己 catch：某个动作失败不该
  // 让后面的命令也跑不了。
  async function runMenuCommand(cmd) {
    switch (cmd) {
      case "addDownload": {
        // 「添加新下载」= 打开添加 URL 弹窗，并展开手动新建面板。
        // 不直接弹另存为：用户还没说地址，弹窗无从填起。
        openAddUrl();
        const nd = $("newDownload");
        if (nd) nd.open = true;
        setTimeout(() => $("ndUrl").focus(), 40);
        break;
      }

      case "startAll": {
        try {
          const n = await invoke("start_all");
          toast(n > 0 ? `已开始 ${n} 个任务` : "没有可开始的任务");
          refreshTasks();
        } catch (e) {
          toast(String(e), true);
        }
        break;
      }

      case "pauseAll": {
        try {
          const n = await invoke("pause_all");
          toast(n > 0 ? `已暂停 ${n} 个任务` : "没有正在进行的任务");
          refreshTasks();
        } catch (e) {
          toast(String(e), true);
        }
        break;
      }

      case "openDownloadDir": {
        try {
          await invoke("open_download_dir");
        } catch (e) {
          toast(String(e), true);
        }
        break;
      }

      // 工具栏「打开」：打开选中任务的文件。和「打开文件夹」不同，
      // 它是对选中项的操作，所以选中为空时要明确说清楚而不是静默什么都不做。
      case "openSelected": {
        const ids = [...selectedIds];
        if (ids.length === 0) {
          toast("请先在列表里勾选任务", true);
          break;
        }
        const targets = ids
          .map((id) => taskMap.get(id))
          .filter((t) => t && t.state === "done" && t.output_path);
        if (targets.length === 0) {
          toast("选中的任务还没有可打开的文件", true);
          break;
        }
        // 「打开」= 调系统关联程序：exe 直接运行、zip 交给解压软件、视频交给
        // 播放器。之前误用了 reveal（只定位到文件夹），用户点「打开」却什么
        // 都不发生——定位该交给「打开文件夹」按钮。逐个容错：一个打不开
        // （被占用/被删）不应中断其余文件。
        let opened = 0;
        for (const t of targets) {
          try {
            await invoke("open_task_file", { path: t.output_path });
            opened++;
          } catch (e) {
            toast(`打开「${t.title || t.output_path}」失败：${String(e)}`, true);
          }
        }
        if (opened) toast(`已打开 ${opened} 个文件`);
        break;
      }

      case "addFromClipboard": {
        try {
          const url = await invoke("clipboard_url");
          if (!url) {
            toast("剪贴板里没有链接", true);
            break;
          }
          // 有链接就直接进「另存为」。这正是 IDM 的行为：
          // 复制链接 → 菜单一键 → 选目录 → 开始，不用再回首页粘贴。
          saveAsQueue.push({ url });
          pumpSaveAsQueue();
        } catch (e) {
          toast(String(e), true);
        }
        break;
      }

      case "resumeSelected":
      case "redoSelected":
      case "pauseSelected":
      case "deleteSelected": {
        const ids = [...selectedIds];
        if (ids.length === 0) {
          toast("请先在任务列表里勾选任务", true);
          break;
        }
        // 弹窗要按状态分流（有几个有成品文件），所以要拿任务快照。
        // 拿不到（任务刚好被清掉）就按"没有成品文件"处理，走保守分支。
        const picked = ids.map((id) => taskMap.get(id)).filter(Boolean);
        try {
          // 批量操作的后端内核都是「逐条判断、不该动的跳过」，提示必须带上
          // 跳过数：选了 5 个只动了 2 个而不说原因，看起来就像功能坏了。
          const report = (verb, n, why) => {
            const skipped = ids.length - n;
            toast(skipped > 0 ? `${verb} ${n} 个任务（跳过 ${skipped} 个：${why}）` : `${verb} ${n} 个任务`);
          };
          if (cmd === "resumeSelected") {
            const n = await invoke("resume_tasks", { args: { ids } });
            report("已恢复", n, "正在下载、已结束或正在收尾");
          } else if (cmd === "redoSelected") {
            const n = await invoke("redo_tasks", { args: { ids } });
            report("已重新下载", n, "正在下载或正在收尾");
          } else if (cmd === "pauseSelected") {
            const n = await invoke("pause_tasks", { args: { ids } });
            report("已暂停", n, "已结束或正在收尾");
          } else {
            // 删除是不可撤销的：正在下的会被中止，临时文件会被清掉。
            // 不确认的话，误点一次 Del 就白下几十分钟。
            //
            // 三条路给用户自己选，而不是替他决定删不删文件：
            //   · 只删任务记录 —— 磁盘上的成品保留（原来的唯一行为）
            //   · 连文件一起删 —— 成品 + 任务都没了，用户要的"真删除"
            //   · 还有什么都不做
            // 用三按钮 confirm 而不是先删任务再追问：顺序一反过来，
            // 删文件失败时任务已经没了，用户连重试的抓手都没有。
            const doneCount = picked.filter((t) => t.state === "done" && t.output_path).length;
            const choice = await askDeleteMode(ids.length, doneCount);
            if (choice === "cancel") break;

            const withFiles = choice === "withFiles";
            const r = await invoke("remove_tasks", {
              args: withFiles ? { ids, delete_files: true } : { ids },
            });

            if (withFiles && r.errors && r.errors.length) {
              // 后端「文件没删成就不动任务记录」：失败的仍在列表里，
              // 所以这里不清选中，用户可直接再点一次。
              toast(
                `${r.errors.length} 个文件未能删除，任务已保留，可稍后重试：\n${r.errors.join("\n")}`,
                true
              );
            } else if (withFiles) {
              toast(`已删除 ${r.removed} 个任务及其文件（共 ${r.files_deleted} 个文件）`);
              ids.forEach((id) => selectedIds.delete(id));
            } else {
              toast(`已删除 ${r.removed} 个任务（文件保留在硬盘上）`);
              ids.forEach((id) => selectedIds.delete(id));
            }
          }
          refreshTasks();
        } catch (e) {
          toast(String(e), true);
        }
        break;
      }

      case "clearAll": {
        const total = taskMap.size;
        if (total === 0) {
          toast("任务列表已经是空的");
          break;
        }
        // 清空是不可撤销的：它会删掉所有临时文件，正在下载的也会被中止。
        // 这里必须确认一次，不能像「清除已结束」那样点了就做。
        const ok = await askConfirm(
          `确定清空全部 ${total} 个任务吗？\n\n正在下载的任务会被中止，未完成的临时文件会被删除。`
        );
        if (!ok) break;
        try {
          const n = await invoke("clear_all_tasks");
          selectedIds.clear();
          toast(`已清空 ${n} 个任务`);
          refreshTasks();
        } catch (e) {
          toast(String(e), true);
        }
        break;
      }

      case "openSettings":
        switchTab("settings");
        break;

      case "planAfterAll":
        await openPlanDialog();
        break;

      case "refresh":
        refreshTasks();
        toast("已刷新");
        break;

      case "usage":
        showInfo(
          "使用说明",
          `<p><b>添加下载</b>：点工具栏「添加URL」（或 Ctrl+N），把链接粘进弹窗。
              视频页会解析出清晰度，普通直链直接多线程分块下载。</p>
           <p><b>筛选任务</b>：左侧「正在下载 / 已完成」按状态筛，
              「视频 / 音乐 / 程序 / 压缩包 / 文档」按文件类型筛。</p>
           <p><b>批量操作</b>：勾选行首复选框后，用工具栏或「下载」菜单里的
              恢复 / 暂停 / 删除；Ctrl+A 全选，Del 删除。</p>
           <p><b>浏览器集成</b>：菜单「帮助 → 启用浏览器扩展」会把扩展目录和
              edge://extensions 一起给你，加载一次后网页点下载就会自动转给 VeloxGrab。</p>
           <p><b>暂停与恢复</b>：暂停保留断点，点「继续」从原位置接着下；
              「取消」会放弃断点，之后用行内「重下」或菜单「重新下载选中」从头再下。</p>`
        );
        break;

      case "setupExt": {
        // 用户主动来问扩展怎么装，顺便把当时的状态说清楚
        let active = false;
        try {
          active = await invoke("extension_active");
        } catch (e) {
          active = false;
        }
        if (active) {
          showInfo(
            "浏览器扩展已在运行",
            `<p>扩展已经加载并在工作：网页点下载会自动转给 VeloxGrab。</p>
             <p class="dim">如果没生效，检查扩展弹窗里「接管浏览器下载」是否被关掉了。</p>`
          );
          break;
        }
        let browsers = [];
        try {
          browsers = await invoke("detect_browsers");
        } catch (e) {
          browsers = ["edge", "chrome"];
        }
        let dir = "";
        try {
          dir = await invoke("open_extension_dir");
        } catch (e) {
          dir = "";
        }
        showExtensionGuide(
          browsers.includes("edge") ? "edge" : browsers.includes("chrome") ? "chrome" : "edge",
          dir
        );
        break;
      }

      case "about": {
        let ver = "";
        try {
          ver = await invoke("app_version");
        } catch (e) {
          // 拿不到版本号不影响「关于」本身，留空即可
        }
        showInfo(
          "关于 VeloxGrab",
          `<p><b>VeloxGrab</b> ${ver ? "v" + ver : ""}</p>
           <p>基于 Rust + Tauri 的下载器，任务调度与断点续传自研，
              视频解析使用 yt-dlp，音视频合并使用 ffmpeg。</p>
           <p class="dim">支持 HTTP/HTTPS 直链多线程分块下载、视频站点自动解析、
              代理与限速、浏览器 veloxgrab:// 协议集成。</p>`
        );
        break;
      }

      case "checkUpdate": {
        // 手动检查：失败要出声（用户就是来问的），有更新弹对话框
        try {
          const info = await invoke("check_update");
          if (info.error) {
            toast(info.error, true);
          } else if (info.available) {
            showUpdateDialog(info);
          } else {
            toast(`已是最新版本（v${info.current}）`);
          }
        } catch (e) {
          toast("检查更新失败：" + String(e), true);
        }
        break;
      }

      default:
        break;
    }
  }

  // ---------- 应用内更新 ----------
  //
  // 两个入口共用一个对话框：启动后的静默检查（Rust 侧发 "update-available"
  // 事件）和菜单「检查更新…」。同一时刻只允许一个更新对话框——启动检查
  // 撞上用户手动检查时，后到的直接丢弃（前一个框里的信息并不过期）。
  let updateInfoShown = null;

  function fmtMB(bytes) {
    if (!bytes) return "";
    const mb = bytes / (1024 * 1024);
    return (mb >= 10 ? mb.toFixed(0) : mb.toFixed(1)) + " MB";
  }

  function showUpdateDialog(info) {
    if (!$("updateOverlay").classList.contains("hidden")) return; // 已开着
    updateInfoShown = info;
    $("upTitle").textContent = "发现新版本";
    $("upVer").textContent = `v${info.current} → v${info.latest}`;
    $("upVer").classList.remove("hidden");
    // release 说明是 GitHub 上的 markdown：不渲染、截前几行、textContent 注入。
    // 半可信来源当纯文本处理，不给它任何 HTML 执行面。
    // 顺手把 markdown 记号洗掉（## 标题、** 加粗、` 代码），界面上留裸符号很难看。
    const lines = String(info.notes || "")
      .split("\n")
      .map((l) => l.replace(/^#+\s*/, "").replace(/\*\*(.+?)\*\*/g, "$1").replace(/`/g, "").trim())
      .filter((l) => l)
      .slice(0, 6);
    $("upBody").innerHTML =
      "<p></p><p class=\"dim\" style=\"margin-top:10px\">更新会重启程序，正在下载的任务会被中断（断点保留，更新后可继续）。</p>";
    $("upBody").firstChild.textContent = lines.join(" ") || "查看更新内容见发布页。";
    $("upBarWrap").classList.add("hidden");
    $("upBar").style.width = "0";
    $("upPct").textContent = "";
    const btn = $("upInstall");
    btn.classList.remove("hidden");
    btn.disabled = false;
    delete btn.dataset.fallback;
    // 增量优先：补丁通常只有全量安装包的零头（只含 veloxgrab.exe 的差异）。
    // 后端对补丁不降级——摘要缺失时 info 里根本不会有 patch_url，这里
    // 看到的就是可信可用的。
    const patch = info.patch_url && info.patch_digest && info.patch_target_digest;
    btn.querySelector("span").textContent = patch
      ? `增量更新（${fmtMB(info.patch_size)}）`
      : `下载并安装（${fmtMB(info.asset_size)}）`;
    const tip = $("upBody").querySelector("p.dim");
    if (tip && patch) tip.textContent += " 本次走增量补丁：体积更小，下载后自动校验再换文件；失败会自动回退全量安装。";
    $("updateOverlay").classList.remove("hidden");
  }

  async function startUpdate() {
    const info = updateInfoShown;
    if (!info || !info.download_url) return;
    const btn = $("upInstall");
    btn.disabled = true;
    btn.querySelector("span").textContent = "下载中…";
    $("upBarWrap").classList.remove("hidden");
    $("upPct").textContent = "准备下载…";
    // 「改用全量」回退态：补丁失败一次后，下一次点击走 update_now 全量
    const patch = !btn.dataset.fallback && info.patch_url && info.patch_digest && info.patch_target_digest;
    try {
      // 成功后 Rust 侧会直接退出本进程去安装，这里不会有返回值可言。
      // digest 是 check_update 里 release JSON 带出来的发布摘要：后端拿它做
      // 逐字节校验，避免为取摘要再打一次限流的 GitHub API。
      if (patch) {
        await invoke("update_now_patch", {
          patchUrl: info.patch_url,
          patchDigest: info.patch_digest,
          targetDigest: info.patch_target_digest,
        });
      } else {
        await invoke("update_now", { url: info.download_url, digest: info.digest || null });
      }
    } catch (e) {
      $("upPct").textContent = "";
      btn.disabled = false;
      if (patch) {
        // 补丁路径的失败（网络/校验/应用）不终结更新：回退全量安装包，
        // 按钮变成「改用全量」再点一次就是 update_now。
        btn.querySelector("span").textContent = `改用全量安装（${fmtMB(info.asset_size)}）`;
        btn.dataset.fallback = "1";
      } else {
        btn.querySelector("span").textContent = "重试";
      }
      toast("更新失败：" + String(e), true);
      return;
    }
  }

  function hideUpdateDialog() {
    // 「以后再说」：本次运行内不再自动弹（启动检查只在开机时发一次事件，
    // 这里只是把语义写死：关掉 = 这轮不追）。手动「检查更新」不受影响。
    $("updateOverlay").classList.add("hidden");
  }

  $("upInstall").onclick = startUpdate;
  $("upLater").onclick = hideUpdateDialog;
  $("updateOverlay").addEventListener("click", (e) => {
    if (e.target === $("updateOverlay")) hideUpdateDialog();
  });

  if (listen) {
    listen("update-progress", (evt) => {
      const p = evt.payload;
      const bar = $("upBar");
      if (p.total > 0) {
        const pct = Math.min(100, (p.downloaded / p.total) * 100);
        bar.style.width = pct.toFixed(1) + "%";
        $("upPct").textContent =
          pct >= 99.5
            ? "下载完成，正在安装——程序会自动重启…"
            : `${pct.toFixed(0)}%  ·  ${fmtMB(p.downloaded)} / ${fmtMB(p.total)}`;
      } else {
        $("upPct").textContent = `已下载 ${fmtMB(p.downloaded)}…`;
      }
    });
    // 启动后的静默检查发现新版本：直接弹框。不打扰式轮询——一次启动只弹一次，
    // 用户点了「以后再说」这次运行就不再自动弹（手动「检查更新」不受此限）。
    listen("update-available", (evt) => {
      showUpdateDialog(evt.payload);
    });
    // 更新包摘要没拿到（GitHub API 限流等）、但长度核对通过了：后端会降级
    // 放行安装。这是可执行文件，降级必须可见——用户可以取消重试或手动下载。
    listen("update-unverified", () => {
      toast("本次更新未能做摘要校验（发布摘要拉取失败），仅核对了文件长度", true);
    });
    // 本地桥端口被占/监听失败：浏览器扩展的链接转发整条不可用。旧实现只在
    // stderr 里打一行——GUI 进程里没人看得见，用户只知道「扩展没反应」。
    listen("bridge-offline", (evt) => {
      const port = evt && evt.payload;
      toast(`浏览器扩展桥启动失败（端口 ${port || 58943} 被占用），扩展暂时无法连接 VeloxGrab`, true);
    });
    // 桥检测到浏览器里挂的还是旧版扩展（ping 版本 < 桌面端）：弹可操作的提醒。
    // 旧扩展在 VeloxGrab 未运行时会跳系统协议确认页（开新标签），必须刷新
    // 扩展才能吃到 1.4.0 起的修复——这条提醒就是那次「点了没反应」的根因。
    listen("extension-outdated", (evt) => {
      const p = (evt && evt.payload) || {};
      toast(
        "浏览器扩展是旧版本（" +
          (p.ext || "未知") +
          " < " +
          (p.app || "?") +
          "）：到 chrome://extensions 刷新 VeloxGrab 扩展",
        true
      );
      // 普通 toast 3.2 秒就消失，这条是「要用户动手」的——重新挂一个长驻版
      clearTimeout(toastTimer);
      toastTimer = setTimeout(() => $("toast").classList.add("hidden"), 15000);
    });
    // history.json 损坏被重置：旧实现只有 stderr 一行，用户只会发现记录消失
    listen("history-reset", () => {
      toast("下载记录文件损坏，已自动备份并重置（原文件保留为 history.json.bad-*）", true);
    });

    // 托盘菜单的常用动作：Rust 侧只负责把窗口拉到前台 + 发命令名，
    // 具体执行与提示全部复用 runMenuCommand —— 托盘、菜单栏、快捷键
    // 三条入口必须走同一个执行器，否则迟早出现三份行为不一致。
    listen("tray-command", (evt) => {
      const cmd = evt && evt.payload;
      if (typeof cmd === "string" && cmd) runMenuCommand(cmd);
    });

    // 剪贴板监听：复制到链接时给一条可点的提示，点「添加」才进另存为流程。
    // 不自动入队是刻意的——剪贴板任何程序都能写，自动下载等于开了一条
    // 「往剪贴板塞链接就能让本程序下载任意文件」的通路。
    listen("clipboard-url", (evt) => {
      const url = evt && evt.payload && evt.payload.url;
      if (!url) return;
      // 用户已经在添加/另存为流程里时不插一条 toast：提示会和弹窗抢注意力，
      // 而他想做的事本来就已经在进行中。
      if (!$("addOverlay").classList.contains("hidden")) return;
      if (!$("saveAsOverlay").classList.contains("hidden")) return;
      // 输入框里已经有同一个链接时不重复提示：用户可能正在粘贴它。
      if (document.activeElement && document.activeElement.value === url) return;
      toastAction("剪贴板中有下载链接", "添加", () => {
        saveAsQueue.push({ url });
        pumpSaveAsQueue();
      });
    });

    // 设置里选了「关闭时询问」：Rust 侧拦下关闭，这里弹三选一。
    listen("close-requested", () => {
      $("closeOverlay").classList.remove("hidden");
    });

    // 全部完成后动作已触发。
    //
    // 关机/重启给了 60 秒倒计时，所以提示要活到倒计时结束，并且带「取消」；
    // 睡眠没有取消一说（机器已经睡了，这条提示其实是睡醒后才看到的）。
    listen("plan-fired", (evt) => {
      const p = (evt && evt.payload) || {};
      const label = planLabel(p.action);
      renderPlanChip("none");
      if (p.action === "shutdown" || p.action === "restart") {
        const secs = p.delay || 60;
        showCountdown(`全部下载完成，${label}将在 ${secs} 秒后执行`, () => {
          invoke("cancel_shutdown")
            .then(() => toast(`已取消${label}`))
            .catch((e) => toast(String(e), true));
        });
      } else if (p.action === "sleep") {
        toast("全部下载完成，即将进入睡眠");
      } else {
        toast(`全部下载完成，已${label}`);
      }
    });

    listen("plan-failed", (evt) => {
      const p = (evt && evt.payload) || {};
      renderPlanChip("none");
      toast(`${planLabel(p.action) || "完成后动作"}失败：${p.error || "未知错误"}`, true);
    });
  }

  function showInfo(title, html) {
    $("infoTitle").textContent = title;
    $("infoBody").innerHTML = html;
    $("infoOverlay").classList.remove("hidden");
  }

  /**
   * 删除方式确认：三选一，返回 "withFiles" | "tasksOnly" | "cancel"。
   *
   * 为什么不用原生 confirm：它只有两态，而「删任务」和「删文件」的后果
   * 差一个数量级（前者可重下，后者不可恢复）。用二选一逼用户猜，就只能
   * 二选一：要么永远不删文件（就是原来的问题），要么把删文件当默认
   * （误删风险）。这里把三个选项都摆出来，默认焦点给最保守的那个。
   *
   * `doneCount` 决定弹窗文案：选取里没有成品文件时，「连文件一起删」
   * 这个按钮要置灰，否则点了什么都不会发生，看起来像坏了。
   */
  function askDeleteMode(total, doneCount) {
    const haveFiles = doneCount > 0;
    $("delTitle").textContent = `删除 ${total} 个任务`;
    $("delBody").textContent =
      `选中的 ${total} 个任务会被移除。` +
      (haveFiles
        ? `\n其中 ${doneCount} 个任务有已下载完成的文件。`
        : `\n其中没有已下载完成的文件（未完成的临时文件总会被清理）。`);

    const warn = $("delWarn");
    if (haveFiles) {
      warn.textContent = "「连文件一起删」会把成品文件从硬盘永久删除，无法恢复。";
      warn.classList.remove("hidden");
    } else {
      warn.classList.add("hidden");
    }
    $("delWithFiles").disabled = !haveFiles;

    $("delOverlay").classList.remove("hidden");

    return new Promise((resolve) => {
      const cleanup = () => {
        $("delOverlay").classList.add("hidden");
        $("delCancel").onclick = null;
        $("delTasksOnly").onclick = null;
        $("delWithFiles").onclick = null;
        $("delOverlay").onclick = null;
      };
      const done = (v) => {
        cleanup();
        resolve(v);
      };
      $("delCancel").onclick = () => done("cancel");
      $("delTasksOnly").onclick = () => done("tasksOnly");
      $("delWithFiles").onclick = () => done("withFiles");
      // 点遮罩 = 取消（与其它弹窗一致）
      $("delOverlay").onclick = (e) => {
        if (e.target === $("delOverlay")) done("cancel");
      };
      // 默认焦点给「只删任务」：回车不该落到不可恢复的那个按钮上
      setTimeout(() => $("delTasksOnly").focus(), 30);
    });
  }

  /**
   * 带一个主动按钮的 info 弹窗。primary = { text, onClick }，传 null 就退回单按钮。
   * 「关闭」按钮的 hidden 由 primary 是否存在决定：给主动按钮时「关闭」藏起来，
   * 避免两个平级按钮让用户猜区别。
   */
  function showInfoAction(title, html, primary) {
    $("infoTitle").textContent = title;
    $("infoBody").innerHTML = html;
    const btn = $("infoPrimary");
    const close = $("infoClose");
    if (primary) {
      $("infoPrimaryText").textContent = primary.text;
      btn.classList.remove("hidden");
      close.classList.add("hidden");
      btn.onclick = () => {
        $("infoOverlay").classList.add("hidden");
        close.classList.remove("hidden");
        btn.classList.add("hidden");
        primary.onClick();
      };
    } else {
      btn.classList.add("hidden");
      close.classList.remove("hidden");
    }
    $("infoOverlay").classList.remove("hidden");
  }

  $("infoClose").onclick = () => $("infoOverlay").classList.add("hidden");
  $("infoOverlay").addEventListener("click", (e) => {
    // 点遮罩关闭，点对话框内部不关
    if (e.target === $("infoOverlay")) $("infoOverlay").classList.add("hidden");
  });
  $("planOverlay").addEventListener("click", (e) => {
    if (e.target === $("planOverlay")) $("planOverlay").classList.add("hidden");
  });

  // 当前视图下的任务 id（不是 taskMap 全量）。全选、批量删除、跨视图统计都以
  // 这个为准——在「已完成」里 Ctrl+A 结果把「正在下载」的也勾上，再一按 Del
  // 就是误删看不见的任务。taskMap 是唯一真相，可见集每次从它按 matchView 现算，
  // 不用另维护一份会漏更新的镜像。
  function visibleIds() {
    const ids = [];
    taskMap.forEach((t, id) => {
      if (matchView(t)) ids.push(id);
    });
    return ids;
  }

  // 全选：就地切换行选中态，不走 refreshTasks()——全量 IPC + 整表重建
  // 只为更新勾选框，几千任务时每次 Ctrl+A 都卡一下。
  $("selectAll").addEventListener("change", (e) => {
    const on = e.target.checked;
    if (on) visibleIds().forEach((id) => selectedIds.add(id));
    else selectedIds.clear();
    document.querySelectorAll(".task-row").forEach((row) => {
      const id = row.dataset.id;
      const selected = selectedIds.has(id);
      row.classList.toggle("selected", selected);
      const box = row.querySelector("input[type=checkbox]");
      if (box) box.checked = selected;
    });
    updateSelHint();
  });

  // 快捷键。全部走菜单命令，保证键盘与菜单是同一条代码路径。
  document.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    const tag = (e.target.tagName || "").toLowerCase();
    const typing = tag === "input" || tag === "textarea" || tag === "select";

    if (mod && !e.shiftKey && e.key.toLowerCase() === "n") {
      e.preventDefault();
      runMenuCommand("addDownload");
      return;
    }
    if (mod && e.key === ",") {
      e.preventDefault();
      runMenuCommand("openSettings");
      return;
    }
    if (e.key === "F5") {
      e.preventDefault();
      runMenuCommand("refresh");
      return;
    }
    // Ctrl+A 在输入框里是全选文字，不能抢
    if (mod && e.key.toLowerCase() === "a" && !typing) {
      e.preventDefault();
      $("selectAll").checked = true;
      // 与「全选」勾选框同一条路径：只选当前视图可见的任务
      visibleIds().forEach((id) => selectedIds.add(id));
      refreshTasks();
      return;
    }
    if (e.key === "Delete" && !typing) {
      if (selectedIds.size > 0) {
        e.preventDefault();
        runMenuCommand("deleteSelected");
      }
      return;
    }
    // Esc：先关弹窗，再收菜单
    if (e.key === "Escape") {
      closeMenus();
      $("infoOverlay").classList.add("hidden");
      closeAddUrl();
      $("planOverlay").classList.add("hidden");
      if (!$("updateOverlay").classList.contains("hidden")) hideUpdateDialog();
      if (!$("saveAsOverlay").classList.contains("hidden")) closeSaveAs();
      closeEditDialog();
      // 删除确认弹窗此前不在 Esc 处理内：它的 Promise 只能靠点击解除，
      // 键盘用户会被困住。
      if (!$("delOverlay").classList.contains("hidden")) {
        $("delOverlay").classList.add("hidden");
        $("delOverlay").onclick = null;
      }
    }
  });

  // ---------- 侧边栏 / 工具栏 ----------
  // 侧边栏项自己带 data-view 或 data-cat，两者都是「切视图」，
  // 只是筛选条件不同，所以共用一个处理函数。
  document.querySelectorAll(".sidebar .sb-item").forEach((btn) => {
    btn.addEventListener("click", () => showView(btn.dataset.view || btn.dataset.cat));
  });

  // 工具栏按钮和菜单项走同一个命令入口：保证两处的行为、报错文案、
  // 二次确认逻辑完全一致，不会出现「菜单里会问、工具栏里不问」。
  $("toolbar").addEventListener("click", (e) => {
    const btn = e.target.closest(".tb");
    if (!btn) return;
    runMenuCommand(btn.dataset.cmd);
  });

  // 状态栏「清除已结束」
  $("clearFinished").onclick = async () => {
    try {
      const n = await invoke("clear_finished");
      toast(n > 0 ? `已清除 ${n} 个已结束任务` : "没有已结束的任务");
      refreshTasks();
    } catch (e) {
      toast(String(e), true);
    }
  };

  // ---------- 事件绑定 ----------
  // ---------- 自绘标题栏的窗口控制（decorations:false） ----------
  function applyMaxIcon(max) {
    document.body.classList.toggle("is-max", !!max);
  }
  $("winMin").onclick = () => invoke("window_control", { action: "minimize" }).catch(() => {});
  $("winMax").onclick = () =>
    invoke("window_control", { action: "maximize_toggle" })
      .then(applyMaxIcon)
      .catch(() => {});
  // close 走后端窗口 close()：CloseRequested 处理器里的 close_mode
  // （询问/最小化到托盘/退出）必须继续生效，这里绝不直接调 quit。
  $("winClose").onclick = () => invoke("window_control", { action: "close" }).catch(() => {});
  // 拖拽区双击最大化、Win+方向键贴靠、任务栏还原都会改最大化状态，
  // 用 resize 事件兜底刷图标（高频事件，150ms 节流）。
  let maxIconTimer = null;
  if (listen) {
    listen("tauri://resize", () => {
      if (maxIconTimer) return;
      maxIconTimer = setTimeout(() => {
        maxIconTimer = null;
        invoke("window_control", { action: "query" })
          .then(applyMaxIcon)
          .catch(() => {});
      }, 150);
    });
  }
  invoke("window_control", { action: "query" }).then(applyMaxIcon).catch(() => {});

  $("parseBtn").onclick = doParse;
  $("urlInput").addEventListener("keydown", (e) => {
    // Enter 走与按钮同一个 disabled 守卫：IPC 往返窗口内连按 Enter
    // 不会再触发第二轮解析。
    if (e.key === "Enter" && !$("parseBtn").disabled) doParse();
  });
  $("downloadBtn").onclick = doDownload;
  $("dDownloadBtn").onclick = doDirectDownload;
  $("plDownloadBtn").onclick = doPlaylistDownload;
  $("plSelectAll").addEventListener("change", (e) => {
    const on = e.target.checked;
    $("plList")
      .querySelectorAll('input[type="checkbox"]')
      .forEach((b) => (b.checked = on));
    updatePlSelected();
  });
  $("plPickDir").onclick = () => {
    invoke("plugin:dialog|open", {
      options: { directory: true, multiple: false, title: "选择保存目录" },
    })
      .then((v) => { if (v) $("plDir").value = v; })
      .catch((e) => toast("选择目录失败：" + String(e), true));
  };
  $("ndStart").onclick = doNewDownload;
  $("ndPickDir").onclick = pickNewDir;
  $("addClose").onclick = closeAddUrl;
  $("addOverlay").addEventListener("click", (e) => {
    // 点遮罩关闭，点对话框内部不关
    if (e.target === $("addOverlay")) closeAddUrl();
  });

  // 编辑任务弹窗：选择目录复用同一个原生对话框插件；保存目录的信任判定
  // 在后端 admit_save_dir，前端只负责把路径带过去。
  $("editPickDir").onclick = () => {
    invoke("plugin:dialog|open", {
      options: { directory: true, multiple: false, title: "选择保存目录" },
    })
      .then((v) => { if (v) $("editDir").value = v; })
      .catch((e) => toast("选择目录失败：" + String(e), true));
  };
  $("editCancel").onclick = closeEditDialog;
  $("editSave").onclick = saveEditDialog;
  $("editOverlay").addEventListener("click", (e) => {
    if (e.target === $("editOverlay")) closeEditDialog();
  });
  // 文件名输入框回车 = 保存（和「添加下载」弹窗一致）
  $("editName").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      saveEditDialog();
    }
  });
  $("saveSettings").onclick = saveSettings;
  $("restoreDefaults").onclick = restoreDefaults;
  $("cancelSettings").onclick = () => {
    // 丢弃表单改动（重读磁盘值）并回主界面
    loadSettings();
    showView("all");
  };
  $("setProxyEnabled").addEventListener("change", syncProxyDetail);
  $("setAria2").addEventListener("change", syncAria2Detail);
  $("setCategoriesEnabled").addEventListener("change", syncCategoryDetail);
  $("addCategoryRule").onclick = () => addCategoryRow(null);
  $("addHeaderPreset").onclick = () => addHeaderPresetRow(null);
  $("setTheme").addEventListener("change", (e) => changeTheme(e.target.value));

  // 手动新建面板里按回车 = 开始下载
  ["ndUrl", "ndFilename"].forEach((id) => {
    $(id).addEventListener("keydown", (e) => {
      if (e.key === "Enter") doNewDownload();
    });
  });

  // 用户在手动新建区动手（展开或输入地址）= 明确选择直链路径，
  // 把之前的解析结果清掉，保证「开始下载」永远只有一个能点。
  $("newDownload").addEventListener("toggle", (e) => {
    if (e.target.open) preferManualEntry();
  });
  $("ndUrl").addEventListener("input", () => {
    if ($("ndUrl").value.trim()) preferManualEntry();
  });

  $("setConcurrent").addEventListener("input", (e) => {
    $("setConcurrentVal").textContent = e.target.value;
    syncRangeFill(e.target);
  });
  $("setFragments").addEventListener("input", (e) => {
    $("setFragmentsVal").textContent = e.target.value;
    syncRangeFill(e.target);
  });
  $("setDefaultThreads").addEventListener("input", (e) => {
    $("setDefaultThreadsVal").textContent = e.target.value;
    syncRangeFill(e.target);
  });

  $("pickDir").onclick = async () => {
    try {
      const picked = await invoke("plugin:dialog|open", {
        options: { directory: true, multiple: false, title: "选择下载目录" },
      });
      if (picked) $("setDownloadDir").value = picked;
    } catch (e) {
      toast("选择目录失败：" + String(e), true);
    }
  };

  $("pickCookies").onclick = async () => {
    try {
      const picked = await invoke("plugin:dialog|open", {
        options: {
          multiple: false,
          title: "选择 cookies.txt",
          filters: [{ name: "Cookies", extensions: ["txt"] }],
        },
      });
      if (picked) $("setCookiesFile").value = picked;
    } catch (e) {
      toast("选择文件失败：" + String(e), true);
    }
  };

  // 打开扩展目录：安装目录下的 extension 文件夹（由构建脚本拷贝进去）
  $("openExtDir").onclick = async () => {
    try {
      const picked = await invoke("open_extension_dir");
      if (picked) {
        await invoke("plugin:opener|reveal_item_in_dir", { paths: [picked] });
      }
    } catch (e) {
      toast("打开扩展目录失败：" + String(e), true);
    }
  };

  // 监听进度事件，只重建受影响的那一张卡片
  //
  // 早期这里每来一个事件就调一次 refreshTasks()（全量 list_tasks + 清空重建整个
  // 列表），而上一行刚替换过的单张卡片又被全量重建覆盖——每帧渲染两遍。
  // yt-dlp 约 10Hz/任务，3 并发就是 30 次/秒的全量 IPC + DOM 重建。
  if (listen) {
    listen("task-progress", (evt) => {
      const p = evt.payload;
      // 全量快照在途时收到增量：这份快照（取数早于本事件）落地后会把本事件
      // 打回去。记 dirty 让 refreshTasks 落地后补刷一轮——否则暂停/终态这类
      // 不再发后续事件的跃迁会被旧快照永久回退（见 refreshTasks 的竞态注释）。
      if (refreshInFlight) refreshDirty = true;
      const prev = taskMap.get(p.id) || {};

      // 事件里的 null 表示「本轮没有这一项」，不能覆盖已有值：
      // 否则下载中会丢掉 total_bytes，完成后会丢掉 output_path
      const patch = {};
      for (const [k, v] of Object.entries(p)) {
        if (v !== null && v !== undefined) patch[k] = v;
      }
      const merged = { ...prev, ...patch };
      const stateChanged = prev.state !== merged.state;
      taskMap.set(p.id, merged);

      // 用 .task-row 而不是 .task：历史页条目也带 .task，两者会同时命中
      const card = document.querySelector(`.task-row[data-id="${CSS.escape(p.id)}"]`);
      if (card) {
        if (stateChanged) {
          // 状态变了先过一遍视图过滤：任务在「正在下载」视图里下完、或在
          // 「视频」分类里开始下 .zip 时，旧行必须离开当前视图。只在全量
          // 刷新跑 matchView 的话，行会「赖着不走」，与侧边栏计数互相矛盾。
          if (!matchView(merged)) {
            card.remove();
          } else {
            card.replaceWith(renderTask(merged));
          }
        } else {
          updateCardProgress(card, merged);
        }
      } else {
        // 本地还没有这一行（任务刚入队 / 刚进入当前筛选条件）；
        // 是否属于当前视图由全量刷新判定，这里不做本地 matchView——
        // 事件 patch 里没有 kind/url 等字段，判不准，宁可多刷一次。
        scheduleRefresh();
      }
      if (stateChanged) {
        updateSummary();
        // 状态变了可能进出当前视图（例如在「正在下载」里下完了），
        // 空态提示要跟着走，否则会一直显示「这个分类下还没有任务」。
        $("listEmpty").classList.toggle("hidden", $("taskList").children.length > 0);
      }
    });
  }

  // ---------- 外部链接（浏览器集成的落点） ----------
  // 用户在网页/邮件里点了 veloxgrab:// 链接时，Rust 侧（deeplink.rs）把解析结果
  // 放进待取队列，再发一个**空载荷**的唤醒事件。
  //
  // 为什么不直接用事件载荷：webview 还没执行 JS 时 emit 会被静默丢弃，
  // 冷启动点链接正好落在这个窗口里（实测会 100% 丢）。所以数据只走队列，
  // 事件只当「有新链接了，来取一趟」的门铃，重复触发也无副作用。
  let drainingLinks = false;

  async function drainExternalLinks() {
    // 门铃可能在极短时间内响多次；一次 drain 就把队列清空了，不必并发取。
    if (drainingLinks) return;
    drainingLinks = true;
    try {
      const list = await invoke("take_pending_deep_links");
      for (const p of list || []) {
        if (!p) continue;
        if (p.error) {
          toast("链接无法处理：" + p.error, true);
          continue;
        }
        await acceptExternalLink(p);
      }
    } catch (e) {
      console.error("取待处理链接失败", e);
    } finally {
      drainingLinks = false;
    }
  }

  if (listen) {
    listen("external-link", () => {
      drainExternalLinks();
    });
  }

  // 兜底轮询：emit 是「尽力送达」，主窗忙/未挂 listen 时事件会静默丢失，
  // 链接从此卡在 pending_links（实测：点扩展下载后主窗闪到前台却没有任何
  // 弹窗，正是事件丢失的形态）。3 秒一次的空队列 IPC 成本可忽略；
  // take_pending_deep_links 取走即清空，天然幂等。
  setInterval(() => {
    drainExternalLinks();
  }, 3000);

  async function acceptExternalLink(p) {
    if (!p || p.error) return;
    // launch 动作：扩展在桌面端未运行时发来的「只拉起」请求，不带任务。
    // 窗口已经被 dispatch_external_link 拉到前台了，这里只需提示。
    if (p.action === "launch") {
      toast("VeloxGrab 已启动");
      return;
    }
    if (!p.url) return;
    const s = appSettings || (await getSettingsCached());
    // auto=1：扩展显式投递（桥/NM 链接由 build_link_from_json 统一注入）。
    // 下载拦截路径送出后就**取消了浏览器自己的下载**，这里若再停在
    // 「另存为」面板等人确认，面板没被注意到的话这条下载就两边都消失了
    // ——能自动落的直接落，只有「非人不可」的信息（清晰度选择、探测
    // 失败的兜底确认）才交给面板。见 autoAcceptExternal。
    if (p.auto) {
      await autoAcceptExternal(p, s);
      return;
    }
    // 「先选目录再下载」开关：关掉时保持老行为，静默存入默认目录
    if (!s.popup_save_as) {
      await silentExternalDownload(p, s);
      return;
    }
    saveAsQueue.push(p);
    pumpSaveAsQueue();
  }

  // auto=1 的落地策略：先探测分流——
  // - 直链：静默入队（与 popup_save_as 关闭时同一条路径）；
  // - 视频：popup_quality 开着 → 面板选清晰度（探测结果预填，不重复探测）；
  //   关着 → 自动取推荐清晰度直接入队；
  // - 探测失败/异常：回退面板——「识别失败，可按直链尝试」的说明只有
  //   面板能承载，静默建任务会把失败原因吞掉。
  async function autoAcceptExternal(p, s) {
    let r = null;
    try {
      r = await invoke("probe_auto", { url: p.url });
    } catch (e) {
      r = { kind: "error", message: String(e) };
    }
    if (!r) r = { kind: "error", message: "probe_auto 返回空" };
    if (r.kind === "direct") {
      await silentExternalDownload(p, s);
      return;
    }
    if (r.kind === "video" && !s.popup_quality) {
      const media = r.media;
      const formats = media.formats || [];
      let idx = formats.findIndex((f) => f.recommended);
      if (idx < 0) idx = 0;
      const f = formats[idx];
      if (f) {
        try {
          await invoke("enqueue_download", {
            args: {
              url: media.webpage_url,
              title: p.filename || media.title,
              thumbnail: media.thumbnail || null,
              duration: media.duration || null,
              format_id: f.format_id,
              quality_label: f.label,
              has_audio: f.has_audio,
              best_audio_format: media.best_audio_format || null,
              kind: "video",
              save_dir: p.save_dir || s.download_dir || null,
              expected_ext: f.ext || null,
              expected_bytes: f.filesize || null,
              // 页面 Referer：防盗链站点的解析/下载都可能需要
              referer: p.referer || null,
            },
          });
          toast("已从浏览器接收下载：" + (media.title || p.url));
          refreshTasks();
          return;
        } catch (e) {
          toast(String(e), true);
          return;
        }
      }
    }
    // 视频且开着清晰度选择 / 探测不顺利：面板接手。探测结果随载荷带给
    // openSaveAs（_probed），面板不再重探一遍——媒体站解析最长 90 秒，
    // 重复跑一遍既慢又可能撞站点的频率限制。
    saveAsQueue.push({ ...p, _probed: r });
    pumpSaveAsQueue();
  }

  async function getSettingsCached() {
    if (!appSettings) {
      try {
        appSettings = await invoke("get_settings");
      } catch (e) {
        appSettings = { download_dir: "", default_threads: 8, popup_quality: true,
          auto_progress_popup: true, language: "zh" };
      }
    }
    return appSettings;
  }

  async function silentExternalDownload(p, s) {
    try {
      await invoke("new_download", {
        args: {
          url: p.url,
          filename: p.filename || null,
          threads: p.threads || null,
          save_dir: p.save_dir || s.download_dir || null,
          referer: p.referer || null,
        },
      });
      toast("已从浏览器接收下载：" + (p.filename || p.url));
      refreshTasks();
    } catch (e) {
      toast(String(e), true);
    }
  }

  // ---------- 另存为弹窗 ----------
  // 浏览器点进来的链接在这里等用户确认：目录 / 文件名 / 线程（直链）或清晰度（视频），
  // 点「开始下载」才入队，点「取消」直接丢弃。多条链接按队列逐个弹，不会叠窗。
  const saveAsQueue = [];
  let saveAsBusy = false;
  let saveAsCtx = null; // { payload, media?, direct? }

  function pumpSaveAsQueue() {
    if (saveAsBusy || !saveAsQueue.length) return;
    // 弹窗还开着就不动：队列等用户处理完当前这条（closeSaveAs 里续泵），
    // 否则探测一结束下一条链接就会把用户正在选的框覆盖掉。
    if (!$("saveAsOverlay").classList.contains("hidden")) return;
    saveAsBusy = true;
    openSaveAs(saveAsQueue.shift()).finally(() => {
      saveAsBusy = false;
      // openSaveAs 返回时弹窗通常仍开着（等用户点按钮）：那时不泵，
      // 关闭动作（closeSaveAs）负责续泵；错误路径自己就把框收了，这里接得上。
      if ($("saveAsOverlay").classList.contains("hidden")) pumpSaveAsQueue();
    });
  }

  async function openSaveAs(p) {
    const s = await getSettingsCached();
    saveAsCtx = { payload: p, media: null, direct: null };
    $("saUrl").textContent = p.url;
    $("saUrl").title = p.url;
    $("saKind").textContent = "";
    // chip 默认带 hidden（首次打开前没有类型可显示），识别出类型后要摘掉
    $("saKind").classList.add("hidden");
    $("saStatus").textContent = "识别中…";
    $("saBody").classList.add("hidden");
    $("saStart").disabled = true;
    $("saFilename").value = p.filename || "";
    $("saDir").value = p.save_dir || s.download_dir || "";
    fillThreadSelect($("saThreads"), p.threads || s.default_threads || 8);
    // 上一轮可能是视频（隐藏了线程行）或没识别出格式；每条都从「都隐藏」重来
    $("saThreadsRow").classList.add("hidden");
    $("saQualityRow").classList.add("hidden");
    $("saveAsOverlay").classList.remove("hidden");

    let r = p._probed || null;
    if (!r) {
      try {
        r = await invoke("probe_auto", { url: p.url });
      } catch (e) {
        if (!saveAsCtx) return; // 探测期间用户已点取消
        $("saStatus").textContent = "识别失败：" + String(e) + "（仍可按直链下载）";
      }
    }

    // 探测是异步的（媒体站最长 90 秒）：这期间用户可能已经点了「取消」，
    // saveAsCtx 被置空。此后再写 saveAsCtx.* 就是在 null 上取属性。
    if (!saveAsCtx) return;

    if (r && r.kind === "video") {
      saveAsCtx.media = r.media;
      $("saKind").textContent = "视频";
      $("saKind").classList.remove("hidden");
      $("saStatus").textContent = "";
      if (!$("saFilename").value) $("saFilename").value = r.media.title || "";
      // 视频走 yt-dlp 分片，没有「线程数」概念：隐藏该行，避免误导
      $("saThreadsRow").classList.add("hidden");
      fillSaQuality(r.media, s.popup_quality);
      $("saBody").classList.remove("hidden");
      $("saStart").disabled = false;
    } else if (r && r.kind === "direct") {
      saveAsCtx.direct = r.direct;
      $("saKind").textContent = "直链";
      $("saKind").classList.remove("hidden");
      $("saStatus").textContent = "";
      // 名字的优先级：后端从 Content-Disposition 解析出的 > 调用方给的 > URL 猜的。
      //
      // 不能无脑保留调用方的名字：扩展传过来的 `downloads.onCreated` 拿到的
      // `item.filename` 在下载刚开始时**往往还是从 URL 推出来的**（服务器头
      // 还没到），于是用户看到的是 `f.bin`，而服务器明明说了
      // `Content-Disposition: attachment; filename="probe.bin"`。
      // 判据是「这个名字是不是就是从 URL 末段来的」——是的话它没有权威性。
      const fromUrl = nameLooksLikeUrl($("saFilename").value, p.url);
      if (r.direct.filename && (!p.filename || fromUrl)) {
        $("saFilename").value = r.direct.filename;
      } else if (!p.filename) {
        $("saFilename").value = r.direct.filename || "";
      }
      if (!p.threads && r.direct.suggested_threads) {
        fillThreadSelect($("saThreads"), r.direct.suggested_threads);
      }
      // 直链才有多线程分块；不支持 Range 时锁死 1 线程
      $("saThreadsRow").classList.toggle("hidden", false);
      if (!r.direct.range_supported) {
        const sel = $("saThreads");
        sel.innerHTML = "";
        const o = el("option");
        o.value = "1";
        o.textContent = "1 线程";
        o.selected = true;
        sel.appendChild(o);
      }
      $("saDetail").textContent =
        (r.direct.total_bytes ? fmtBytes(r.direct.total_bytes) + "   ·   " : "") +
        describeRange(r.direct);
      $("saBody").classList.remove("hidden");
      $("saStart").disabled = false;
    } else if (r && r.kind === "error") {
      // 两边都识别不了：不悄悄收场（弹窗一闪而过是「程序坏了」的观感），
      // 保留弹窗让用户看错误原因，并仍可按直链下载——探测失败不代表
      // 下载失败（动态生成的链接、需要 Referer 的直链都会探不出来）。
      saveAsCtx.direct = { sourceUrl: p.url, fallback: true };
      $("saKind").textContent = "直链";
      $("saKind").classList.remove("hidden");
      $("saStatus").textContent = "识别失败，可按直链尝试下载：" + (r.message || "未知原因");
      $("saStatus").classList.add("err");
      $("saThreadsRow").classList.remove("hidden");
      $("saDetail").textContent = "服务器信息未知；若下载失败请检查链接是否需要登录";
      $("saBody").classList.remove("hidden");
      $("saStart").disabled = false;
    } else {
      // 探测抛异常：按直链兜底，让用户自己决定
      $("saThreadsRow").classList.remove("hidden");
      $("saBody").classList.remove("hidden");
      $("saStart").disabled = false;
    }
  }

  function fillSaQuality(media, showSelect) {
    const sel = $("saQuality");
    sel.innerHTML = "";
    (media.formats || []).forEach((f, i) => {
      const o = el("option");
      o.value = String(i);
      o.textContent = f.label + (f.filesize ? " — " + fmtBytes(f.filesize) : "");
      if (f.recommended) o.selected = true;
      sel.appendChild(o);
    });
    // 清晰度行只对视频有意义；关掉弹窗开关时自动取推荐项并隐藏该行
    const row = sel.closest(".fld");
    if (row) row.classList.toggle("hidden", !showSelect);
    updateSaFmtDetail(media);
    sel.onchange = () => updateSaFmtDetail(media);
  }

  function updateSaFmtDetail(media) {
    const idx = Number($("saQuality").value || 0);
    const f = (media.formats || [])[idx];
    if (!f) return;
    const parts = [`format_id: ${f.format_id}`];
    if (f.resolution) parts.push(`分辨率: ${f.resolution}`);
    if (f.fps) parts.push(`${Math.round(f.fps)}fps`);
    if (f.filesize) parts.push(`大小: ${fmtBytes(f.filesize)}`);
    $("saDetail").textContent = parts.join("   ·   ");
  }

  function closeSaveAs() {
    $("saveAsOverlay").classList.add("hidden");
    saveAsCtx = null;
    // 用户处理完这一条：队列里还有下一个就接着弹
    pumpSaveAsQueue();
  }

  $("saPickDir").onclick = () => {
    invoke("plugin:dialog|open", {
      options: { directory: true, multiple: false, title: "选择保存目录" },
    })
      .then((v) => { if (v) $("saDir").value = v; })
      .catch((e) => toast("选择目录失败：" + String(e), true));
  };
  $("saCancel").onclick = closeSaveAs;
  $("saStart").onclick = async () => {
    if (!saveAsCtx) return;
    const p = saveAsCtx.payload;
    const btn = $("saStart");
    btn.disabled = true;
    try {
      const dir = $("saDir").value.trim() || null;
      const name = $("saFilename").value.trim() || null;
      if (saveAsCtx.media) {
        const media = saveAsCtx.media;
        let idx;
        const qRowHidden = $("saQuality").closest(".fld")?.classList.contains("hidden");
        if (qRowHidden) {
          idx = (media.formats || []).findIndex((f) => f.recommended);
          if (idx < 0) idx = 0;
        } else {
          idx = Number($("saQuality").value || 0);
        }
        const f = (media.formats || [])[idx];
        if (!f) throw new Error("没有可用的清晰度");
        await invoke("enqueue_download", {
          args: {
            url: media.webpage_url,
            title: name || media.title,
            thumbnail: media.thumbnail || null,
            duration: media.duration || null,
            format_id: f.format_id,
            quality_label: f.label,
            has_audio: f.has_audio,
            best_audio_format: media.best_audio_format || null,
            kind: "video",
            save_dir: dir,
            expected_ext: f.ext || null,
            expected_bytes: f.filesize || null,
            // 页面 Referer：防盗链站点解析/下载都可能需要
            referer: p.referer || null,
          },
        });
      } else {
        await invoke("new_download", {
          args: {
            url: p.url,
            filename: name,
            threads: Number($("saThreads").value || 1),
            save_dir: dir,
            // 防盗链直链：扩展带来的页面 Referer 必须继续往下传
            referer: p.referer || null,
          },
        });
      }
      closeSaveAs();
      onDownloadAccepted();
    } catch (e) {
      toast(String(e), true);
      btn.disabled = false;
    }
  };

  // ---------- 启动 ----------
  /**
   * 首启引导：安装器装了扩展文件、但浏览器里还没加载时，指路一次。
   *
   * ## 判据为什么改了
   *
   * 原来只看 `extension_installed`（安装目录下有 `extension-installed.flag`），
   * 那个文件只证明**安装器把文件拷过去了**，完全不等于用户在浏览器里加载过它。
   * 「装完看到引导 → 关掉 → 再也没提示 → 浏览器里翻遍也没有扩展」就是这么来的。
   *
   * 现在真正的判据是 `extension_active`：桥有没有收到过带 `chrome-extension://`
   * Origin 的请求（扩展 SW 醒来会主动 ping 一次）。只有它返回 true 才算真的装好了。
   * 没装好就每次启动都提示一次——直到用户在浏览器里点「加载解压缩的扩展」。
   * 这不是骚扰，是唯一能让他知道问题在哪的办法。
   */
  async function maybeGuideExtension() {
    let installed = false;
    let active = false;
    try {
      installed = await invoke("extension_installed");
      active = await invoke("extension_active");
    } catch (e) {
      return;
    }
    // 没装扩展文件（dev 构建）或者扩展已经在跑 → 都不用管
    if (!installed || active) return;

    let browsers = [];
    try {
      browsers = await invoke("detect_browsers");
    } catch (e) {
      browsers = ["edge", "chrome"];
    }
    let dir = "";
    try {
      dir = await invoke("open_extension_dir");
    } catch (e) {
      dir = "";
    }

    const target = browsers.includes("edge")
      ? "edge"
      : browsers.includes("chrome")
        ? "chrome"
        : "edge";
    showExtensionGuide(target, dir);
  }

  /**
   * 弹出扩展加载引导。菜单「帮助 → 启用浏览器扩展」也走这里，方便用户
   * 事后重看（自动弹的那次很容易被顺手关掉）。
   */
  function showExtensionGuide(target, dir) {
    if (!dir) {
      dir = "";
    }
    const pageName = target === "edge" ? "edge://extensions" : "chrome://extensions";
    const dirLine = dir
      ? `<p style="margin:8px 0 0">扩展目录（可直接复制）：<code style="user-select:all;word-break:break-all">${escHtml(dir)}</code></p>`
      : "";
    const devMode = target === "edge" ? "左下角" : "右上角";

    showInfoAction(
      "启用浏览器扩展",
      `<p>扩展文件已经装好了，但浏览器不会自动加载它——Chrome 和 Edge 都不允许程序静默安装扩展，必须由你手动加一次。</p>
       <ol style="margin:8px 0 0;padding-left:20px;line-height:1.9">
         <li>点下面的按钮打开 <code style="user-select:all">${pageName}</code></li>
         <li>打开${devMode}的「开发人员模式」开关</li>
         <li>点「加载解压缩的扩展」，选中下面这个目录</li>
       </ol>
       ${dirLine}
       <p style="margin:8px 0 0" class="dim">加载成功后扩展列表里会出现「VeloxGrab Downloader」，工具栏上也有图标。之后网页点下载就会自动转给 VeloxGrab，不用再管。</p>`,
      {
        text: `打开 ${pageName}`,
        onClick: () => {
          invoke("open_extensions_page", { browser: target }).catch(() => {
            toast("打开扩展页失败，请手动在浏览器地址栏输入 " + pageName, true);
          });
          // 「加载解压缩的扩展」弹的是文件夹选择框，手打路径很烦。
          // 顺手把目录放进剪贴板，用户在那个框里直接 Ctrl+V 粘到地址栏即可。
          if (dir && navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard
              .writeText(dir)
              .then(() => toast("扩展目录已复制，粘贴到选择文件夹的地址栏即可"))
              .catch(() => {});
          }
        },
      }
    );
    // 「关闭」按钮恢复可点：showInfoAction 给了主动按钮后会把「关闭」藏起来，
    // 但这里的引导不点也无法继续，用户总得能关掉它。
    $("infoClose").classList.remove("hidden");
    $("infoPrimary").classList.remove("hidden");
  }

  (async function boot() {
    // 先拿设置：语言、线程下拉的默认值都要用它
    try {
      appSettings = await invoke("get_settings");
    } catch (e) {
      appSettings = null;
    }
    applyLanguage((appSettings && appSettings.language) || "zh");
    // 用 settings.json 里的真值纠正首帧的缓存值（详见 applyTheme 处的说明）。
    // 二者一致时这一步是空操作。
    applyTheme((appSettings && appSettings.theme) || "system");
    const defT = (appSettings && appSettings.default_threads) || 8;
    fillThreadSelect($("ndThreads"), defT);
    fillThreadSelect($("dThreads"), defT);

    // 初始视图由侧边栏的 active 决定（HTML 里是「所有下载」）
    showView("all");

    const h = await checkHealth();
    if (h && h.ytdlp_ok === false) {
      // 首次启动就把问题说清楚。提示条在「添加 URL」弹窗里，
      // 不自动弹出会把问题藏起来，所以先展开弹窗让用户看到。
      showError(
        "缺少 yt-dlp。\n把 yt-dlp.exe 放到本程序所在目录，或设置环境变量 VELOXGRAB_YTDLP 指向它，然后重启。"
      );
      openAddUrl();
    }
    // 冷启动是从链接进来的话，链接已经躺在队列里，这里补一次取用。
    drainExternalLinks();

    // 完成后动作只存在 Rust 内存里，页面重载（F5 / devtools）不会清掉它，
    // 所以这里补一次状态，免得状态栏 chip 和真实计划对不上。
    try {
      renderPlanChip(await invoke("get_plan"));
    } catch (e) {
      renderPlanChip("none");
    }

    // 安装器勾了「浏览器扩展」组件 → 引导用户去浏览器扩展页手动加载。
    // Chrome 不允许安装器静默注册扩展，这一步省不掉，只能把路铺到嘴边。
    await maybeGuideExtension();
  })();
})();
