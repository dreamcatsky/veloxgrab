// 生成 VeloxGrab 应用图标：1024x1024 PNG，纯手写，无外部依赖。
//
// 设计（与应用内「深色磨砂 + 青紫双强调」同一套视觉语言）：
//   · squircle（超椭圆 n≈4.4）底板，比普通圆角矩形更接近 iOS 的连续曲率，
//     观感立刻「贵」一档；深靛底色压得住，不留饱和色块的那种廉价感
//   · 两团极光辉光（紫 左上 / 青 右下）低透明度叠在底色里，模拟磨砂玻璃
//     后透光的效果，与主界面顶栏/侧栏的发光一致
//   · 主体是品牌 V 形笔画（顶栏 brand 同一个 V）：上粗下收的锥形笔画
//     传达「速度」，白→冰蓝渐变 + 顶点处青色光晕拉出层次
//   · V 下方一条青→紫渐变的「着陆条」承接下载语义（箭头落进槽位），
//     同时把构图重心拉回画面中轴
//   · 顶缘一道随高度衰减的内描边高光，是玻璃质感的点睛笔
const fs = require("fs");
const path = require("path");
const zlib = require("zlib");

const W = 1024;
const H = 1024;

// 累积缓冲区（float，用于 alpha 混合）
// 注意：blend() 做的是 **straight-alpha** 合成（颜色通道存的是未预乘的
// 「本色」），编码时必须乘回 alpha 转成 PNG 要求的预乘格式——否则低 alpha
// 的边缘像素会以「本色 255 + 微量 alpha」落盘，解码端反预乘成刺眼的
// 白色光晕（旧图标的边界弧线光晕就是它）。
const acc = new Float32Array(W * H * 4); // r,g,b 端 Straight alpha + a

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

const clamp01 = (v) => Math.max(0, Math.min(1, v));

// ---------- squircle（超椭圆）几何 ----------
const PAD = 44;
const CX = W / 2;
const CY = H / 2;
const AX = (W - 2 * PAD) / 2; // 半宽 468
const AY = (H - 2 * PAD) / 2; // 半高 468
const NEXP = 4.4;             // 越大越接近圆角矩形，4~5 之间是 iOS 手感

// 归一化超椭圆场：0 = 边界内，正值在外的近似有符号距离（按半宽缩放）
function squircleSDF(px, py) {
  const nx = Math.abs((px - CX) / AX);
  const ny = Math.abs((py - CY) / AY);
  const f = Math.pow(Math.pow(nx, NEXP) + Math.pow(ny, NEXP), 1 / NEXP);
  return (f - 1) * AX;
}

// 3x3 超采样的覆盖率的
function squircleCov(x, y) {
  let hits = 0;
  for (let sy = 0; sy < 3; sy++) {
    for (let sx = 0; sx < 3; sx++) {
      if (squircleSDF(x + (sx + 0.5) / 3, y + (sy + 0.5) / 3) < 0) hits++;
    }
  }
  return hits / 9;
}

// ---------- 1. 深靛底板 + 顶部微提亮 ----------
for (let y = 0; y < H; y++) {
  for (let x = 0; x < W; x++) {
    const cov = squircleCov(x, y);
    if (cov <= 0) continue;
    const t = clamp01((y - PAD) / (H - 2 * PAD));
    // 顶 #2B2B4E → 底 #131320，略微偏靛而不是纯灰黑
    const r = Math.round(43 + (19 - 43) * t);
    const g = Math.round(43 + (19 - 43) * t);
    const b = Math.round(78 + (32 - 78) * t);
    blend(x, y, r, g, b, cov);
  }
}

// ---------- 2. 极光双辉（紫 左上 / 青 右下），受 squircle 裁剪 ----------
function glow(gx, gy, gr, cr, cg, cb, alpha, falloff) {
  for (let y = 0; y < H; y++) {
    for (let x = 0; x < W; x++) {
      const cov = squircleCov(x, y);
      if (cov <= 0) continue;
      const dx = (x - gx) / gr;
      const dy = (y - gy) / gr;
      const d = Math.sqrt(dx * dx + dy * dy);
      const a = Math.pow(Math.max(0, 1 - d), falloff) * alpha;
      if (a > 0.003) blend(x, y, cr, cg, cb, cov * a);
    }
  }
}
glow(CX - AX * 0.42, CY - AY * 0.46, AX * 0.92, 124, 92, 255, 0.40, 2.2); // 紫
glow(CX + AX * 0.46, CY + AY * 0.50, AX * 0.90, 63, 224, 240, 0.36, 2.2); // 青

// ---------- 3. 顶缘内描边高光（玻璃质感，向下衰减） ----------
(function () {
  const inset = 3;
  const thick = 3.2;
  // 循环变量必须是整数：blend 直写 acc[(y*W+x)*4]，浮点坐标的非整数索引
  // 会静默失效，而小数部分恰好相消时又会**写错位置**——旧版的边界白色
  // 光晕就是这里的浮点 y/x 落错格子造成的。
  for (let y = Math.ceil(PAD - thick); y < H / 2; y++) {
    for (let x = Math.ceil(PAD - thick); x < W - PAD + thick; x++) {
      const sd = squircleSDF(x, y);
      if (sd > -0.5) continue; // 高光只画在形状**内部**
      const band = clamp01(1 - Math.abs(-sd - inset) / thick);
      if (band <= 0.01) continue;
      const t = clamp01((y - PAD) / (H - 2 * PAD));
      // 水平钟形：中段亮、到两角淡出——整条环都亮会像「箍了一圈铁丝」
      const bell = Math.pow(Math.max(0, 1 - Math.abs(x - CX) / (AX * 0.96)), 0.9);
      const a = band * 0.18 * Math.pow(1 - t, 1.6) * bell;
      if (a > 0.01) blend(x, y, 255, 255, 255, a);
    }
  }
})();

