// 生成 veloxgrab.exe 的 hdiff 增量补丁，并（可选）上传到当前版本的 Release。
//
// ## 为什么补「裸 exe」而不是安装包
//
// NSIS 用 LZMA 重压缩整包数据块：新旧安装包哪怕只差 1MB 的程序体，两个
// 压缩流也几乎没有相同字节，diff 出来 ≈ 全量。所以补丁的对象是**已安装
// 的 veloxgrab.exe**（15MB 级、相邻版本差异小）——更新器（update.rs 的
// update_now_patch）把补丁应用到本机 exe 上，产物再对「裸 exe 资产的发布
// 摘要」逐字节校验。Ghost-Downloader-3 的 .hdiff 资产也是同一思路。
//
// ## 用法
//
//   node scripts/make-patch.mjs [--from v1.5.4] [--upload]
//
//   --from    上一个 release 的 tag（缺省：gh release list 最新一个已发布的）
//   --upload  生成并校验通过后，把补丁与裸 exe 上传到 v<当前版本> 的 release
//
// 前置条件：cargo build --release 已完成（取 target/release/veloxgrab.exe
// 作为新 exe）；gh CLI 已登录；7z 在 PATH（GitHub Windows runner 自带，
// 本机用 scoop 装的即可）。
//
// ## 产物命名（与 update.rs 的 patch_asset_names 契约一致）
//
//   patch-veloxgrab-<from版本>-to-<to版本>.hdiff   补丁本体
//   veloxgrab-<to版本>.exe                          裸 exe（digest 来源）
//
// 脚本内部会先做一次**往返校验**（hpatchz 应用补丁 → 与新 exe 比哈希）
// 再谈上传——补丁坏了宁可不上传，更新器那边摘要兜底也拦得住，但拦住
// 之前的下载流量就浪费了。

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import os from "node:os";

const ROOT = path.resolve(import.meta.dirname, "..");
const HDIFF_TAG = "v5.1.3"; // HDiffPatch 工具钉版本：补丁格式升级会破坏兼容

const arg = (name) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : null;
};
const has = (name) => process.argv.includes(name);

const run = (cmd, args, opts = {}) =>
  execFileSync(cmd, args, { stdio: ["ignore", "pipe", "pipe"], encoding: "utf8", ...opts });

function ghJson(...args) {
  return JSON.parse(run("gh", args).trim());
}

function main() {
  const pkg = JSON.parse(fs.readFileSync(path.join(ROOT, "package.json"), "utf8"));
  const toVer = pkg.version;
  // 只认**已发布**的 release：本次 tag 触发的草稿 release 刚被创建出来，
  // 自动探测必须跳过它，否则 from == to
  const rels = ghJson("release", "list", "--limit", "10", "--json", "tagName,isDraft");
  const prev = rels.find((r) => !r.isDraft && r.tagName !== `v${toVer}`);
  const fromTag = arg("--from") || prev?.tagName;
  if (!fromTag) throw new Error("找不到上一个 release，请用 --from 指定 tag");
  const fromVer = fromTag.replace(/^v/, "");
  if (fromVer === toVer) throw new Error(`--from (${fromVer}) 与当前版本相同，无需补丁`);

  const work = fs.mkdtempSync(path.join(os.tmpdir(), "vgx-patch-"));
  const tools = path.join(os.tmpdir(), "vgx-hdiff-" + HDIFF_TAG);
  fs.mkdirSync(tools, { recursive: true });
  let hdiffz = path.join(tools, "hdiffz.exe");
  if (!fs.existsSync(hdiffz)) {
    console.log(`下载 HDiffPatch ${HDIFF_TAG} 工具…`);
    const zip = path.join(tools, "hd.zip");
    run("gh", [
      "release", "download", HDIFF_TAG, "--repo", "sisong/HDiffPatch",
      "--pattern", `hdiffpatch_${HDIFF_TAG.slice(1)}_bin_windows64.zip`,
      "--output", zip, "--clobber",
    ]);
    // 工具在 zip 的顶层子目录里，include 过滤器匹配不到相对路径——
    // 包里只有 hdiffz/hpatchz 两个 exe，全量解出来就是平铺的
    run("7z", ["e", "-y", zip, `-o${tools}`]);
  }

  console.log(`下载上一个 release：${fromTag} 的安装包…`);
  const assets = ghJson("release", "view", fromTag, "--json", "assets")
    .assets.map((a) => a.name);
  const setup = assets.find((n) => n.toLowerCase().endsWith("setup.exe"));
  if (!setup) throw new Error(`${fromTag} 里没有 setup.exe 资产`);
  const oldSetup = path.join(work, setup);
  run("gh", ["release", "download", fromTag, "--pattern", setup, "--output", oldSetup, "--clobber"]);

  console.log("解出旧版 veloxgrab.exe…");
  run("7z", ["e", "-y", oldSetup, `-o${work}`]);
  const oldExe = path.join(work, "veloxgrab.exe");
  if (!fs.existsSync(oldExe)) throw new Error("旧安装包里找不到 veloxgrab.exe");

  let newExe = arg("--exe") || path.join(ROOT, "src-tauri", "target", "release", "veloxgrab.exe");
  newExe = path.resolve(newExe);
  if (!fs.existsSync(newExe)) throw new Error(`新 exe 不存在：${newExe}——先构建（CI 上签名后传 --exe）`);

  const patchName = `patch-veloxgrab-${fromVer}-to-${toVer}.hdiff`;
  const patchPath = path.join(ROOT, "src-tauri", "target", "release", patchName);
  console.log(`生成补丁 ${patchName} …`);
  run(hdiffz, [oldExe, newExe, patchPath]);

  // 往返校验：补丁坏了就不该进发布流程
  console.log("往返校验（hpatchz 应用 → 比对 SHA-256）…");
  const applied = path.join(work, "applied.exe");
  run(path.join(tools, "hpatchz.exe"), [oldExe, patchPath, applied]);
  const sha = (p) => crypto.createHash("sha256").update(fs.readFileSync(p)).digest("hex");
  if (sha(applied) !== sha(newExe)) {
    throw new Error("往返校验失败：补丁产物与新 exe 不一致，禁止上传");
  }

  // 裸 exe 资产：更新器用它资产上的 digest 字段校验补丁产物
  const bareName = `veloxgrab-${toVer}.exe`;
  const barePath = path.join(ROOT, "src-tauri", "target", "release", bareName);
  fs.copyFileSync(newExe, barePath);

  const patchSize = fs.statSync(patchPath).size;
  const fullSize = fs.statSync(path.join(ROOT, "src-tauri/target/release/bundle/nsis",
    `VeloxGrab_${toVer}_x64-setup.exe`)).size;
  console.log(`补丁 ${patchName}：${(patchSize / 1048576).toFixed(1)} MB` +
    `（全量安装包 ${(fullSize / 1048576).toFixed(1)} MB）`);
  console.log(`产物哈希（补丁结果应为此值）：sha256:${sha(newExe)}`);

  if (has("--upload")) {
    const tag = `v${toVer}`;
    console.log(`上传到 ${tag} …`);
    run("gh", ["release", "upload", tag, patchPath, barePath, "--clobber"]);
    console.log("✅ 已上传");
  } else {
    console.log(`（未上传；要上传请加 --upload，或手动：`);
    console.log(`  gh release upload ${`v${toVer}`} "${patchPath}" "${barePath}" --clobber`);
  }
}

main();
