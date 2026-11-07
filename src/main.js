import qrcode from "./qrcode.js";

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const $ = (id) => document.getElementById(id);

const state = {
  info: null,
  devices: [],             // 在线设备 [{id, name, ip}]
  selectedIds: new Set(),  // 用户勾选的发送目标
  maxDevices: 5,
  // 键为 `${deviceId}:${transferId}`，同一文件发多台设备互不覆盖
  sending: new Map(),
  // 键为 `${deviceId}:${手机端 id}`，跨设备同名 id 不冲突
  receiving: new Map(),
  queues: new Map(),       // deviceId -> {items: [], flushing: false}
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

// 只有地址/端口/会话真的变了才重绘二维码，避免定时刷新时整张图闪烁。
let lastQrKey = "";

async function refreshInfo() {
  try {
    const info = await invoke("get_session_info");
    state.info = info;
    $("serverAddr").textContent = `${info.ip}:${info.port}`;
    $("sessionId").textContent = info.session_id;
    $("saveDir").textContent = info.download_dir;
    setDevices({ devices: info.devices || [], count: info.devices?.length || 0, max: info.max_devices || 5 });
    $("qrHint").textContent =
      `手机扫码后打开网页并自动连接。请确保手机与电脑在同一局域网。若无法连接，请检查防火墙放行 ${info.port} 端口。`;
    const key = `${info.ip}:${info.port}:${info.session_id}`;
    if (key !== lastQrKey) {
      lastQrKey = key;
      drawQr(`http://${info.ip}:${info.port}/?session=${encodeURIComponent(info.session_id)}`);
    }
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

function setDevices(list) {
  const prev = new Map(state.devices.map((d) => [d.id, d]));
  state.devices = list.devices || [];
  state.maxDevices = list.max || 5;

  const online = new Set(state.devices.map((d) => d.id));
  // 设备断开后，它名下正在接收的文件永远等不到完成消息，必须就地收尾。
  for (const t of Array.from(state.receiving.values())) {
    if (!online.has(t.deviceId)) abortReceiving("设备已断开，传输中断", t.deviceId);
  }
  // 收紧勾选：离线设备自动取消
  for (const id of Array.from(state.selectedIds)) {
    if (!online.has(id)) state.selectedIds.delete(id);
  }
  // 只有「新设备接入且当前没有任何勾选」才自动勾选它；
  // 用户主动取消全部勾选后，定时刷新不会再把它们勾回来。
  const fresh = state.devices.filter((d) => !prev.has(d.id));
  if (fresh.length && !state.selectedIds.size) {
    fresh.forEach((d) => state.selectedIds.add(d.id));
  }

  const n = state.devices.length;
  const pill = $("statusPill");
  pill.classList.toggle("online", n > 0);
  pill.classList.toggle("offline", n === 0);
  pill.querySelector(".status-label").textContent =
    n > 0 ? `已连接 ${n}/${state.maxDevices}` : "等待扫码连接";
  // 按钮保持可点：未连接时由点击 handler 弹 toast 提示，
  // 避免 disabled 按钮“点了毫无反应”的静默失败。
  $("sendSelectedBtn").classList.toggle("btn-dim", n === 0);
  $("sendClipBtn").classList.toggle("btn-dim", n === 0);
  if (n === 0) {
    // 断开后别再让防抖把内容推给“已经不在的手机”
    if (typeof cancelLiveSync === "function") cancelLiveSync();
    setSyncStatus("");
  }
  renderDeviceList();
  refreshSendButton();
}

function renderDeviceList() {
  const box = $("deviceList");
  if (!box) return;
  $("deviceCount").textContent = `${state.devices.length}/${state.maxDevices}`;
  box.innerHTML = "";
  if (!state.devices.length) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "等待扫码连接";
    box.appendChild(empty);
    return;
  }
  for (const d of state.devices) {
    const row = document.createElement("div");
    row.className = "device-item";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = state.selectedIds.has(d.id);
    cb.dataset.deviceId = String(d.id);
    cb.title = "勾选后发送给该设备";
    const name = document.createElement("span");
    name.className = "device-name";
    name.textContent = d.name;
    const ipEl = document.createElement("span");
    ipEl.className = "device-ip";
    ipEl.textContent = d.ip || "";
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "secondary small";
    btn.textContent = "断开";
    btn.dataset.disconnectId = String(d.id);
    row.append(cb, name, ipEl, btn);
    box.appendChild(row);
  }
}

$("deviceList").addEventListener("change", (e) => {
  const id = Number(e.target.dataset.deviceId);
  if (!id) return;
  if (e.target.checked) state.selectedIds.add(id);
  else state.selectedIds.delete(id);
});

$("deviceList").addEventListener("click", async (e) => {
  const id = Number(e.target.dataset.disconnectId);
  if (!id) return;
  try {
    await invoke("disconnect_device", { deviceId: id });
    toast("已断开该设备");
  } catch (err) {
    toast("断开失败：" + err);
  }
});

function deviceLabel(id) {
  return state.devices.find((d) => d.id === id)?.name || `设备 ${id}`;
}

function selectedDeviceIds() {
  const online = new Set(state.devices.map((d) => d.id));
  return Array.from(state.selectedIds).filter((id) => online.has(id));
}

// 未勾选任何设备时不默认广播，明确提示用户选择
function requireTargets(explicit) {
  const targets = explicit || selectedDeviceIds();
  if (!targets.length) {
    toast("请先在左侧勾选要发送的设备");
    return null;
  }
  return targets;
}

function refreshSendButton() {
  const list = $("sendList");
  const hasPending = !!list.querySelector('.file-item[data-path][data-send-state="pending"]');
  $("sendSelectedBtn").disabled = !hasPending;
}

// ---------- 手机消息处理 ----------

async function onPhoneMessage(raw) {
  let env;
  try { env = JSON.parse(raw); } catch { return; }
  // 后端统一包一层信封：{deviceId, deviceName, ip, message}
  const deviceId = env.deviceId;
  const deviceName = env.deviceName || deviceLabel(deviceId);
  const msg = env.message || env;
  if (deviceId == null) return;
  switch (msg.type) {
    case "file-meta":
      addReceivingItem(deviceId, deviceName, msg.id, msg.name, msg.size);
      break;
    case "file-chunk":
      await onPhoneChunk(deviceId, msg);
      break;
    case "file-complete":
      await onPhoneComplete(deviceId, deviceName, msg.id);
      break;
    case "file-ack": {
      const key = `${deviceId}:${msg.id}`;
      const item = state.sending.get(key);
      if (item) {
        const ok = msg.status === "ok";
        setTargetStatus(item.el, deviceId, ok ? "已送达" : `手机接收失败${msg.message ? "：" + msg.message : ""}`, ok ? "ok" : "err");
        state.sending.delete(key);
      }
      break;
    }
    case "clipboard":
      showReceivedClipboard(msg.text, deviceName);
      break;
    case "clipboard-request":
      // 只回请求方，不要顺手发给其他勾选设备
      await sendClipboardToPhone([deviceId]);
      break;
    case "error":
      toast(`${deviceName} 错误：` + (msg.message || "未知"));
      break;
  }
}

const receiveKey = (deviceId, id) => `${deviceId}:${id}`;

// 设备断开或换会话时收尾接收中的行，避免永远停在「接收中」。
function abortReceiving(text, deviceId = null) {
  for (const [key, t] of Array.from(state.receiving)) {
    if (deviceId != null && t.deviceId !== deviceId) continue;
    const status = t.el?.querySelector(".file-status");
    if (status) {
      status.textContent = text;
      status.className = "file-status err";
    }
    t.el?.querySelector(".progress")?.remove();
    state.receiving.delete(key);
  }
}

function addReceivingItem(deviceId, deviceName, id, name, size) {
  // 传给 Rust 的 id 加设备前缀，避免两台手机同时传同名 id 互相覆盖
  const rustId = `d${deviceId}-${id}`;
  state.receiving.set(receiveKey(deviceId, id), {
    deviceId, id: rustId, name, size, received: 0, chunks: [], el: null,
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
      <div class="file-source"></div>
      <div class="file-size">0 / ${escapeHtml(formatSize(size))}</div>
      <div class="progress"><div></div></div>
    </span>
    <span class="file-status">接收中</span>`;
  item.querySelector(".file-name").textContent = name;
  item.querySelector(".file-source").textContent = `来自 ${deviceName}`;
  list.appendChild(item);
  state.receiving.get(receiveKey(deviceId, id)).el = item;
}

async function onPhoneChunk(deviceId, msg) {
  const t = state.receiving.get(receiveKey(deviceId, msg.id));
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
        deviceId: t.deviceId,
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

// 把接收结果告知手机，让手机端不再「发完就显示完成」。
async function sendFileAck(deviceId, id, status, message) {
  try {
    await invoke("send_to_phone", {
      deviceId,
      payload: { type: "file-ack", id, status, ...(message ? { message } : {}) },
    });
  } catch {
    // 手机可能已经断开，这里静默忽略即可
  }
}

async function onPhoneComplete(deviceId, deviceName, id) {
  const key = receiveKey(deviceId, id);
  const t = state.receiving.get(key);
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
    await sendFileAck(deviceId, id, "ok");
  } catch (e) {
    t.el.querySelector(".file-status").textContent = "保存失败";
    t.el.querySelector(".file-status").className = "file-status err";
    toast("保存失败：" + e);
    await sendFileAck(deviceId, id, "error", String(e));
  }
  state.receiving.delete(key);
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
  const targets = requireTargets();
  if (!targets) return;
  for (const el of items) {
    const path = el.dataset.path;
    el.dataset.sendState = "sending";
    el.querySelector(".file-status").textContent = "";
    let failed = false;
    // 外层按设备、内层按文件串行：同一台设备内的分块顺序不能乱
    for (const deviceId of targets) {
      try {
        await sendOneFile(el, path, deviceId);
      } catch (e) {
        setTargetStatus(el, deviceId, "失败：" + e, "err");
        failed = true;
      }
    }
    el.dataset.sendState = failed ? "failed" : "sent";
  }
  refreshSendButton();
});

// 在文件行里为每台目标设备单独显示一行进度/结果，避免多设备互相覆盖
function targetStatusEl(el, deviceId) {
  let box = el.querySelector(".send-targets");
  if (!box) {
    box = document.createElement("div");
    box.className = "send-targets";
    el.querySelector(".file-info").appendChild(box);
  }
  let s = box.querySelector(`[data-device="${deviceId}"]`);
  if (!s) {
    s = document.createElement("div");
    s.className = "target-status";
    s.dataset.device = String(deviceId);
    box.appendChild(s);
  }
  return s;
}

function setTargetStatus(el, deviceId, text, cls) {
  const s = targetStatusEl(el, deviceId);
  s.textContent = `${deviceLabel(deviceId)}：${text}`;
  s.className = "target-status" + (cls ? " " + cls : "");
}

async function sendOneFile(el, path, deviceId) {
  const name = path.split(/[\\/]/).pop() || "file";
  const size = await invoke("get_file_size", { path });
  const id = "dl-" + Date.now() + "-" + Math.random().toString(36).slice(2, 8);
  const key = `${deviceId}:${id}`;
  state.sending.set(key, { id, deviceId, name, size, sent: 0, el });

  const info = el.querySelector(".file-info");
  const sizeEl = info.querySelector(".file-size");
  let progress = info.querySelector(".progress");
  if (!progress) {
    progress = document.createElement("div");
    progress.className = "progress";
    progress.innerHTML = "<div></div>";
    info.appendChild(progress);
  }
  const bar = progress.querySelector("div");
  sizeEl.textContent = formatSize(size);
  setTargetStatus(el, deviceId, "发送中…");

  try {
    enqueue(deviceId, { type: "file-meta", id, name, size });
    await enqueueAndWait(deviceId);

    let offset = 0;
    while (true) {
      const chunk = await invoke("read_file_chunk", { path, offset });
      enqueue(deviceId, { type: "file-chunk", id, seq: Math.floor(offset / MAX_CHUNK), data: chunk.data });
      await enqueueAndWait(deviceId);
      state.sending.get(key).sent = chunk.next_offset;
      const pct = Math.round((chunk.next_offset * 100) / Math.max(1, size));
      bar.style.width = pct + "%";
      setTargetStatus(el, deviceId, `已发送 ${pct}%`);
      offset = chunk.next_offset;
      if (chunk.eof) break;
    }

    enqueue(deviceId, { type: "file-complete", id });
    await enqueueAndWait(deviceId);
    // 不再用定时器假装成功：只有手机回 file-ack 才标「已送达」
    setTargetStatus(el, deviceId, "等待手机确认…");
  } catch (e) {
    // 发送失败时清掉这条记录，避免 state.sending 长期引用已结束的 DOM。
    state.sending.delete(key);
    throw e;
  }

  // 手机中途消失可能永远不回 file-ack：超时后收起记录，避免无限堆积。
  setTimeout(() => {
    if (state.sending.delete(key)) {
      setTargetStatus(el, deviceId, "未收到手机确认", "err");
    }
  }, 60000);
}

// ---------- 消息队列（每台设备独立串行，互不拖累） ----------

function queueFor(deviceId) {
  let q = state.queues.get(deviceId);
  if (!q) {
    q = { items: [], flushing: false };
    state.queues.set(deviceId, q);
  }
  return q;
}

function enqueue(deviceId, msg) {
  queueFor(deviceId).items.push(msg);
  flushQueue(deviceId);
}

async function enqueueAndWait(deviceId) {
  return new Promise((resolve, reject) => {
    queueFor(deviceId).items.push({ __wait: resolve, __reject: reject });
    flushQueue(deviceId);
  });
}

async function flushQueue(deviceId) {
  const q = queueFor(deviceId);
  if (q.flushing) return;
  q.flushing = true;
  while (q.items.length) {
    const msg = q.items.shift();
    if (msg.__wait) {
      msg.__wait();
      continue;
    }
    try {
      await invoke("send_to_phone", { deviceId, payload: msg });
    } catch (e) {
      toast(`发送到 ${deviceLabel(deviceId)} 失败：` + e);
      // 只丢弃该设备的剩余消息，并唤醒等待点，避免 sendOneFile 永久卡住
      while (q.items.length) {
        const left = q.items.shift();
        if (left.__reject) left.__reject(e instanceof Error ? e : new Error(String(e)));
      }
      break;
    }
  }
  q.flushing = false;
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
  await sendClipboardToPhone();
});

// Enter 发送，Shift+Enter 换行——按聊天框的习惯来
$("clipText").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
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
  const targets = selectedDeviceIds();
  if (!targets.length) {
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
      for (const deviceId of targets) {
        enqueue(deviceId, { type: "clipboard", text });
        await enqueueAndWait(deviceId);
      }
      const t = new Date();
      setSyncStatus("已同步 " + t.getHours().toString().padStart(2, "0") + ":" + t.getMinutes().toString().padStart(2, "0") + ":" + t.getSeconds().toString().padStart(2, "0"));
    } catch {
      setSyncStatus("同步失败");
    }
  }, 400);
});

// explicitTargets：手机主动请求剪贴板时只回到那台设备
async function sendClipboardToPhone(explicitTargets) {
  cancelLiveSync(); // 手动发送后绝不能再被 400ms 防抖补发一次
  const targets = requireTargets(explicitTargets);
  if (!targets) return;
  // 框里有内容就直接发框里的（用户可能直接在此输入，而非系统剪贴板）；
  // 框为空才回退剪贴板。
  const boxText = $("clipText").value;
  if (boxText.trim()) {
    try {
      for (const deviceId of targets) {
        enqueue(deviceId, { type: "clipboard", text: boxText });
        await enqueueAndWait(deviceId);
      }
      setSyncStatus("已发送 " + new Date().toLocaleTimeString());
      toast("已发送到所选设备");
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
      item.querySelector(".file-status").textContent = "";
      for (const deviceId of targets) {
        try {
          await sendOneFile(item, path, deviceId);
        } catch (e) {
          setTargetStatus(item, deviceId, "失败：" + e, "err");
          toast(`发送到 ${deviceLabel(deviceId)} 失败：` + e);
        }
      }
      item.dataset.sendState = "sent";
    }
    toast(payload.kind === "image" ? "已发送剪贴板图片" : `已发送 ${payload.files.length} 个剪贴板文件`);
    hideClipPreview();
    state.clipboardPayload = null;
    return;
  }

  const text = payload.text || "";
  if (!text) { toast("剪贴板为空：先在框里输入，或到别处 Ctrl+C 后再发"); return; }
  try {
    for (const deviceId of targets) {
      enqueue(deviceId, { type: "clipboard", text });
      await enqueueAndWait(deviceId);
    }
    setSyncStatus("已发送系统剪贴板 " + new Date().toLocaleTimeString());
    toast("已发送到所选设备");
  } catch {
    setSyncStatus("发送失败");
  }
}

function showReceivedClipboard(text, source) {
  $("phoneClipText").textContent = text || "（空）";
  $("phoneClipSource").textContent = source ? `来自 ${source}` : "";
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
    const logs = await invoke("get_recent_logs", { limit: 200 });
    $("logText").textContent = logs.join("\n") || "暂无日志";
  } catch { /* 忽略 */ }
});

$("rotateBtn").addEventListener("click", async () => {
  try {
    // 先收尾接收中的行，避免随后设备列表清空时显示成「设备已断开」
    abortReceiving("会话已更换，传输中断");
    await invoke("rotate_session");
    await refreshInfo();
    // 旧会话建立的传输全部作废：清队列、清勾选，并给未确认的发送行一个明确结果
    state.queues.clear();
    state.selectedIds.clear();
    for (const item of state.sending.values()) {
      setTargetStatus(item.el, item.deviceId, "会话已更换，传输中断", "err");
    }
    state.sending.clear();
    setDevices({ devices: [], count: 0, max: state.maxDevices });
    toast("已生成新会话，旧二维码失效");
  } catch (e) {
    toast("操作失败：" + e);
  }
});

// ---------- 事件监听 ----------

async function init() {
  await refreshInfo();
  setDevices({ devices: [], count: 0, max: state.maxDevices });
  try {
    await listen("phone://message", (e) => onPhoneMessage(e.payload));
    await listen("phone://status", (e) => {
      const p = e.payload;
      if (p && typeof p === "object" && Array.isArray(p.devices)) setDevices(p);
    });
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
