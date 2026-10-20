/**
 * Tauri 的 NSIS 生成器不输出「组件选择页」，但用户在安装时需要一个勾选框来决定
 * 要不要装浏览器扩展。
 *
 * 时序问题：`tauri build --bundles nsis` 内部会先生成 installer.nsi、**紧接着
 * 就调 makensis**，所以简单地在 build 之后 patch nsi 是来不及的——setup.exe
 * 已经打完了。
 *
 * 因此本脚本做两件事：
 *   1. patch 现有 installer.nsi（幂等）
 *   2. 用 patch 后的 nsi 重新跑一次 makensis，覆盖 bundle\nsis\setup.exe
 *
 * makensis 位置：%LOCALAPPDATA%\tauri\NSIS\Bin\makensis.exe（Tauri 自己下载的那份），
 * 也可用环境变量 MAKENSIS 覆盖。
 *
 * patch 内容：
 *   - 声明可选 Section SEC_EXT（`/o` = 默认不勾选）
 *   - 把 Section Install 里 extension\* 的 File 行搬进 SEC_EXT（只搬 File，Delete 留在卸载段）
 *   - 插入 MUI_PAGE_COMPONENTS（静默安装 SkipIfPassive 自动跳过）
 *   - 写 SimpChinese/English 的 SEC_EXT_DESC 语言串
 *   - SEC_EXT 选中时写 $INSTDIR\extension-installed.flag，供 VGrab 首启弹窗判断
 */
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import os from "node:os";

const EXT_FILES = [
  "README.md",
  "background.js",
  "icon128.png",
  "icon16.png",
  "icon48.png",
  "manifest.json",
  "popup.html",
  "popup.js",
];

// 从 tauri build 的输出里定位 NSIS 工作目录。约定：target/release/nsis/<triple>/
function findNsisDirs(root) {
  const nsisDir = path.join(root, "src-tauri", "target", "release", "nsis");
  if (!fs.existsSync(nsisDir)) {
    throw new Error(`找不到 NSIS 输出目录：${nsisDir}（先跑 tauri build --bundles nsis）`);
  }
  const triples = fs.readdirSync(nsisDir, { withFileTypes: true })
    .filter((d) => d.isDirectory())
    .map((d) => path.join(nsisDir, d.name));
  if (!triples.length) {
    throw new Error(`${nsisDir} 下没有 arch 目录`);
  }
  return triples;
}

// Tauri 自己下载的 makensis；也允许 MAKENSIS 环境变量覆盖。
function findMakensis() {
  if (process.env.MAKENSIS && fs.existsSync(process.env.MAKENSIS)) return process.env.MAKENSIS;
  const p = path.join(os.homedir(), "AppData", "Local", "tauri", "NSIS", "Bin", "makensis.exe");
  if (fs.existsSync(p)) return p;
  throw new Error("找不到 makensis.exe（设 MAKENSIS 环境变量指向它）");
}

function alreadyPatched(text) {
  return text.includes("SEC_EXT");
}

