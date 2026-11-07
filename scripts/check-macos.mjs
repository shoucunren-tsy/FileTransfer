import { spawnSync } from "node:child_process";

if (process.platform !== "darwin") {
  console.error("macOS 构建必须在 Mac 上执行。");
  process.exit(1);
}

const requiredCommands = [
  ["xcode-select", ["-p"], "Xcode Command Line Tools"],
  ["rustc", ["--version"], "Rust 工具链"],
  ["cargo", ["--version"], "Cargo"],
];

const missing = requiredCommands.filter(([command, args]) => {
  const result = spawnSync(command, args, { stdio: "ignore" });
  return result.status !== 0;
});

if (missing.length > 0) {
  console.error(`缺少构建环境：${missing.map(([, , label]) => label).join("、")}`);
  console.error("请先安装 Xcode Command Line Tools 与 Rust，再重新执行 npm run mac:build。");
  process.exit(1);
}

console.log("macOS 构建环境检查通过。");
