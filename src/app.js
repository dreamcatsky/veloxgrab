// VGrab 前端逻辑
// 注意：这里不 import任何打包器，直接用全局 window.__TAURI__（Tauri 2 的 withGlobalTauri 行为）
(function () {
  "use strict";

  const invoke = window.__TAURI__?.core?.invoke;
  const listen = window.__TAURI__?.event?.listen;

  if (!invoke) {
    // 用 var(--err) 而不是写死的 #e5544b：那串红是按深色底调的，
    // 浅色主题下对比度不够，看不清。
    document.body.innerHTML =
      '<div style="padding:40px;font-family:sans-serif;color:var(--err)">' +
      "无法连接 Tauri 运行时。请通过 VGrab 桌面程序启动，而不是用浏览器打开本页面。</div>";
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

  function fmtDuration(s) {
    if (!s || s <= 0) return "";
    const total = Math.round(s);
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const sec = total % 60;
    const pad = (n) => String(n).padStart(2, "0");
    return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
  }

  // 取链接的 host（去掉 www. 前缀），失败返回空串。
  // 不用 new URL()：用户粘进来的可能是不带协议的裸串，
  // new URL 会抛异常，而这里只是想给列表补个来源提示，不值得 try/catch。
  function hostOf(url) {
    if (!url) return "";
    const rest = url.split("://").pop() || "";
    const host = rest.split(/[/?#]/)[0].split("@").pop() || "";
    return host.replace(/^www\./i, "").split(":")[0];
  }

  /**
   * 这个文件名是不是只是「URL 最后一段原样抄来的」。
   *
   * 扩展拦截下载时，`chrome.downloads.onCreated` 的 `item.filename` 在服务器
   * 响应头到达之前就已经填好了，内容就是从 URL 末段推的名字。拿它当真名会
   * 盖掉后端从 `Content-Disposition` 解出的正确名字
   * （`f.bin` vs 服务器声明的 `probe.bin`）。
   * 只做保守判断：去掉查询串后，URL 末段与文件名相同（忽略大小写）就算「抄来的」。
   */
  function nameLooksLikeUrl(name, url) {
    if (!name || !url) return false;
    const tail = (url.split("?")[0].split("#")[0].split("/").pop() || "").trim();
    if (!tail) return false;
    try {
      return decodeURIComponent(tail).toLowerCase() === String(name).toLowerCase();
    } catch (e) {
      // URL 里有非法转义序列（少见），退化成直接比较
      return tail.toLowerCase() === String(name).toLowerCase();
    }
  }

  // ---------- 主题 ----------
  //
  // 首帧的落定在 theme.js（<head> 里的同步脚本）做完了，这里只负责
  // 「拿到 Rust 侧真值之后再对齐一次」，以及用户改选时的即时切换。
  //
  // 分工的理由：get_settings 是一次 IPC，等它回来再上色，窗口会先闪一帧
  // 错误主题。所以先用 localStorage 的缓存值上色（可能旧一帧但绝不闪），
  // 等真值到了再纠正。二者一致时这个纠正完全看不出来。
  function applyTheme(theme) {
    if (!window.VGrabTheme) return;
    window.VGrabTheme.apply(theme);
  }

  let toastTimer = null;
  function toast(msg, isErr) {
    const t = $("toast");
    t.textContent = msg;
    t.className = "toast" + (isErr ? " err" : "");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => t.classList.add("hidden"), 3200);
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
    popupSaveAs: 'Browser vgrab:// downloads ask "Save as" first',
    popupSaveAsHint: "(off: save straight to the default folder)",
    popupQuality: "Video sites: popup quality chooser",
    popupQualityHint: "(off: auto best quality)",
    autoProgress: "Switch to task view when a download starts",
    mergeMp4: "Always merge output to MP4",
    autostart: "Start with Windows",
    autostartHint: "(writes HKCU Run key on save)",
    telemetry: "Anonymous usage stats",
    telemetryHint: "(no reporting exists yet; checkbox is a placeholder)",
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
    openExtDir: "Open extension folder",
    browserIntHint: "Load the vgrab-extension folder there into Chrome / Edge extensions",
    browserIntHint2: 'Then right-click a page → "Download with VGrab" sends the link to this app (vgrab:// protocol).',
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
    miPauseSel: "Pause selected",
    miDeleteSel: "Delete selected",
    miClearAll: "Clear all tasks",
    miOptions: "Options",
    miRefresh: "Refresh",
    miProgress: "Open progress window",
    miUsage: "How to use",
    miSetupExt: "Enable browser extension…",
    miAbout: "About VGrab",
    selectAll: "All",
    paused: "Paused",
  };
  let i18nOriginal = null;
  let i18nPhOriginal = null;

  function applyLanguage(lang) {
    const nodes = document.querySelectorAll("[data-i18n],[data-i18n-attr]");
    const phNodes = document.querySelectorAll("[data-i18n-ph]");
    if (!i18nOriginal) {
      i18nOriginal = new Map();
      nodes.forEach((n) => i18nOriginal.set(n, n.innerHTML));
      i18nPhOriginal = new Map();
      phNodes.forEach((n) => i18nPhOriginal.set(n, n.placeholder));
    }
    nodes.forEach((n) => {
      const key = n.dataset.i18n || n.dataset.i18nAttr;
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
    setTimeout(() => $("urlInput").focus(), 30);
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
    if (appSettings && appSettings.auto_progress_popup) showView("all");
    refreshTasks();
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
    const url = $("urlInput").value.trim();
    if (!url) {
      toast("请先粘贴链接", true);
      return;
    }

    showError("");
    $("mediaCard").classList.add("hidden");
    $("directCard").classList.add("hidden");
    currentMedia = null;
    currentDirect = null;

    const btn = $("parseBtn");
    btn.disabled = true;
    // 按钮里包着 data-i18n 的 span：只改 span，别把标记节点整个换掉
    const btnLabel = btn.querySelector("span");
    if (btnLabel) btnLabel.textContent = "识别中…";

    try {
      const r = await invoke("probe_auto", { url });

      if (r.kind === "video") {
        currentMedia = r.media;
        renderMedia(r.media);
      } else if (r.kind === "direct") {
        currentDirect = r.direct;
        currentDirect.sourceUrl = url;
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
  }

  async function doDirectDownload() {
    if (!currentDirect) return;
    const btn = $("dDownloadBtn");
    btn.disabled = true;
    try {
      const threads = Number($("dThreads").value || 1);
      const nameOverride = $("dFilenameInput").value.trim();
      await invoke("enqueue_download", {
        args: {
          url: currentDirect.sourceUrl,
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

    const btn = $("ndStart");
    btn.disabled = true;
    try {
      await invoke("new_download", {
        args: {
          url,
          filename: $("ndFilename").value.trim() || null,
          threads: Number($("ndThreads").value || 1),
          save_dir: $("ndDir").value.trim() || null,
        },
      });
      // 建完把地址清掉，方便连续粘下一个
      $("ndUrl").value = "";
      $("ndFilename").value = "";
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
      thumb.src = info.thumbnail;
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

  function toggleSelect(id, on) {
    if (on) selectedIds.add(id);
    else selectedIds.delete(id);
    updateSelHint();
  }

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
    // 让用户一眼看出「不是全选也不是没选」。
    const all = $("selectAll");
    if (all) {
      const total = taskMap.size;
      all.checked = n > 0 && n === total;
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

  // 当前视图是否接受这条任务。分类项与状态项是两套维度，
  // 但都作用在同一张表上，所以用同一个 currentView 决定。
  function matchView(t) {
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

  // 速度是后端格式化好的字符串（"1.24 MiB/s"）。状态栏要汇总总速度，
  // 只能反解回字节数——让后端再加一个数值字段要动 Task 结构体与事件载荷，
  // 为一个底部统计不值得。
  function parseSpeed(s) {
    if (!s) return 0;
    const m = /^([\d.]+)\s*([KMGT]?)i?B\/s$/i.exec(s.trim());
    if (!m) return 0;
    const mult = { "": 1, K: 1024, M: 1024 ** 2, G: 1024 ** 3, T: 1024 ** 4 };
    return parseFloat(m[1]) * (mult[m[2].toUpperCase()] || 1);
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

    // 名称列：主行是文件名，副行是来源/清晰度等补充信息。
    // 单独一列放不下路径，用 title 兜住，悬停能看全。
    const nameCell = el("div", "tr-name");
    const title = el("div", "tr-title", t.title || t.url);
    title.title = t.output_path || t.url || t.title || "";
    nameCell.appendChild(title);
    const subBits = [];
    // 直链任务的 quality_label 是后端填的「手动新建」，写出来等于没说；
    // 换成来源域名，用户一眼能看出这个文件是从哪儿下的。
    const host = hostOf(t.url);
    if (t.kind === "direct" && host) subBits.push(host);
    else if (t.quality_label) subBits.push(t.quality_label);
    if (t.state === "failed" && t.error) subBits.push(t.error);
    else if (t.output_path && t.state === "done") subBits.push(t.output_path);
    if (subBits.length) {
      const sub = el("div", "tr-sub", subBits.join("  ·  "));
      sub.title = subBits.join("  ·  ");
      nameCell.appendChild(sub);
    }
    row.appendChild(nameCell);

    // 大小：优先总量，没有总量（yt-dlp 解析前）时退回到已下载量
    const size = t.total_bytes || t.downloaded_bytes;
    row.appendChild(el("div", "tr-size", size ? fmtBytes(size) : "—"));

    const stateCell = el("div", "tr-state");
    stateCell.appendChild(el("span", "task-state " + t.state, STATE_TEXT[t.state] || t.state));
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

    // 行内动作：只留最常用的一个，其余走菜单/工具栏的批量命令，
    // 否则七列宽的表里每行三个按钮会把名称列挤没。
    const actCell = el("div", "tr-act");
    const acting = t.state === "queued" || t.state === "downloading" || t.state === "merging";
    if (acting) {
      const p = el("button", "icon-btn", "暂停");
      p.title = "暂停（保留断点）";
      p.onclick = async () => {
        try {
          await invoke("pause_task", { id: t.id });
          refreshTasks();
        } catch (e) {
          toast(String(e), true);
        }
      };
      actCell.appendChild(p);
    } else if (t.state === "paused" || t.state === "failed") {
      const r = el("button", "icon-btn", "继续");
      r.title = "从断点继续";
      r.onclick = async () => {
        try {
          await invoke("resume_task", { id: t.id });
          refreshTasks();
        } catch (e) {
          toast(String(e), true);
        }
      };
      actCell.appendChild(r);
    } else if (done && t.output_path) {
      const o = el("button", "icon-btn", "打开");
      o.title = "打开所在文件夹";
      o.onclick = () => revealItem(t.output_path);
      actCell.appendChild(o);
    }
    row.appendChild(actCell);

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

  const ACTIVE_STATES = new Set(["queued", "downloading", "merging"]);

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
  async function refreshTasks() {
    try {
      const list = await invoke("list_tasks");
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
    } catch (e) {
      // 静默
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
    $("setHfCap").value = s.hf_thread_cap ?? 4;
    $("setHfToken").value = s.hf_token || "";
    $("setRateLimit").value = s.rate_limit_kb ?? 0;
    $("setPopupSaveAs").checked = s.popup_save_as ?? true;
    $("setPopupQuality").checked = s.popup_quality ?? true;
    $("setAutoProgress").checked = s.auto_progress_popup ?? true;
    $("setMergeMp4").checked = !!s.merge_to_mp4;
    $("setAutostart").checked = !!s.autostart;
    $("setTelemetry").checked = !!s.telemetry;
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
      auto_progress_popup: $("setAutoProgress").checked,
      autostart: $("setAutostart").checked,
      telemetry: $("setTelemetry").checked,
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
        for (const t of targets) revealItem(t.output_path);
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
      case "pauseSelected":
      case "deleteSelected": {
        const ids = [...selectedIds];
        if (ids.length === 0) {
          toast("请先在任务列表里勾选任务", true);
          break;
        }
        try {
          if (cmd === "resumeSelected") {
            const n = await invoke("resume_tasks", { args: { ids } });
            toast(`已恢复 ${n} 个任务`);
          } else if (cmd === "pauseSelected") {
            const n = await invoke("pause_tasks", { args: { ids } });
            toast(`已暂停 ${n} 个任务`);
          } else {
            // 删除和「清空」一样是不可撤销的：正在下的会被中止，临时文件会被清掉。
            // 不确认的话，误点一次 Del 就白下几十分钟。
            const ok = confirm(
              `确定删除选中的 ${ids.length} 个任务吗？\n\n正在下载的任务会被中止，未完成的临时文件会被删除。`
            );
            if (!ok) break;
            const n = await invoke("remove_tasks", { args: { ids } });
            toast(`已删除 ${n} 个任务`);
            ids.forEach((id) => selectedIds.delete(id));
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
        const ok = confirm(
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

      case "refresh":
        refreshTasks();
        toast("已刷新");
        break;

      case "openProgress":
        try {
          await invoke("open_progress_window");
        } catch (e) {
          toast(String(e), true);
        }
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
              edge://extensions 一起给你，加载一次后网页点下载就会自动转给 VGrab。</p>
           <p><b>暂停与恢复</b>：暂停保留断点，点「继续」从原位置接着下；
              「取消」才会真正放弃。</p>`
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
            `<p>扩展已经加载并在工作：网页点下载会自动转给 VGrab。</p>
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
          "关于 VGrab",
          `<p><b>VGrab</b> ${ver ? "v" + ver : ""}</p>
           <p>基于 Rust + Tauri 的下载器，任务调度与断点续传自研，
              视频解析使用 yt-dlp，音视频合并使用 ffmpeg。</p>
           <p class="dim">支持 HTTP/HTTPS 直链多线程分块下载、视频站点自动解析、
              代理与限速、浏览器 vgrab:// 协议集成。</p>`
        );
        break;
      }

      default:
        break;
    }
  }

  function showInfo(title, html) {
    $("infoTitle").textContent = title;
    $("infoBody").innerHTML = html;
    $("infoOverlay").classList.remove("hidden");
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

  // 全选
  $("selectAll").addEventListener("change", (e) => {
    const on = e.target.checked;
    if (on) taskMap.forEach((_, id) => selectedIds.add(id));
    else selectedIds.clear();
    refreshTasks();
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
      taskMap.forEach((_, id) => selectedIds.add(id));
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
      if (!$("saveAsOverlay").classList.contains("hidden")) closeSaveAs();
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
  $("parseBtn").onclick = doParse;
  $("urlInput").addEventListener("keydown", (e) => {
    if (e.key === "Enter") doParse();
  });
  $("downloadBtn").onclick = doDownload;
  $("dDownloadBtn").onclick = doDirectDownload;
  $("ndStart").onclick = doNewDownload;
  $("ndPickDir").onclick = pickNewDir;
  $("addClose").onclick = closeAddUrl;
  $("addOverlay").addEventListener("click", (e) => {
    // 点遮罩关闭，点对话框内部不关
    if (e.target === $("addOverlay")) closeAddUrl();
  });
  $("saveSettings").onclick = saveSettings;
  $("restoreDefaults").onclick = restoreDefaults;
  $("cancelSettings").onclick = () => { loadSettings(); };
  $("setProxyEnabled").addEventListener("change", syncProxyDetail);
  $("setTheme").addEventListener("change", (e) => changeTheme(e.target.value));

  // 手动新建面板里按回车 = 开始下载
  ["ndUrl", "ndFilename"].forEach((id) => {
    $(id).addEventListener("keydown", (e) => {
      if (e.key === "Enter") doNewDownload();
    });
  });

  $("setConcurrent").addEventListener("input", (e) => {
    $("setConcurrentVal").textContent = e.target.value;
  });
  $("setFragments").addEventListener("input", (e) => {
    $("setFragmentsVal").textContent = e.target.value;
  });
  $("setDefaultThreads").addEventListener("input", (e) => {
    $("setDefaultThreadsVal").textContent = e.target.value;
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
      const card = document.querySelector(`.task-row[data-id="${p.id}"]`);
      if (card) {
        // 状态没变时只改进度条宽度和数字，不重建整行：
        // 重建会顺带销毁按钮、丢焦点，10Hz × N 个任务时纯属浪费。
        if (stateChanged) {
          card.replaceWith(renderTask(merged));
        } else {
          updateCardProgress(card, merged);
        }
      } else {
        // 本地还没有这一行（任务刚入队 / 刚进入当前筛选条件）
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
  // 用户在网页/邮件里点了 vgrab:// 链接时，Rust 侧（deeplink.rs）把解析结果
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

  async function acceptExternalLink(p) {
    if (!p || !p.url) return;
    // 「先选目录再下载」开关：关掉时保持老行为，静默存入默认目录
    const s = appSettings || (await getSettingsCached());
    if (!s.popup_save_as) {
      await silentExternalDownload(p, s);
      return;
    }
    saveAsQueue.push(p);
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

    let r = null;
    try {
      r = await invoke("probe_auto", { url: p.url });
    } catch (e) {
      if (!saveAsCtx) return; // 探测期间用户已点取消
      $("saStatus").textContent = "识别失败：" + String(e) + "（仍可按直链下载）";
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
      // 两边都识别不了：不弹一个必然失败的框，直接报错收场
      $("saStatus").textContent = r.message || "无法识别该链接";
      $("saveAsOverlay").classList.add("hidden");
      saveAsCtx = null;
      return;
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
          },
        });
      } else {
        await invoke("new_download", {
          args: {
            url: p.url,
            filename: name,
            threads: Number($("saThreads").value || 1),
            save_dir: dir,
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
      ? `<p style="margin:8px 0 0">扩展目录（可直接复制）：<code style="user-select:all;word-break:break-all">${dir}</code></p>`
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
       <p style="margin:8px 0 0" class="dim">加载成功后扩展列表里会出现「VGrab Downloader」，工具栏上也有图标。之后网页点下载就会自动转给 VGrab，不用再管。</p>`,
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
        "缺少 yt-dlp。\n把 yt-dlp.exe 放到本程序所在目录，或设置环境变量 VGRAB_YTDLP 指向它，然后重启。"
      );
      openAddUrl();
    }
    // 冷启动是从链接进来的话，链接已经躺在队列里，这里补一次取用。
    drainExternalLinks();

    // 安装器勾了「浏览器扩展」组件 → 引导用户去浏览器扩展页手动加载。
    // Chrome 不允许安装器静默注册扩展，这一步省不掉，只能把路铺到嘴边。
    await maybeGuideExtension();
  })();
})();
