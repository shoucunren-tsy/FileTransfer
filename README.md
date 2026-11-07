# 文件互传 — 跨平台文件传输助手

一个轻量级桌面应用（macOS / Windows），让手机扫码后与电脑在同一局域网内互传文件和剪贴板内容，类似微信“文件传输助手”，但数据只在你自己的设备之间流动。

## 功能

- **扫码连接**：启动后界面显示二维码，手机扫码即可打开连接页并自动连接（无需安装手机 App）
- **电脑 → 手机**：选择本地文件（支持多选），分块通过 WebSocket 发送，手机浏览器直接下载
- **拖拽发送**：可将一个或多个文件直接拖入“发送文件到手机”区域，再点击发送
- **手机 → 电脑**：手机网页选择文件上传，电脑自动保存到指定目录（macOS 默认 `~/Library/Application Support/drop-app/received`，Windows 默认 `%APPDATA%\\drop-app\\received`，可在界面更改）
- **剪贴板双向同步**：电脑一键把剪贴板文本发到手机；手机也可把文本发到电脑并一键写入系统剪贴板
- **传输进度**：发送/接收均显示百分比进度
- **断线重连**：手机端自动重连（指数退避），电脑端实时显示连接状态
- **错误日志**：使用 Tauri log 插件记录到日志目录，界面提供日志面板
- **Windows 自动放行**：MSI 安装包自动放行本地子网访问 TCP 8080-8099，并清理旧版无效规则；默认端口被占用时自动顺延并显示实际端口

