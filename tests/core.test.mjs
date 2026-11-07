// core.js（前端纯函数工具集）的 node:test 测试。
//
// 覆盖的是 P3-8 里「前端零测试」的最小闭环：不碰 DOM 的逻辑抽到 core.js
// 之后，这里直接 require 断言行为。app.js 内的解构别名保证生产路径用的
// 就是这些函数——测试锁住的是真实现，不是副本。
//
// 运行：node --test tests/
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const core = require(path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  "src",
  "core.js"
));

// ---- fmtBytes ----

test("fmtBytes formats common sizes", () => {
  assert.equal(core.fmtBytes(0), "");
  assert.equal(core.fmtBytes(-5), "");
  assert.equal(core.fmtBytes(undefined), "");
  assert.equal(core.fmtBytes(512), "512 B");
  assert.equal(core.fmtBytes(1024), "1.0 KB");
  assert.equal(core.fmtBytes(1536), "1.5 KB");
  assert.equal(core.fmtBytes(1024 ** 2), "1.0 MB");
  // >= 100 的判定作用于**除算后**的值：1.46 KiB → 一位小数，不是整数
  assert.equal(core.fmtBytes(1500), "1.5 KB");
  assert.equal(core.fmtBytes(1024 ** 3), "1.0 GB");
  // 超出单位表停在 GB
  assert.equal(core.fmtBytes(2 * 1024 ** 4), "2048 GB");
});

// ---- fmtDuration ----

test("fmtDuration formats mm:ss and h:mm:ss", () => {
  assert.equal(core.fmtDuration(0), "");
  assert.equal(core.fmtDuration(-1), "");
  // 分钟段 pad，秒不 pad（现状：59 → "0:59"，不到 10 分时不补零）
  assert.equal(core.fmtDuration(59), "0:59");
  assert.equal(core.fmtDuration(60), "1:00");
  assert.equal(core.fmtDuration(3661), "1:01:01");
  // 小数秒四舍五入
  assert.equal(core.fmtDuration(59.6), "1:00");
});

// ---- hostOf ----

test("hostOf strips scheme, userinfo, port and www", () => {
  assert.equal(core.hostOf("https://user:pw@www.a.b.com:8443/x?y#z"), "a.b.com");
  assert.equal(core.hostOf("http://127.0.0.1:8000/f"), "127.0.0.1");
  assert.equal(core.hostOf(""), "");
  assert.equal(core.hostOf(null), "");
  // 裸串（无 scheme）不抛异常
  assert.equal(core.hostOf("example.com/x"), "example.com");
  // IPv6 字面量是已知局限：按第一个 ':' 切，返回 "["。仅用于任务列表的
  // 来源展示提示，不参与任何判定——锁住现状，若将来改括号感知在此更新。
  assert.equal(core.hostOf("http://[::1]:8080/f"), "[");
});

// ---- extOfName ----

test("extOfName extracts lowercase ext from names and URLs", () => {
  assert.equal(core.extOfName("movie.MP4"), "mp4");
  assert.equal(core.extOfName("https://e.com/a/b/file.zip?token=x#f"), "zip");
  assert.equal(core.extOfName("https://e.com/download"), null);
  assert.equal(core.extOfName(".gitignore"), null);
  assert.equal(core.extOfName("a."), null);
  assert.equal(core.extOfName(""), null);
});

// ---- nameLooksLikeUrl ----

test("nameLooksLikeUrl only flags URL-derived names", () => {
  // URL 末段与文件名一致 = 「抄来的」
  assert.equal(core.nameLooksLikeUrl("f.bin", "https://e.com/dl/f.bin"), true);
  // 忽略大小写
  assert.equal(core.nameLooksLikeUrl("F.BIN", "https://e.com/dl/f.bin"), true);
  // 服务器声明的真名与 URL 末段不同 = 有权威性
  assert.equal(core.nameLooksLikeUrl("probe.bin", "https://e.com/dl/f.bin"), false);
  // query 不参与比较
  assert.equal(core.nameLooksLikeUrl("f.bin", "https://e.com/dl/f.bin?sig=abc"), true);
  // 非法转义序列退化成直接比较，不抛异常
  assert.equal(core.nameLooksLikeUrl("a%b", "https://e.com/a%zz"), false);
  assert.equal(core.nameLooksLikeUrl("", "https://e.com/x"), false);
  assert.equal(core.nameLooksLikeUrl("x", ""), false);
});

// ---- escHtml ----

test("escHtml escapes all five markup-significant chars", () => {
  assert.equal(core.escHtml(`D:\\a&b\\<c>"d"'e'`), "D:\\a&amp;b\\&lt;c&gt;&quot;d&quot;&#39;e&#39;");
  assert.equal(core.escHtml(123), "123");
  assert.equal(core.escHtml(null), "null");
  // 转义后不得再含可被解析的裸 < 或 &
  const out = core.escHtml('<img src=x onerror=alert(1)>&"\'');
  assert.ok(!/[<>&"']/.test(out.replace(/&amp;|&lt;|&gt;|&quot;|&#39;/g, "")));
});

// ---- parseSpeed ----

test("parseSpeed parses backend speed strings", () => {
  assert.equal(core.parseSpeed("1.24 MiB/s"), 1.24 * 1024 ** 2);
  assert.equal(core.parseSpeed("512 B/s"), 512);
  assert.equal(core.parseSpeed("2 KiB/s"), 2048);
  assert.equal(core.parseSpeed("1.5 GiB/s"), 1.5 * 1024 ** 3);
  assert.equal(core.parseSpeed("3 TiB/s"), 3 * 1024 ** 4);
  // 容错：空/畸形
  assert.equal(core.parseSpeed(""), 0);
  assert.equal(core.parseSpeed(null), 0);
  assert.equal(core.parseSpeed("unknown"), 0);
  // i 可省略（"MB/s" 同样命中——正则的 i? 就是为后端可能的十进制写法留的容差）
  assert.equal(core.parseSpeed("1.24 MB/s"), 1.24 * 1024 ** 2);
});
