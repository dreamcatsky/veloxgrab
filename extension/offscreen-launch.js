// offscreen 文档的脚本。必须是独立文件：MV3 扩展页面的 CSP 是
// `script-src 'self'`，内联 <script> 更早就被拦了（实测错误：
// "Executing inline script violates the following Content Security
// Policy directive: script-src 'self'"），拉起也就从未发生。
//
// 导航到自定义协议由 Windows 协议分发处理；某些环境下 Chrome 会延迟或
// 吞掉首次导航（协议确认策略、文档生命周期）。这里用 location.replace
// （不写历史栈，避免 offscreen 文档被协议确认页顶替后残留可后退条目），
// 并按 200/600/1600ms 三次尝试——文档从创建到可导航有加载时序，单次
// location.href 赋值可能落在「文档还在解析」的窗口里被丢弃。重试无害：
// single_instance 只会把窗口带到前台。
//
// 每次尝试前探测「是否已经出发过」：导航真正发生时，本文档的 URL 会变成
// veloxgrab://…（或被顶替），protocol 不再是 chrome-extension:，后续
// 尝试自动停手。
function go() {
  location.replace("veloxgrab://launch");
}
go();
for (const delay of [200, 600, 1600]) {
  setTimeout(() => {
    if (location.protocol === "chrome-extension:") go();
  }, delay);
}
