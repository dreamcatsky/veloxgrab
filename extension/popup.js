// VGrab 扩展弹窗逻辑
//
// 打开 popup 时做三件事：
//   1. 把当前标签页的 URL 填进输入框；
//   2. 扫描页面里的 <video>/<audio>/<img>/<a download>，列出来供一键发送；
//   3. 读 storage 里保存过的目录与线程数。
//
// 发送统一走 chrome.runtime.sendMessage → background.js：
// service worker 才有 tabs.update 权限，popup 自己导航不可靠。

const $ = (id) => document.getElementById(id);

function msg(text, isErr) {
  const m = $("msg");
  m.textContent = text || "";
  m.className = isErr ? "err" : "";
}

async function init() {
  // 恢复上次的目录/线程/接管开关
  const cfg = await chrome.storage.sync.get(["threads", "dir", "intercept"]);
  if (cfg.dir) $("dir").value = cfg.dir;
  if (cfg.threads) $("threads").value = String(cfg.threads);
  // 默认接管：这是用户装扩展最想要的行为，未设置过就当开启。
  $("intercept").checked = cfg.intercept !== false;
  $("intercept").onchange = () => {
    chrome.storage.sync.set({ intercept: $("intercept").checked });
    msg($("intercept").checked ? "已开启：网页下载将转给 VGrab" : "已关闭：网页下载走浏览器");
  };

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab && tab.url) $("url").value = tab.url;

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

  await chrome.storage.sync.set({ dir, threads });

  const btn = $("send");
  btn.disabled = true;
  msg("发送中…");
  try {
    const res = await chrome.runtime.sendMessage({
      type: "vgrab-send",
      url: url || target,
      dir,
      threads,
    });
    if (res && res.ok) {
      msg("已发送给 VGrab");
      setTimeout(() => window.close(), 700);
    } else {
      msg("VGrab 没有响应，请确认它正在运行（托盘图标可见）", true);
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
