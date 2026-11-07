// 零依赖开发服务器：提供与生产构建一致的结构。
import { createServer } from "node:http";
import { readFileSync, statSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, extname, normalize, resolve } from "node:path";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "src");
const port = 1420;

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".json": "application/json",
};

createServer((req, res) => {
  let pathname = decodeURIComponent(new URL(req.url, "http://localhost").pathname);
  if (pathname === "/") pathname = "/index.html";
  let file = normalize(join(root, pathname));
  if (!file.startsWith(root)) {
    res.writeHead(403).end("Forbidden");
    return;
  }
  if (!existsSync(file)) {
    res.writeHead(404).end("Not found");
    return;
  }
  if (statSync(file).isDirectory()) file = join(file, "index.html");
  const body = readFileSync(file);
  res.writeHead(200, { "Content-Type": MIME[extname(file)] || "application/octet-stream" });
  res.end(body);
}).listen(port, () => console.log(`Dev server: http://localhost:${port}`));