function patchOne(nsiPath) {
  let nsi = fs.readFileSync(nsiPath, "utf8");
  if (alreadyPatched(nsi)) {
    console.log(`  [skip] nsi 已是 patch 过的，跳过注入（仍会重建 setup.exe）`);
    return false;
  }

  // ---- 1. 搬 extension 的 File 行进可选 Section -------------------------------
  // 形如：  File /a "/oname=extension\popup.js" "D:\...\extension\popup.js"
  const lines = nsi.split(/\r?\n/);
  const extFileLines = [];
  const keptLines = [];
  for (const line of lines) {
    // 只要 /oname=extension\xxx 的 File 行；卸载段里的 Delete 行以 Delete 开头，不会被误搬。
    if (EXT_FILES.some((f) => line.includes(`extension\\${f}"`)) && /^\s*File\s/.test(line)) {
      extFileLines.push(line);
    } else {
      keptLines.push(line);
    }
  }
  if (!extFileLines.length) {
    throw new Error("没在 Section Install 里找到 extension 的 File 行，bundle.resources 变了吗？");
  }
  const movedBlock = extFileLines.map(l => `  ${l.trim()}`).join("\n");
  nsi = keptLines.join("\n");

  // ---- 2. 声明 SEC_EXT Section -------------------------------------------------
  // 放在 Section WebView2 之前（顺序无所谓，只是放前面更直观）。
  //
  // 静默安装 `/S` 的处理，踩过三个坑：
  //
  //  1. 只写 `/o`（默认不勾选）→ **静默安装照样装上**。NSIS 静默模式跳过组件页，
  //     Section 照原样执行，/o 只影响 GUI 初始勾选状态。
  //  2. `!insertmacro UnselectSection ${SEC_EXT}` → makensis warning 6000
  //     「unknown variable/constant」：该宏要 section **index**，不认 id 宏。
  //  3. Section 内 `${If} ${Silent}` / `${If} $PassiveMode = 1` → **都不生效**。
  //     ${Silent} 在 Section 执行期不等于安装器静默标记；$PassiveMode 是 tauri
  //     在 .onInit 用 ${GetOptions} $CMDLINE "/P" 解的，而 `/S` 不带 `/P`，
  //     实测一直是 0。
  //
  // 最终做法：自己在 .onInit 里解 `/S` 写进 `$VgrabSilent`，Section 里判它。
  // ${GetOptions} 找不到开关会把 Errors flag 置位但不改目标变量，所以先清零。
  //
  // flag 文件仍在写：只有 GUI 安装且用户勾了扩展才会有，`extension_installed()`
  // 据此决定首启要不要弹引导——静默安装没勾选就不弹，正合适。
  const secExt =
`; ---- 以下为 patch-installer.mjs 注入：可选「浏览器扩展」组件 ----
Section /o "浏览器扩展" SEC_EXT
  ; 静默安装不装扩展。/o 只影响 GUI 初始勾选，这里靠 $VgrabSilent 兜底。
  \${If} $VgrabSilent = 1
    Return
  \${EndIf}
  ; File 的 /oname 带子目录（extension\\xxx）时，目标目录必须已存在，
  ; SetOutPath 必须是 $INSTDIR 而不是 extension——否则 /oname 的路径会和
  ; SetOutPath 叠加成 extension\\extension\\xxx 从而静默失败（实测过）。
  SetOutPath $INSTDIR
  CreateDirectory "$INSTDIR\\extension"
${movedBlock}
  ; 给 VGrab 首启弹窗用的标记：用户勾了扩展
  ; 寄存器必须用 $R0 —— File 指令内部会把 $0 当临时寄存器覆盖，
  ; 用 $0 的话 FileOpen 拿到的是被覆盖后的值，写不出来，而 ClearErrors
  ; 把错误吞了，表现为「文件装了但 flag 没有」。
  ClearErrors
  FileOpen $R0 "$INSTDIR\\extension-installed.flag" w
  FileWrite $R0 "1"
  FileClose $R0
SectionEnd
; ---- patch 结束 ----`;

  // .onInit 里补一段解 /S 的逻辑。ClearErrors 必须每次 GetOptions 前调用，
  // 否则前一次的 Errors flag 会让后面的判断读到假值。
  const onInitAnchor =
`Function .onInit
  \${GetOptions} $CMDLINE "/P" $PassiveMode`;
  if (!nsi.includes(onInitAnchor)) {
    throw new Error("找不到 .onInit 里 PassiveMode 解析锚点，tauri 的 nsi 结构变了吗？");
  }
  nsi = nsi.replace(
    onInitAnchor,
    `Function .onInit
  ; ---- patch-installer.mjs 注入：解出静默安装标记，供 SEC_EXT 跳过扩展 ----
  ; /S 是 NSIS 的静默开关。GetOptions 失败会置 Errors 但不动目标变量，
  ; 所以每次都先 ClearErrors。
  ClearErrors
  \${GetOptions} $CMDLINE "/S" $VgrabSilent
  \${IfNot} \${Errors}
    StrCpy $VgrabSilent 1
  \${EndIf}
  ; ---- 注入结束 ----

  \${GetOptions} $CMDLINE "/P" $PassiveMode`
  );

  // $VgrabSilent 声明成 Var
  nsi = nsi.replace(
    /(^|\r?\n)Var PassiveMode(\r?\n|$)/,
    "$1Var PassiveMode$2Var VgrabSilent$2"
  );
  // 插到 "Section WebView2" 前面
  const anchor = "Section WebView2";
  const idx = nsi.indexOf(anchor);
  if (idx < 0) throw new Error("找不到 Section WebView2 锚点");
  nsi = nsi.slice(0, idx) + secExt + "\n\n" + nsi.slice(idx);

  // ---- 3. 在目录页之前插入组件页 -----------------------------------------------
  const dirPage = "; 5. Choose install directory page";
  const dIdx = nsi.indexOf(dirPage);
  if (dIdx < 0) throw new Error("找不到 Choose install directory page 锚点");
  const compPage =
`; 4b. Components page (可选浏览器扩展), 静默安装时自动跳过
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_COMPONENTS

`;
  nsi = nsi.slice(0, dIdx) + compPage + nsi.slice(dIdx);

  // ---- 4. 语言串（可选的描述串）-----------------------------------------------
  // MUI2 的组件页支持 `${SEC_EXT}` 的 tooltip：在 .onInit 里用
  //   !insertmacro MUI_DESCRIPTION_BEGIN / MUI_DESCRIPTION_TEXT
  // 这里不装 tooltip——组件页只有一项，Section 名「浏览器扩展」已经自解释，
  // 而 MUI_DESCRIPTION_BEGIN 只能在 Section/Function 内部之外、脚本顶层调用，
  // 我们注入的位置夹在 Section 之间，位置不对会报「command not valid outside Section」。
  // 保留 SEC_EXT_DESC 语言串不进 nsi（删掉这段逻辑），避免无用的字符串。

  fs.writeFileSync(nsiPath, nsi, "utf8");
  return true;
}