## 下载
前往[release页面](https://github.com/RedStoneGuo/Drop_App/releases)进行安装包下载

## 技术栈

- **框架**：Tauri 2（Rust 后端 + 原生 WebView 前端，Vanilla JS，无前端框架、无打包器）
- **通信**：Rust 内置 WebSocket 服务（监听 `0.0.0.0:8080`），手机浏览器直连
- **二维码**：内嵌 MIT 协议 [qrcode-generator](https://github.com/kazuhikoarase/qrcode-generator)，不依赖第三方服务
- **剪贴板**：`tauri-plugin-clipboard-manager`

## 目录结构

```
Drop_App/
├── src/                  # 桌面端前端（纯 HTML/CSS/JS，ES Module）
│   ├── index.html
│   ├── main.js           # 桌面端逻辑（二维码、发送/接收、剪贴板）
│   ├── styles.css
│   └── qrcode.js         # 二维码生成库（MIT）
├── src-tauri/            # Rust 后端
│   ├── src/
│   │   ├── lib.rs        # 应用入口、插件注册、命令注册
│   │   ├── server.rs     # WebSocket/HTTP 服务、会话校验、消息转发
│   │   ├── files.rs      # 文件分块读取/写入、目录选择、落盘
│   │   ├── session.rs    # 会话 ID 生成、局域网 IP 探测
│   │   └── state.rs      # 共享状态（连接、传输、保存目录）
│   ├── mobile/           # 手机端网页（内嵌到二进制，扫码打开）
│   │   ├── index.html
│   │   ├── app.js        # 连接、上传/下载、剪贴板
│   │   └── qrcode.js
│   ├── capabilities/     # Tauri 权限声明
│   └── tauri.conf.json
└── scripts/              # 零依赖开发服务器与构建脚本
```

## 环境要求

### Windows

- Windows 10/11（自带 WebView2 运行时）
- [Rust 工具链](https://rustup.rs)（`stable-msvc`）
- [Visual Studio Build Tools 2022](https://visualstudio.microsoft.com/zh-hans/downloads/)（勾选“使用 C++ 的桌面开发”），提供 MSVC 链接器
- Node.js 18+（仅用于运行构建脚本，本项目无 npm 运行时依赖）

### macOS

- macOS 12+
- Xcode Command Line Tools（`xcode-select --install`）
- Rust 工具链
- Node.js 18+
- 首次启动时，在 macOS“系统设置 → 隐私与安全性 → 本地网络”中允许 Drop App 访问本地网络

首次构建前安装依赖：

```bash
rustup target add aarch64-apple-darwin
npm install
```

## 开发运行

```bash
npm run dev       # 启动前端静态服务器（http://localhost:1420）
npm run tauri dev # 以开发模式运行桌面应用
```

macOS 快捷命令：

```bash
npm run mac:dev    # macOS 开发运行
npm run mac:build  # 生成 .app 与 .dmg
```

若需在 Apple Silicon Mac 上同时生成 Intel 版本，可先安装目标：

```bash
rustup target add x86_64-apple-darwin
npm run tauri build -- --target x86_64-apple-darwin --bundles app,dmg
```

## 打包安装包

```bash
npm run tauri build
```

- Windows 产物：`src-tauri/target/release/bundle/msi/*.msi`
- macOS 产物：`src-tauri/target/release/bundle/macos/*.app` 与 `src-tauri/target/release/bundle/dmg/*.dmg`（需在 macOS 上构建）

## 使用方法

1. 启动 Drop App，界面左侧显示二维码和地址（如 `http://192.168.1.5:8080/?session=xxxx`）
2. 手机与电脑连接同一 Wi-Fi/局域网，用手机相机或任意扫码工具扫描二维码
3. 手机浏览器打开页面后自动连接，电脑右上角显示“已连接设备”
4. 传文件：
   - 电脑 → 手机：点击“选择文件”（可多选）→“发送所选文件”，手机端自动开始下载
   - 手机 → 电脑：手机页面点“选择文件”，文件会自动保存到电脑保存目录
5. 剪贴板：
   - 电脑 → 手机：点击“读取剪贴板”后再点“发送剪贴板到手机”（或直接点发送）
   - 手机 → 电脑：手机页面点“获取电脑剪贴板”查看电脑内容；手机网页可粘贴文本后，电脑端会收到并支持一键写入系统剪贴板
6. 连接异常时，手机页面会自动重连；也可在电脑端点“更换会话”生成新二维码

> 若手机扫码无法连接，请确认手机与电脑在同一局域网；Windows MSI 会自动放行本地子网访问 TCP 8080-8099；更新旧版本时请重新运行最新版 MSI 完成防火墙规则升级。

## 安全设计

- WebSocket 服务仅监听局域网接口（`0.0.0.0` 以支持局域网访问），不做公网映射；会话 ID 随机生成（12 位字母数字），每次启动和点击“更换会话”都会更新
- 手机页面与 WebSocket 均校验会话 ID，无效会话返回 403 或直接断开
- 文件读取仅限用户通过原生文件对话框选择的路径；保存目录默认为系统应用数据目录下的 `drop-app/received`，也可由用户显式指定
- 剪贴板权限在 `capabilities/default.json` 中显式声明
- 单个文件上限 5 GiB；接收端做文件名清洗与字节数校验，防止路径穿越与超限写入

## 常见问题

**扫码后手机打不开页面**：确认手机与电脑在同一局域网；macOS 请在“隐私与安全性 → 本地网络”允许 Drop App；Windows 安装版（MSI）会自动放行 TCP 8080-8099；若使用源码/开发模式运行，请以管理员身份放行 TCP 8080-8099。若使用 VPN、访客 Wi-Fi 或路由器开启 AP 隔离，请先关闭后重试；也可尝试在手机浏览器地址栏手动输入电脑 IP。

**电脑 IP 显示 127.0.0.1**：说明未探测到局域网网卡。应用会优先使用默认路由对应的网卡，并自动跳过 Hyper-V/WSL/VMware/VPN 等虚拟网卡；若仍显示 127.0.0.1，请确认已连接 Wi-Fi 或有线网络。

**手机能打开页面但连接不上 / 提示端口被占用**：应用启动时会从 8080 开始自动顺延尝试可用端口，界面与二维码显示的就是实际监听端口；若仍无法连接，请检查路由器是否开启了 AP 隔离（同一 Wi-Fi 下设备互访限制）。

**大文件传输较慢**：当前实现为 base64 分块 + JSON 文本帧，适合日常文件（几十 MB 以内）；超大文件建议压缩后再传。

**日志在哪里**：Windows 为 `%APPDATA%\com.dropapp.desktop\logs\`，macOS 为 `~/Library/Logs/com.dropapp.desktop/`。

## 许可证

本项目代码以 MIT 许可证发布。二维码生成库版权归 Kazuhiko Arase，MIT 许可。
