# CLAUDE.md — FileTransfer（drop-app）

手机 ↔ 电脑局域网互传工具。上游 GitHub `RedStoneGuo/Drop_App` v0.1.1，本仓库是在其基础上的个人改进版。
技术栈：Tauri 2 + Rust（`warp` + `tokio-tungstenite` 自带 WS 服务）+ 原生 WebView 前端（无框架、无打包器、无构建步骤）。

## 铁律

**绝不压缩，必须原图上传。** 任何「传前压缩 / 缩小尺寸 / 降质量 / 转码 / HEIC→JPG」的改动一律不做，除非用户主动要求。

## 目录职责

| 路径 | 说明 |
|---|---|
| `src/` | 桌面端前端（`index.html` / `main.js` / `styles.css` / `qrcode.js`） |
| `dist/` | **桌面端真正的加载源**——`tauri.conf.json` 的 `frontendDist` 指向它 |
| `src-tauri/mobile/` | 手机端扫码页，由 `server.rs` 用 `include_str!` **编译期内嵌** |
| `src-tauri/src/` | Rust 后端（`server.rs` 传输服务、`files.rs` 落盘与系统集成） |

**改 `src/` 后必须把文件拷进 `dist/`再编译**，否则改了等于没改。
改 `src-tauri/mobile/` 或任何 `.rs` 只需重新编译，不用拷贝。

## 构建与部署

构建脚本在仓库外面：`D:\Software\claude-packages\FileTransfer_App\`

```bash
_build_patch.cmd        # 编译（见下方两条必须遵守的约束）
_install_real.ps1       # 备份 + 替换 + 比对 SHA256
_shot_pid.ps1 -TargetPid <pid> -Out <png>   # 截图验证
```

1. **`--features custom-protocol` 不能省。** 少了它 exe 会被打成开发模式，WebView 去拉不存在的 `http://localhost:1420`，界面直接显示 `ERR_CONNECTION_REFUSED`。`cargo build --release` 不会自动带这个 feature。
2. **必须显式指定 MSVC toolchain**：`cargo +stable-x86_64-pc-windows-msvc build --release --features custom-protocol`（rustup 默认是 gnu）。

部署目标是 `C:\Users\19697\AppData\Local\Programs\FileTransfer\drop-app.exe`（用户实际在用的那份）。
`C:\Program Files\FileTransfer\` 是安装器装的原版，**不要动**。

## 验证

**编译通过 ≠ 能跑。** 每次改完都要真的启动 exe、截图、肉眼看界面出没出来——「开发模式打包」这类问题只有看图才能发现，HTTP smoke test 是完全正常的。

## 其他约定

- 程序数据在 `%APPDATA%\drop-app\`（`settings.json` 存 `download_dir`）。
- 传输链路是 `readAsDataURL` → base64 → 服务端逐字节写盘，**字节级无损**，别引入会改字节的处理。
- 临时脚本、截图等中间产物放 `D:\Software\claude-packages\FileTransfer_App\`，**不要丢进源码仓库**。
- 删除任何文件/目录一律走回收站，禁止 `rm`、`shutil.rmtree()` 这类永久删除。
- git-bash 里调 `cmd /c` 会被 MSYS 路径转换搞坏 → 用 `MSYS_NO_PATHCONV=1` 或直接 `powershell -File`。
