// 本地测试服务器：支持 Range 的静态文件服务 + 慢速节流。
//
// 用法: node scripts/test-server.mjs [sizeMB] [delayMsPerChunk]
//
// 默认 64 MiB，每 16 KiB 块延迟 2ms（≈8 MB/s 单连接），用来验证多线程
// 是否真的能叠加带宽、动态提速是否生效。
import http from "node:http";
import crypto from "node:crypto";

const SIZE_MB = Number(process.argv[2] || 64);
const CHUNK_DELAY_MS = Number(process.argv[3] || 2);
const SIZE = SIZE_MB * 1024 * 1024;

// 确定性的伪随机内容（同一尺寸每次生成相同数据，便于比对哈希）
const data = Buffer.alloc(SIZE);
let seed = 0x12345678;
for (let i = 0; i < SIZE; i++) {
  seed = (seed * 1103515245 + 12345) & 0x7fffffff;
  data[i] = seed & 0xff;
}
const sha = crypto.createHash("sha256").update(data).digest("hex");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, "http://127.0.0.1");

  if (url.pathname === "/sha") {
    res.writeHead(200, { "Content-Type": "text/plain" });
    res.end(sha);
    return;
  }
  if (url.pathname !== "/bigtest.bin") {
    res.writeHead(404);
    res.end("not found");
    return;
  }

  const range = req.headers.range;
  let start = 0;
  let end = SIZE - 1;
  let status = 200;
  if (range) {
    const m = /bytes=(\d+)-(\d*)/.exec(range);
    if (m) {
      start = Number(m[1]);
      end = m[2] ? Math.min(Number(m[2]), SIZE - 1) : SIZE - 1;
      status = 206;
    }
  }
  const headers = {
    "Content-Type": "application/octet-stream",
    "Accept-Ranges": "bytes",
    "Content-Length": String(end - start + 1),
    ETag: `"testserver-${SIZE_MB}m"`,
  };
  if (status === 206) {
    headers["Content-Range"] = `bytes ${start}-${end}/${SIZE}`;
  }
  res.writeHead(status, headers);

  const CHUNK = 16 * 1024;
  let pos = start;
  while (pos <= end) {
    const n = Math.min(CHUNK, end - pos + 1);
    if (!res.write(data.subarray(pos, pos + n))) {
      await new Promise((r) => res.once("drain", r));
    }
    pos += n;
    if (CHUNK_DELAY_MS > 0) await sleep(CHUNK_DELAY_MS);
    if (res.destroyed) return;
  }
  res.end();
});

server.listen(18777, "127.0.0.1", () => {
  console.log(`listening 127.0.0.1:18777  size=${SIZE_MB}MiB  chunkDelay=${CHUNK_DELAY_MS}ms`);
  console.log(`sha256=${sha}`);
});
