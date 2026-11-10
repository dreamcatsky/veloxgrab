// offscreen 文档的脚本。必须是独立文件：MV3 扩展页面的 CSP 是
// `script-src 'self'`，内联 <script> 会被直接拦截（实测错误：
// "Executing inline script violates the following Content Security
// Policy directive"），拉起也就从未发生。
//
// 导航到自定义协议由 Windows 协议分发处理；某些环境下 Chrome 会延迟或
// 吞掉首次导航（协议确认策略、文档生命周期），所以失败时重试一次。
function go() {
  location.href = "veloxgrab://launch";
}
go();
setTimeout(() => {
  // 仍在本页说明第一次导航没生效（导航成功时本文档通常会被替换或保持
  // 挂起但 URL 已变）。重试一次无害：single_instance 只会把窗口带到前台。
  if (location.protocol === "chrome-extension:") go();
}, 800);
