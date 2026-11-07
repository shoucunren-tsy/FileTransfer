import qrcode from "./qrcode.js";

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const $ = (id) => document.getElementById(id);

const state = {
  info: null,
  connected: false,
  sending: new Map(),      // id -> {name, size, sent, el}
  receiving: new Map(),    // id -> {name, size, received, chunks: [], el}
  outbox: [],              // 待发送给手机的消息队列
  flushing: false,
  // 最近一次读到的剪贴板负载，保证“界面看到的”和“实际发出去的”是同一份
  clipboardPayload: null,
};

const MAX_CHUNK = 512 * 1024;

// ---------- 基础工具 ----------

function formatSize(n) {
  if (n == null) return "";
  if (n < 1024) return n + " B";
  if (n < 1048576) return (n / 1024).toFixed(1) + " KB";
  if (n < 1073741824) return (n / 1048576).toFixed(1) + " MB";
  return (n / 1073741824).toFixed(2) + " GB";
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

let toastTimer = null;
function toast(text) {
  let el = $("toast");
  el.textContent = text;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 2200);
}

// ---------- 会话信息与二维码 ----------

async function refreshInfo() {
  try {
    const info = await invoke("get_session_info");
    state.info = info;
    $("serverAddr").textContent = `${info.ip}:${info.port}`;
    $("sessionId").textContent = info.session_id;
    $("saveDir").textContent = info.download_dir;
    $("qrHint").textContent =
      `手机扫码后打开网页并自动连接。请确保手机与电脑在同一局域网。若无法连接，请检查防火墙放行 ${info.port} 端口。`;
    drawQr(`http://${info.ip}:${info.port}/?session=${encodeURIComponent(info.session_id)}`);
  } catch (e) {
    toast("获取会话信息失败：" + e);
  }
}

function drawQr(text) {
  const qr = qrcode(0, "M");
  qr.addData(text);
  qr.make();
  $("qrImg").src = qr.createDataURL(6, 8);
}

// ---------- 状态显示 ----------

function setConnected(on) {
  state.connected = on;
  const pill = $("statusPill");
  pill.classList.toggle("online", on);
  pill.classList.toggle("offline", !on);
  pill.querySelector(".status-label").textContent = on ? "已连接设备" : "等待扫码连接";
  // 按钮保持可点：未连接时由点击 handler 弹 toast 提示，
  // 避免 disabled 按钮“点了毫无反应”的静默失败。
  $("sendSelectedBtn").classList.toggle("btn-dim", !on);
  $("sendClipBtn").classList.toggle("btn-dim", !on);
  if (!on) {
    // 断开后别再让防抖把内容推给“已经不在的手机”
    if (typeof cancelLiveSync === "function") cancelLiveSync();
    setSyncStatus("");
  }
  refreshSendButton();
}

function refreshSendButton() {
  const list = $("sendList");
  const hasPending = !!list.querySelector('.file-item[data-path][data-send-state="pending"]');
  $("sendSelectedBtn").disabled = !hasPending;
}

// ---------- 手机消息处理 ----------

async function onPhoneMessage(raw) {
  let msg;
  try { msg = JSON.parse(raw); } catch { return; }
  switch (msg.type) {
    case "file-meta":
      addReceivingItem(msg.id, msg.name, msg.size);
      break;
    case "file-chunk":
      await onPhoneChunk(msg);
      break;
    case "file-complete":
      await onPhoneComplete(msg.id);
      break;
    case "file-ack": {
      const item = state.sending.get(msg.id);
      if (item) {
        item.el.querySelector(".file-status").textContent =
          msg.status === "ok" ? "已送达" : "手机接收失败";
        item.el.querySelector(".file-status").className = "file-status " +
          (msg.status === "ok" ? "ok" : "err");
        item.el.dataset.sendState = msg.status === "ok" ? "sent" : "failed";
      }
      break;
    }
    case "clipboard":
      showReceivedClipboard(msg.text);
      break;
    case "clipboard-request":
      await sendClipboardToPhone();
      break;
    case "error":
      toast("手机端错误：" + (msg.message || "未知"));
      break;
  }
}

