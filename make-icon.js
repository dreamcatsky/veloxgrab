// 生成 VeloxGrab 应用图标：1024x1024 PNG，纯手写，无外部依赖
const fs = require("fs");
const path = require("path");
const zlib = require("zlib");

const W = 1024;
const H = 1024;

// 累积缓冲区（float，用于 alpha 混合）
const acc = new Float32Array(W * H * 4); // r,g,b 端 Premultiplied + a

function blend(x, y, r, g, b, a) {
  if (x < 0 || y < 0 || x >= W || y >= H || a <= 0) return;
  const i = (y * W + x) * 4;
  const sa = Math.min(1, a);
  const da = acc[i + 3];
  const outA = sa + da * (1 - sa);
  if (outA <= 0) return;
  acc[i] = (r * sa + acc[i] * da * (1 - sa)) / outA;
  acc[i + 1] = (g * sa + acc[i + 1] * da * (1 - sa)) / outA;
  acc[i + 2] = (b * sa + acc[i + 2] * da * (1 - sa)) / outA;
  acc[i + 3] = outA;
}

// 圆角矩形的覆盖率（0..1），超采样 2x2
function roundRectCov(x, y, x0, y0, x1, y1, rad) {
  let hits = 0;
  for (let sy = 0; sy < 2; sy++) {
    for (let sx = 0; sx < 2; sx++) {
      const px = x + (sx + 0.5) / 2;
      const py = y + (sy + 0.5) / 2;
      const cx = Math.min(Math.max(px, x0 + rad), x1 - rad);
      const cy = Math.min(Math.max(py, y0 + rad), y1 - rad);
      const dx = px - cx;
      const dy = py - cy;
      if (dx * dx + dy * dy <= rad * rad) hits++;
    }
  }
  return hits / 4;
}

function fillRoundRect(x0, y0, x1, y1, rad, colorFn) {
  const rx0 = Math.floor(x0),
    ry0 = Math.floor(y0);
  const rx1 = Math.ceil(x1),
    ry1 = Math.ceil(y1);
  for (let y = ry0; y <= ry1; y++) {
    for (let x = rx0; x <= rx1; x++) {
      const cov = roundRectCov(x, y, x0, y0, x1, y1, rad);
      if (cov <= 0) continue;
      const c = colorFn(x, y);
      if (c) blend(x, y, c[0], c[1], c[2], cov * c[3]);
    }
  }
}

// 圆角矩形描边：外圈减内圈
function strokeRoundRect(x0, y0, x1, y1, rad, thick, color) {
  const rx0 = Math.floor(x0 - thick),
    ry0 = Math.floor(y0 - thick);
  const rx1 = Math.ceil(x1 + thick),
    ry1 = Math.ceil(y1 + thick);
  for (let y = ry0; y <= ry1; y++) {
    for (let x = rx0; x <= rx1; x++) {
      const outer = roundRectCov(x, y, x0, y0, x1, y1, rad);
      const inner = roundRectCov(
        x,
        y,
        x0 + thick,
        y0 + thick,
        x1 - thick,
        y1 - thick,
        Math.max(1, rad - thick)
      );
      const cov = Math.max(0, outer - inner);
      if (cov > 0) blend(x, y, color[0], color[1], color[2], cov * color[3]);
    }
  }
}

// ========== 1. 深色圆角底板 ==========
const pad = 60;
const x0 = pad,
  y0 = pad,
  x1 = W - pad,
  y1 = H - pad;
const rad = 200;

fillRoundRect(x0, y0, x1, y1, rad, (x, y) => {
  // 垂直渐变：顶部 #2A3550 → 底部 #171A21
  const t = (y - y0) / (y1 - y0);
  const r = Math.round(42 + (23 - 42) * t);
  const g = Math.round(53 + (26 - 53) * t);
  const b = Math.round(80 + (33 - 80) * t);
  return [r, g, b, 1];
});

// ========== 2. 顶部蓝色光晕 ==========
for (let y = y0; y < H; y++) {
  const t = Math.max(0, (y - y0) / (H * 0.62));
  const a = Math.pow(1 - t, 2.2) * 0.38;
  if (a <= 0.002) continue;
  for (let x = x0; x < x1; x++) {
    const cov = roundRectCov(x, y, x0, y0, x1, y1, rad);
    if (cov <= 0) continue;
    // 中心偏上的径向光斑
    const dx = (x - W / 2) / (W * 0.55);
    const dy = (y - H * 0.28) / (H * 0.45);
    const radial = Math.max(0, 1 - Math.sqrt(dx * dx + dy * dy));
    const aa = a * radial;
    if (aa > 0.002) blend(x, y, 75, 139, 245, cov * aa);
  }
}

// ========== 3. 内描边高光 ==========
strokeRoundRect(x0, y0, x1, y1, rad, 3, [255, 255, 255, 0.16]);