// 用 patch 后的 nsi 重新跑 makensis。
// 关键：nsi 自己写了 `!define OUTFILE "nsis-output.exe"` —— 这是 tauri 通过
// 命令行注入的**占位默认值**。天真地传 `/DOUTFILE=<path>` 是没用的：nsi 里的
// `!define` 会把命令行注入的值**覆盖**掉，产出还是落到 nsi 同目录的
// nsis-output.exe（实测过：-D 语法没错，就是被覆盖了）。
// 正确的做法是 `/XOutFile <path>` —— 它是「编译后执行」指令，跑在 !define
// 之后，能真正改掉 OutFile。
// 另外必须传 /DARCH=x64（nsi 里有 ${ARCH} 引用）。
function rebuild(nsisDir, makensis, outputExe) {
  const nsi = path.join(nsisDir, "installer.nsi");
  const args = [
    nsi,
    "/DARCH=x64",
    `/XOutFile ${outputExe}`,
  ];
  console.log(`  [makensis] installer.nsi → ${path.basename(outputExe)}`);
  // makensis 用 PowerShell 传参时中文路径容易踩管道编码问题，raw 输出直接透出
  execFileSync(makensis, args, { stdio: "inherit" });
  // 兜底检查：万一 OUTFile 又落到 nsi 同目录，说明自定义输出没生效。
  // 这时候不能保持沉默——用户会装到没 patch 的旧包。
  const stray = path.join(nsisDir, "nsis-output.exe");
  if (fs.existsSync(stray)) {
    throw new Error(`makensis 把产物写到了 ${stray}，自定义输出没生效（installer.nsi 结构变了？）`);
  }
  if (!fs.existsSync(outputExe)) {
    throw new Error(`makensis 没生成 ${outputExe}（看上面 makensis 的输出）`);
  }
  return outputExe;
}

// ---- 入口 -----------------------------------------------------------------------
const root = process.argv[2] || process.cwd();
const dirs = findNsisDirs(root);
const makensis = findMakensis();
// tauri 把 setup.exe 写到 target/release/bundle/nsis/；nsi 里的默认 OUTFILE 是
// 占位名，patch 脚本必须显式覆盖成正式名字，否则产出落在 nsi 同目录。
const bundleDir = path.resolve(dirs[0], "..", "..", "bundle", "nsis");
fs.mkdirSync(bundleDir, { recursive: true });
const versionMatch = /^!define VERSION "([^"]+)"/m.exec(
  fs.readFileSync(path.join(dirs[0], "installer.nsi"), "utf8")
);
const version = versionMatch ? versionMatch[1] : "0.1.0";
const outputExe = path.join(bundleDir, `VGrab_${version}_x64-setup.exe`);

let patched = 0;
for (const d of dirs) {
  console.log(`patch ${d}`);
  const injected = patchOne(path.join(d, "installer.nsi"));
  if (injected) {
    patched++;
    console.log(`  [ok] 已注入可选扩展组件`);
  }
  // 重建 setup.exe 每次都要做：tauri build 每次都会重新生成 nsi + setup.exe，
  // 所以「nsi 已 patch」不代表「setup.exe 是最新 patch 版」。
  // （踩过坑：一开始只在注入后重建，结果装的一直是 tauri 产出的未 patch 包。）
  rebuild(d, makensis, outputExe);
  console.log(`  [ok] setup.exe → ${outputExe}`);
}
console.log(`完成：注入 ${patched}/${dirs.length} 个 installer.nsi，重建 ${dirs.length} 个 setup.exe`);
process.exit(0);