function addReceivingItem(id, name, size) {
  state.receiving.set(id, {
    id, name, size, received: 0, chunks: [], el: null,
    writeChain: Promise.resolve(), writeError: null,
  });
  const list = $("receivedDone");
  const empty = list.querySelector(".empty");
  if (empty) empty.remove();
  const item = document.createElement("div");
  item.className = "file-item";
  item.innerHTML = `
    <span class="file-icon">⬇</span>
    <span class="file-info">
      <div class="file-name"></div>
      <div class="file-size">0 / ${escapeHtml(formatSize(size))}</div>
      <div class="progress"><div></div></div>
    </span>
    <span class="file-status">接收中</span>`;
  item.querySelector(".file-name").textContent = name;
  list.appendChild(item);
  state.receiving.get(id).el = item;
}

async function onPhoneChunk(msg) {
  const t = state.receiving.get(msg.id);
  if (!t) return;
  t.chunks.push({ seq: msg.seq, data: msg.data });
  t.received += estimateBytes(msg.data);
  const pct = Math.min(100, Math.round((t.received * 100) / Math.max(1, t.size)));
  t.el.querySelector(".progress > div").style.width = pct + "%";
  t.el.querySelector(".file-size").textContent =
    `${formatSize(t.received)} / ${formatSize(t.size)}`;

  // 每 4 个分块写一次盘，降低命令调用频率
  if (t.chunks.length % 4 === 0 || t.received >= t.size) {
    t.writeChain = t.writeChain
      .then(() => t.writeError ? undefined : flushChunksToDisk(t))
      .catch((e) => {
        t.writeError = e;
      });
    await t.writeChain;
  }
}

async function flushChunksToDisk(t) {
  const pending = t.chunks;
  t.chunks = [];
  for (const c of pending) {
    try {
      await invoke("receive_file_chunk", {
        id: t.id,
        name: t.name,
        size: t.size,
        seq: c.seq,
        data: c.data,
      });
    } catch (e) {
      t.el.querySelector(".file-status").textContent = "写入失败";
      t.el.querySelector(".file-status").className = "file-status err";
      toast("写入失败：" + e);
      throw e;
    }
  }
}

async function onPhoneComplete(id) {
  const t = state.receiving.get(id);
  if (!t) return;
  try {
    await t.writeChain;
    if (t.writeError) throw t.writeError;
    await flushChunksToDisk(t);
    const finalPath = await invoke("finalize_receive", {
      id: t.id,
      name: t.name,
      size: t.size,
    });
    t.el.querySelector(".file-status").textContent = "已保存";
    t.el.querySelector(".file-status").className = "file-status ok";
    t.el.querySelector(".file-size").textContent = formatSize(t.size);
    // 传完就把进度条撤掉、图标改成完成态，这一行从此只表达结果
    t.el.querySelector(".progress")?.remove();
    t.el.querySelector(".file-icon").textContent = "✅";
    t.el.querySelector(".file-name").title = finalPath;
    t.el.dataset.path = finalPath;
    attachRevealButton(t.el, finalPath);
  } catch (e) {
    t.el.querySelector(".file-status").textContent = "保存失败";
    t.el.querySelector(".file-status").className = "file-status err";
    toast("保存失败：" + e);
  }
  state.receiving.delete(id);
}

// 给某个已接收的文件行挂上「在文件夹中显示」按钮（悬停时才可见）。
function attachRevealButton(itemEl, path) {
  const actions = document.createElement("span");
  actions.className = "file-actions";
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "icon-btn";
  btn.textContent = "📁";
  btn.title = "在文件夹中显示";
  btn.addEventListener("click", () => revealPath(path));
  actions.appendChild(btn);
  itemEl.insertBefore(actions, itemEl.querySelector(".file-status"));
}

// 交给后端去调起系统文件管理器；后端会校验路径确实在保存目录内。
async function revealPath(path) {
  try {
    await invoke("reveal_path", { path });
  } catch (e) {
    toast("打开文件夹失败：" + e);
  }
}

function createSendingItem(path) {
  const item = document.createElement("div");
  item.className = "file-item";
  item.dataset.path = path;
  item.dataset.sendState = "pending";
  item.innerHTML = `
    <span class="file-icon">📄</span>
    <span class="file-info">
      <div class="file-name"></div>
      <div class="file-size">待读取</div>
    </span>
    <span class="file-status">待发送</span>`;
  const name = path.split(/[\\/]/).pop() || path;
  item.querySelector(".file-name").textContent = name;
  item.querySelector(".file-name").title = path;
  $("sendList").appendChild(item);
  return item;
}

function estimateBytes(b64) { return Math.floor(b64.length * 3 / 4); }

// ---------- 桌面发送文件 ----------