// ---------- 4. 品牌 V 形笔画（锥形描边 + 圆头 + 渐变） ----------
const VT = 296;      // 两个上端点的 y
const VB = 644;      // 底部顶点的 y
const VSPAN = 206;   // 上端点距中轴的半距
const VW_TOP = 112;  // 顶部笔画宽
const VW_BOT = 74;   // 顶点处笔画宽（收窄 = 速度感）

// 到线段的距离 + 沿程参数（用于锥形宽度与渐变着色）
function segHit(px, py, x0, y0, x1, y1) {
  const vx = x1 - x0;
  const vy = y1 - y0;
  const len2 = vx * vx + vy * vy;
  let u = ((px - x0) * vx + (py - y0) * vy) / len2;
  u = Math.max(0, Math.min(1, u));
  const qx = x0 + u * vx;
  const qy = y0 + u * vy;
  const dx = px - qx;
  const dy = py - qy;
  return { d: Math.sqrt(dx * dx + dy * dy), u };
}

// V 底点先铺一团青色光晕，把白色笔画从底色里「托」出来
glow(CX, VB + 20, AX * 0.55, 63, 224, 240, 0.30, 1.9);

// 两笔：左上→中底，右上→中底。3x3 超采样保边缘干净
(function () {
  const L0 = [CX - VSPAN, VT];
  const L1 = [CX, VB];
  const R0 = [CX + VSPAN, VT];
  const R1 = [CX, VB];
  const yMin = VT - VW_TOP;
  const yMax = VB + VW_TOP;
  for (let y = Math.floor(yMin); y <= Math.ceil(yMax); y++) {
    for (let x = Math.floor(CX - VSPAN - VW_TOP); x <= Math.ceil(CX + VSPAN + VW_TOP); x++) {
      let covSum = 0;
      let gy = 0; // 渐变参数（沿笔画从上到下）
      for (let sy = 0; sy < 3; sy++) {
        for (let sx = 0; sx < 3; sx++) {
          const px = x + (sx + 0.5) / 3;
          const py = y + (sy + 0.5) / 3;
          // 取两笔里更近的那条
          const hl = segHit(px, py, L0[0], L0[1], L1[0], L1[1]);
          const hr = segHit(px, py, R0[0], R0[1], R1[0], R1[1]);
          const h = hl.d <= hr.d ? hl : hr;
          const w = VW_TOP + (VW_BOT - VW_TOP) * h.u; // 锥形
          const a = clamp01(w / 2 + 0.5 - h.d);
          if (a > 0) {
            covSum += a;
            gy = Math.max(gy, h.u);
          }
        }
      }
      const cov = covSum / 9;
      if (cov > 0) {
        // 白 → 冰蓝，沿笔画向下过渡
        const r = Math.round(255 + (176 - 255) * gy);
        const g = Math.round(255 + (234 - 255) * gy);
        const b = Math.round(255 + (255 - 255) * gy);
        blend(x, y, r, g, b, cov);
      }
    }
  }
})();

// ---------- 5. 着陆条（青→紫渐变的圆角胶囊） ----------
(function () {
  const bw = 372;
  const bh = 54;
  const bx0 = CX - bw / 2;
  const bx1 = CX + bw / 2;
  const by0 = 742;
  const by1 = by0 + bh;
  const rad = bh / 2;
  for (let y = by0 - 2; y <= by1 + 2; y++) {
    for (let x = bx0 - 2; x <= bx1 + 2; x++) {
      // 圆角矩形 SDF
      const qx = Math.max(bx0 + rad, Math.min(x, bx1 - rad)) ;
      const qy = Math.max(by0 + rad, Math.min(y, by1 - rad));
      const d = Math.sqrt((x - qx) ** 2 + (y - qy) ** 2) - rad;
      // 胶囊本身
      const a = clamp01(0.5 - d);
      if (a <= 0) continue;
      const u = clamp01((x - bx0) / bw);
      // 青 → 紫 横向渐变
      const r = Math.round(63 + (124 - 63) * u);
      const g = Math.round(224 + (92 - 224) * u);
      const b = Math.round(240 + (255 - 240) * u);
      blend(x, y, r, g, b, a * 0.96);
      // 下缘一点同色光晕，承接 V 的「落入」感
      const ga = Math.pow(Math.max(0, 1 - Math.abs(d) / 46), 2.2) * 0.22;
      if (ga > 0.01) blend(x, y, r, g, b, a * ga);
    }
  }
})();

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
    // straight → premultiplied：颜色通道乘回 alpha
    pixels[i * 4] = Math.round(Math.min(255, acc[i * 4] * a));
    pixels[i * 4 + 1] = Math.round(Math.min(255, acc[i * 4 + 1] * a));
    pixels[i * 4 + 2] = Math.round(Math.min(255, acc[i * 4 + 2] * a));
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