// ========== 4. 下载箭头 ==========
// 竖杆
const shaftW = 92;
const shaftTop = 250;
const shaftBot = 580;
const cx = W / 2;

fillRoundRect(
  cx - shaftW / 2,
  shaftTop,
  cx + shaftW / 2,
  shaftBot + 10,
  12,
  () => [255, 255, 255, 1]
);

// 箭头三角（用逐行扫描填充）
const headTop = shaftBot - 10;
const headBot = 700;
const headHalf = 175;
for (let y = headTop; y <= headBot; y++) {
  const t = (y - headTop) / (headBot - headTop); // 0..1
  const half = headHalf * (1 - t);
  if (half <= 0.4) continue;
  const lx = cx - half;
  const rx = cx + half;
  for (let x = Math.floor(lx); x <= Math.ceil(rx); x++) {
    // 水平方向抗锯齿
    const covL = Math.max(0, Math.min(1, x + 1 - lx));
    const covR = Math.max(0, Math.min(1, rx - x));
    const cov = Math.min(covL, covR);
    // 底部边缘抗锯齿
    const edgeA = Math.min(1, (headBot - y) / 1.5);
    if (cov > 0 && edgeA > 0) blend(x, y, 255, 255, 255, cov * edgeA);
  }
}

// ========== 5. 底部托盘 (U 形) ==========
// 用「粗描边的下半圆弧 + 两条竖线」构造，逐像素按有符号距离填充
const trayW = 400;        // 外宽
const trayThick = 50;     // 线宽
const trayTop = 706;      // 竖线顶端（贴近箭头下方）
const trayBotY = 868;     // 圆弧最低点的外沿
const trayL = cx - trayW / 2;
const trayR = cx + trayW / 2;
const arcR = trayW / 2 - trayThick / 2; // 中心线半径
const arcCy = trayBotY - trayW / 2;     // 圆心 y（半圆）

function coverTray(px, py) {
  // 左竖线
  if (py <= arcCy) {
    const dl = Math.abs(px - (trayL + trayThick / 2));
    const dr = Math.abs(px - (trayR - trayThick / 2));
    if (py >= trayTop) {
      if (dl <= trayThick / 2) return true;
      if (dr <= trayThick / 2) return true;
    }
    return false;
  }
  // 下半圆弧：到圆心的距离落在 [arcR - thick/2, arcR + thick/2]
  const dx = px - cx;
  const dy = py - arcCy;
  if (dy < 0) return false;
  const d = Math.sqrt(dx * dx + dy * dy);
  return Math.abs(d - arcR) <= trayThick / 2;
}

for (let y = trayTop - 3; y <= trayBotY + 3; y++) {
  for (let x = trayL - 3; x <= trayR + 3; x++) {
    // 4x4 超采样抗锯齿
    let hits = 0;
    for (let sy = 0; sy < 4; sy++) {
      for (let sx = 0; sx < 4; sx++) {
        if (coverTray(x + (sx + 0.5) / 4, y + (sy + 0.5) / 4)) hits++;
      }
    }
    if (hits > 0) blend(x, y, 255, 255, 255, hits / 16);
  }
}

// ========== 编码 PNG ==========
function crc32(data) {
  let c = ~0 >>> 0;
  for (let i = 0; i < data.length; i++) {
    c ^= data[i];
    for (let k = 0; k < 8; k++) c = ((c >>> 1) ^ (0xedb88320 & -(c & 1))) >>> 0;
  }
  return (~c) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

// 转成 8-bit RGBA
const pixels = Buffer.alloc(W * H * 4);
for (let i = 0; i < W * H; i++) {
  const a = acc[i * 4 + 3];
  if (a <= 0.001) {
    pixels[i * 4] = 0;
    pixels[i * 4 + 1] = 0;
    pixels[i * 4 + 2] = 0;
    pixels[i * 4 + 3] = 0;
  } else {
    pixels[i * 4] = Math.round(Math.min(255, acc[i * 4]));
    pixels[i * 4 + 1] = Math.round(Math.min(255, acc[i * 4 + 1]));
    pixels[i * 4 + 2] = Math.round(Math.min(255, acc[i * 4 + 2]));
    pixels[i * 4 + 3] = Math.round(Math.min(1, a) * 255);
  }
}

const raw = Buffer.alloc((W * 4 + 1) * H);
for (let y = 0; y < H; y++) {
  raw[y * (W * 4 + 1)] = 0;
  pixels.copy(raw, y * (W * 4 + 1) + 1, y * W * 4, (y + 1) * W * 4);
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(W, 0);
ihdr.writeUInt32BE(H, 4);
ihdr[8] = 8;
ihdr[9] = 6;
ihdr[10] = 0;
ihdr[11] = 0;
ihdr[12] = 0;

const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);

const outDir = path.join(__dirname, "src-tauri", "icons");
fs.mkdirSync(outDir, { recursive: true });
fs.writeFileSync(path.join(outDir, "source.png"), png);
console.log(`wrote source.png ${png.length} bytes`);