function appendSendingFiles(paths) {
  const list = $("sendList");
  const empty = list.querySelector(".empty");
  if (empty) empty.remove();
  paths.forEach((p) => {
    if (Array.from(list.querySelectorAll(".file-item[data-path]"))
      .some((item) => item.dataset.path === p)) return;
    const item = document.createElement("div");
    item.className = "file-item";
    item.dataset.path = p;
    item.dataset.sendState = "pending";
    item.innerHTML = `
      <span class="file-icon">📄</span>
      <span class="file-info">
        <div class="file-name"></div>
        <div class="file-size">待读取</div>
      </span>
      <span class="file-status">待发送</span>`;
    const name = p.split(/[\\/]/).pop() || p;
    item.querySelector(".file-name").textContent = name;
    item.querySelector(".file-name").title = p;
    list.appendChild(item);
  });
  refreshSendButton();
}

async function registerDroppedFiles(paths) {
  try {
    const res = await invoke("register_files", { paths });
    if (!res.paths.length) {
      toast("未发现可发送的文件");
      return;
    }
    appendSendingFiles(res.paths);
    toast(`已添加 ${res.paths.length} 个文件`);
  } catch (e) {
    toast("添加文件失败：" + e);
  }
}

$("pickFilesBtn").addEventListener("click", async () => {
  try {
    const res = await invoke("pick_files");
    if (!res.paths.length) return;
    appendSendingFiles(res.paths);
    toast(`已选择 ${res.paths.length} 个文件`);
  } catch (e) {
    toast("选择文件失败：" + e);
  }
});

async function setupFileDrop() {
  const dropZone = $("fileDropZone");
  const eventApi = window.__TAURI__?.event;
  if (!dropZone || !eventApi?.listen) return;

  await eventApi.listen("tauri://drag-enter", () => {
    dropZone.classList.add("drag-over");
  });
  await eventApi.listen("tauri://drag-leave", () => {
    dropZone.classList.remove("drag-over");
  });
  await eventApi.listen("tauri://drag-drop", async (event) => {
    dropZone.classList.remove("drag-over");
    const paths = event.payload?.paths || [];
    if (paths.length) await registerDroppedFiles(paths);
  });
}

$("sendSelectedBtn").addEventListener("click", async () => {
  const items = Array.from($("sendList").querySelectorAll(".file-item[data-path][data-send-state=\"pending\"]"));
  if (!items.length) { toast("没有待发送文件"); return; }
  if (!state.connected) { toast("手机未连接"); return; }
  for (const el of items) {
    const path = el.dataset.path;
    el.querySelector(".file-status").textContent = "读取中…";
    try {
      await sendOneFile(el, path);
    } catch (e) {
      el.querySelector(".file-status").textContent = "失败";
      el.querySelector(".file-status").className = "file-status err";
      el.dataset.sendState = "failed";
      toast("发送失败：" + e);
    }
  }
  refreshSendButton();
});

async function sendOneFile(el, path) {
  el.dataset.sendState = "sending";
  const name = path.split(/[\\/]/).pop() || "file";
  const size = await invoke("get_file_size", { path });
  const id = "dl-" + Date.now() + "-" + Math.random().toString(36).slice(2, 8);
  state.sending.set(id, { name, size, sent: 0, el });

  el.querySelector(".file-info .file-size").textContent =
    `0 / ${formatSize(size)}`;
  el.querySelector(".file-status").textContent = "发送中";
  el.querySelector(".progress")?.remove();
  const progress = document.createElement("div");
  progress.className = "progress";
  progress.innerHTML = "<div></div>";
  el.querySelector(".file-info").appendChild(progress);

  enqueue({ type: "file-meta", id, name, size });
  await enqueueAndWait();

  let offset = 0;
  while (true) {
    const chunk = await invoke("read_file_chunk", { path, offset });
    enqueue({ type: "file-chunk", id, seq: Math.floor(offset / MAX_CHUNK), data: chunk.data });
    await enqueueAndWait();
    state.sending.get(id).sent = chunk.next_offset;
    const pct = Math.round((chunk.next_offset * 100) / Math.max(1, size));
    progress.querySelector("div").style.width = pct + "%";
    el.querySelector(".file-info .file-size").textContent =
      `${formatSize(chunk.next_offset)} / ${formatSize(size)}`;
    offset = chunk.next_offset;
    if (chunk.eof) break;
  }

  enqueue({ type: "file-complete", id });
  await enqueueAndWait();
  el.querySelector(".file-status").textContent = "等待手机确认…";
  setTimeout(() => {
    const item = state.sending.get(id);
    if (item && item.el.querySelector(".file-status").textContent === "等待手机确认…") {
      item.el.querySelector(".file-status").textContent = "已发送";
      item.el.querySelector(".file-status").className = "file-status ok";
      item.el.dataset.sendState = "sent";
    }
  }, 3000);
}

