// offscreen 文档的脚本。必须是独立文件：MV3 扩展页面的 CSP 是
// `script-src 'self'`，内联 <script> 会被直接拦截（实测错误：
// "Executing inline script violates the following Content Security
// Policy directive"），拉起也就从未发生。
location.href = "veloxgrab://launch";
