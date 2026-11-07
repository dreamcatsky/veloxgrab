// 纯函数工具集：前后端共享的格式化/解析逻辑，无 DOM、无 IPC 依赖。
//
// 为什么单独一个文件：app.js 是 3000 行的 IIFE，函数定义在闭包里，
// 测试只能起浏览器才能触达。抽到这里之后 node:test 可以直接 require——
// 前端测试覆盖的第一步是把「不碰 DOM 的逻辑」从闭包里捞出来。
//
// 加载方式：浏览器端 <script src="core.js"></script> 挂 window.VeloxGrabCore；
// Node 测试端 require 后拿 module.exports。UMD 尾部同时照顾两边。
(function (root, factory) {
  "use strict";
  if (typeof module === "object" && module.exports) {
    module.exports = factory();
  } else {
    root.VeloxGrabCore = factory();
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

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

  // 从文件名 / URL 里取扩展名（小写、不带点）。取不到返回 null。
  // 后端还会再规范化一次，这里只负责把探测阶段已知的信息带给分类目录。
  function extOfName(name) {
    if (!name) return null;
    const base = String(name).split(/[?#]/)[0];
    const last = base.split(/[\\/]/).pop() || "";
    const i = last.lastIndexOf(".");
    if (i <= 0 || i === last.length - 1) return null;
    return last.slice(i + 1).toLowerCase();
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

  /**
   * 把字符串转义成可安全拼进 innerHTML 的文本。
   *
   * 后端返回的目录名来自安装/解压位置，用户完全可能把它放在 `D:\a&b\<c>`
   * 这种路径下；直接拼进模板会让路径里的标签被当成 HTML 解析。
   */
  function escHtml(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
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

  return { fmtBytes, fmtDuration, hostOf, extOfName, nameLooksLikeUrl, escHtml, parseSpeed };
});