// ---------- 消息队列（串行发送，保证顺序） ----------

function enqueue(msg) {
  state.outbox.push(msg);
  flushQueue();
}

async function enqueueAndWait() {
  return new Promise((resolve, reject) => {
    state.outbox.push({ __wait: resolve, __reject: reject });
    flushQueue();
  });
}

async function flushQueue() {
  if (state.flushing) return;
  state.flushing = true;
  while (state.outbox.length) {
    const msg = state.outbox.shift();
    if (msg.__wait) {
      msg.__wait();
      continue;
    }
    try {
      await invoke("send_to_phone", { payload: msg });
    } catch (e) {
      toast("发送到手机失败：" + e);
      // 发送失败后剩余消息一律丢弃，但必须唤醒所有等待点，
      // 否则 sendOneFile / 实时同步会永久卡在 await enqueueAndWait()。
      while (state.outbox.length) {
        const left = state.outbox.shift();
        if (left.__reject) left.__reject(e instanceof Error ? e : new Error(String(e)));
      }
      break;
    }
  }
  state.flushing = false;
}

// ---------- 剪贴板 ----------

async function writeClipboard(text) {
  await invoke("plugin:clipboard-manager|write_text", { text });
}

$("refreshClipBtn").addEventListener("click", async () => {
  await loadClipboardToBox(false);
});

// 把系统剪贴板读进界面：图片显示预览，文字填进框。silent=true 时不弹 toast（启动自动读取用）
async function loadClipboardToBox(silent) {
  try {
    const payload = await invoke("get_clipboard_payload");
    state.clipboardPayload = payload;
    if (payload.kind === "image" && payload.image_preview) {
      $("clipPreviewImg").src = payload.image_preview;
      $("clipPreview").style.display = "block";
      $("clipText").value = "";
      if (!silent) toast("剪贴板是图片，已显示预览");
      return;
    }
    if (payload.kind === "files" && payload.files?.length) {
      hideClipPreview();
      $("clipText").value = "";
      if (!silent) toast(`剪贴板是 ${payload.files.length} 个文件，点「发送到手机」发送`);
      return;
    }
    hideClipPreview();
    $("clipText").value = payload.text || "";
    if (!silent) toast(payload.text ? "已读取剪贴板" : "剪贴板为空");
  } catch (e) {
    if (!silent) toast("读取剪贴板失败：" + e);
  }
}

function hideClipPreview() {
  $("clipPreview").style.display = "none";
  $("clipPreviewImg").removeAttribute("src");
}

$("sendClipBtn").addEventListener("click", async () => {
  if (!state.connected) { toast("手机未连接"); return; }
  await sendClipboardToPhone();
});

// Enter 发送，Shift+Enter 换行——按聊天框的习惯来
$("clipText").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (!state.connected) { toast("手机未连接"); return; }
    sendClipboardToPhone();
  }
});

function setSyncStatus(text) {
  const el = $("syncStatus");
  if (el) el.textContent = text || "";
}

// 打字自动同步：输入停顿 400ms 后把框里内容推给手机，实现「边打边到」
let liveSyncTimer = null;
function cancelLiveSync() {
  clearTimeout(liveSyncTimer);
  liveSyncTimer = null;
}
$("clipText").addEventListener("input", () => {
  hideClipPreview(); // 打字就是发文字，图片预览让位
  cancelLiveSync();
  if (!state.connected) {
    setSyncStatus("");
    return;
  }
  setSyncStatus("输入中…");
  liveSyncTimer = setTimeout(async () => {
    const text = $("clipText").value;
    if (!text.trim()) {
      setSyncStatus("");
      return; // 空内容不推，避免清空手机显示
    }
    try {
      enqueue({ type: "clipboard", text });
      await enqueueAndWait();
      const t = new Date();
      setSyncStatus("已同步 " + t.getHours().toString().padStart(2, "0") + ":" + t.getMinutes().toString().padStart(2, "0") + ":" + t.getSeconds().toString().padStart(2, "0"));
    } catch {
      setSyncStatus("同步失败");
    }
  }, 400);
});

