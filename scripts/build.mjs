// 零依赖构建：把 src/ 下的静态文件原样拷贝到 dist/。
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const srcDir = join(root, "src");
const distDir = join(root, "dist");

rmSync(distDir, { recursive: true, force: true });
mkdirSync(distDir, { recursive: true });
cpSync(srcDir, distDir, { recursive: true });
console.log("Frontend build complete ->", distDir);
