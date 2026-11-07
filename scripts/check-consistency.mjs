/**
 * 跨文件一致性校验：版本号（4 处）与桥端口（2 处）。
 *
 * 这些值分散在多个手工维护的文件里，漏改一处就会出现「扩展版本 ≠ 桌面版本」
 * 的 token 403、或扩展连错端口。CI 在合并前跑本脚本挡住；发布前手动跑也行：
 *   node scripts/check-consistency.mjs
 *
 * 校验的值以 package.json 为唯一事实源：
 *   - src-tauri/Cargo.toml   [package] version
 *   - src-tauri/tauri.conf.json   version
 *   - extension/manifest.json     version
 *   - extension/background.js     BRIDGE 常量里的端口
 *   - src-tauri/src/bridge.rs     pub const PORT
 */
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (p) => fs.readFileSync(path.join(root, p), "utf8");

const problems = [];

// ---- 版本号 ----
const pkg = JSON.parse(read("package.json"));
const expected = pkg.version;
if (!/^\d+\.\d+\.\d+$/.test(expected)) {
  problems.push(`package.json version 不是 x.y.z 形态: ${expected}`);
}

const cargo = read("src-tauri/Cargo.toml");
const cargoVer = /^\s*version\s*=\s*"([^"]+)"/m.exec(cargo)?.[1];
if (cargoVer !== expected) {
  problems.push(`Cargo.toml version (${cargoVer}) != package.json (${expected})`);
}

const conf = JSON.parse(read("src-tauri/tauri.conf.json"));
if (conf.version !== expected) {
  problems.push(`tauri.conf.json version (${conf.version}) != package.json (${expected})`);
}

const manifest = JSON.parse(read("extension/manifest.json"));
if (manifest.version !== expected) {
  problems.push(`extension/manifest.json version (${manifest.version}) != package.json (${expected})`);
}

// ---- 桥端口 ----
const portFromJs = /const BRIDGE = "http:\/\/127\.0\.0\.1:(\d+)"/.exec(read("extension/background.js"))?.[1];
const portFromRs = /pub const PORT: u16 = (\d+);/.exec(read("src-tauri/src/bridge.rs"))?.[1];
if (!portFromJs || !portFromRs || portFromJs !== portFromRs) {
  problems.push(
    `桥端口不一致: extension/background.js (${portFromJs}) vs src-tauri/src/bridge.rs (${portFromRs})`
  );
}

if (problems.length) {
  console.error("一致性校验失败：");
  for (const p of problems) console.error(`  - ${p}`);
  process.exit(1);
}
console.log(`一致性校验通过：版本 ${expected}（4 处），桥端口 ${portFromJs}（2 处）`);