async function sendClipboardToPhone() {
  cancelLiveSync(); // 手动发送后绝不能再被 400ms 防抖补发一次
  if (!state.connected) { toast("手机未连接，请先扫码连接"); return; }
  // 框里有内容就直接发框里的（用户可能直接在此输入，而非系统剪贴板）；
  // 框为空才回退剪贴板。
  const boxText = $("clipText").value;
  if (boxText.trim()) {
    try {
      enqueue({ type: "clipboard", text: boxText });
      await enqueueAndWait();
      setSyncStatus("已发送 " + new Date().toLocaleTimeString());
      toast("已发送到手机");
    } catch {
      setSyncStatus("发送失败");
    }
    return;
  }

  // 预览正显示着图片时，发的就是预览那一份；否则才重读系统剪贴板。
  const previewOn = $("clipPreview").style.display === "block";
  let payload;
  if (previewOn && state.clipboardPayload?.files?.length) {
    payload = state.clipboardPayload;
  } else {
    try {
      payload = await invoke("get_clipboard_payload");
    } catch (e) {
      toast("读取剪贴板失败：" + e);
      return;
    }
    state.clipboardPayload = payload;
  }

  if (payload.files?.length) {
    for (const path of payload.files) {
      const item = createSendingItem(path);
      try {
        await sendOneFile(item, path);
      } catch (e) {
        item.querySelector(".file-status").textContent = "失败";
        item.querySelector(".file-status").className = "file-status err";
        item.dataset.sendState = "failed";
        toast("发送失败：" + e);
      }
    }
    toast(payload.kind === "image" ? "已发送剪贴板图片" : `已发送 ${payload.files.length} 个剪贴板文件`);
    hideClipPreview();
    state.clipboardPayload = null;
    return;
  }

  const text = payload.text || "";
  if (!text) { toast("剪贴板为空：先在框里输入，或到别处 Ctrl+C 后再发"); return; }
  try {
    enqueue({ type: "clipboard", text });
    await enqueueAndWait();
    setSyncStatus("已发送系统剪贴板 " + new Date().toLocaleTimeString());
    toast("已发送到手机");
  } catch {
    setSyncStatus("发送失败");
  }
}

function showReceivedClipboard(text) {
  $("phoneClipText").textContent = text || "（空）";
  $("copyClipBtn").style.display = text ? "inline-block" : "none";
}

$("copyClipBtn").addEventListener("click", async () => {
  const text = $("phoneClipText").textContent;
  try {
    await writeClipboard(text);
    toast("已写入电脑剪贴板");
  } catch (e) {
    toast("写入失败：" + e);
  }
});

// ---------- 目录与日志 ----------

$("openDirBtn").addEventListener("click", () => {
  const dir = state.info?.download_dir;
  if (!dir) {
    toast("保存目录尚未就绪");
    return;
  }
  revealPath(dir);
});

$("changeDirBtn").addEventListener("click", async () => {
  try {
    const dir = await invoke("pick_download_dir");
    $("saveDir").textContent = dir;
    if (state.info) state.info.download_dir = dir;
    toast("保存目录已更新");
  } catch (e) {
    toast("更新目录失败：" + e);
  }
});

$("refreshLogBtn").addEventListener("click", async () => {
  try {
    const logs = await invoke("get_recent_logs");
    $("logText").textContent = logs.join("\n") || "暂无日志";
  } catch { /* 忽略 */ }
});

$("rotateBtn").addEventListener("click", async () => {
  try {
    await invoke("rotate_session");
    await refreshInfo();
    setConnected(false);
    toast("已生成新会话，旧二维码失效");
  } catch (e) {
    toast("操作失败：" + e);
  }
});

// ---------- 事件监听 ----------

async function init() {
  await refreshInfo();
  setConnected(false);
  try {
    await listen("phone://message", (e) => onPhoneMessage(e.payload));
    await listen("phone://status", (e) => setConnected(!!e.payload));
    await setupFileDrop();
  } catch (err) {
    toast("事件监听初始化失败：" + err);
  }
  // 启动时自动读一次系统剪贴板填入，省去手点「读取剪贴板」
  try {
    await loadClipboardToBox(true);
  } catch { /* 忽略 */ }
  // 定时刷新会话信息（IP 变化时更新二维码）
  setInterval(refreshInfo, 30000);
}

init();
