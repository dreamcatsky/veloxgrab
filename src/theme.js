// 主题：必须在首帧之前落定。
//
// 放在 <head> 里同步执行（而不是等 app.js 的 boot() 去 invoke get_settings）：
// 那样做的话，深色用户在窗口出现的第一帧会看到深色的底、浅色用户在浅色系统下
// 会看到一帧深色再跳成浅色——webview 冷启动到拿到 IPC 结果之间有几百毫秒，
// 这个闪烁非常明显。
//
// 代价是这份「首帧用的主题」只能来自 localStorage 缓存，可能与 Rust 侧的
// settings.json 短暂不一致（用户在另一台机器上改过、或清过缓存）。
// 不一致最多持续到 app.js boot() 拿到真值为止，且 boot() 会把真值写回缓存，
// 下一帧起就对齐了。这是「宁可错一帧也不能闪一帧」的取舍。
(function () {
  "use strict";

  var KEY = "veloxgrab.theme";
  var VALUES = ["system", "light", "dark"];

  function matchLight() {
    return !!(
      window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches
    );
  }

  function normalize(v) {
    return VALUES.indexOf(v) >= 0 ? v : "system";
  }

  // 把三态解析成真正要画的两态。
  // system 且拿不到 matchMedia（很老的 webview）时退回深色：本程序一直是深色的，
  // 「跟随系统」在无从判断时保持原样，比擅自改成浅色安全。
  function resolve(theme) {
    if (theme === "light" || theme === "dark") return theme;
    return matchLight() ? "light" : "dark";
  }

  var mql = null;
  var follow = function () {};

  // 原生标题栏对齐：webview 的 CSS 管不到 DWM 画的标题栏，不显式设置时
  // 它跟随 Windows 系统主题——深色系统上的浅色应用会顶着一条黑标题栏。
  // 必须传**三态原值**：system 原样传给 Rust（映射成 None = 窗口跟随系统）。
  // 如果这里传解析后的两态，set_theme(Some(...)) 会把 webview 的
  // prefers-color-scheme 一并钉死，「跟随系统」在系统明暗再切换时就失灵。
  function syncNative(t) {
    try {
      var inv =
        window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
      if (inv) inv("set_window_theme", { theme: t }).catch(function () {});
    } catch (e) {
      // 对齐失败只影响标题栏颜色，不值得让它炸首帧
    }
  }

  function apply(theme) {
    var t = normalize(theme);
    document.documentElement.setAttribute("data-theme", resolve(t));

    try {
      localStorage.setItem(KEY, t);
    } catch (e) {
      // 隐私模式下 localStorage 可能直接抛。写不进去不影响本次渲染，
      // 只是下次启动要重新按系统猜一次。
    }
    syncNative(t);

    // 只有「跟随系统」时才需要监听系统切换。
    // 切到显式 light/dark 时必须把旧监听摘掉：否则用户刚选了深色，
    // 系统一变浅色就被改回去，看起来像设置没生效。
    if (mql) {
      mql.removeEventListener("change", follow);
      mql = null;
    }
    if (t === "system" && window.matchMedia) {
      mql = window.matchMedia("(prefers-color-scheme: light)");
      follow = function () {
        document.documentElement.setAttribute("data-theme", resolve("system"));
      };
      // addEventListener 在旧版 webview 的 MediaQueryList 上可能不存在，
      // 那种环境就退化成「不跟随实时切换，重启后生效」，好过整个脚本报错。
      if (mql.addEventListener) mql.addEventListener("change", follow);
    }

    return t;
  }

  function cached() {
    try {
      return normalize(localStorage.getItem(KEY));
    } catch (e) {
      return "system";
    }
  }

  window.VeloxGrabTheme = {
    KEY: KEY,
    apply: apply,
    cached: cached,
    resolve: resolve,
    normalize: normalize,
  };

  // 首帧落定：读缓存（读不到就是 system，按系统解析）
  apply(cached());
})();
