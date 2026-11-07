(function () {
  "use strict";

  var $ = function (id) { return document.getElementById(id); };
  var statusEl = $("status");
  var serverInfoEl = $("serverInfo");
  var pickBtn = $("pickBtn");
  var fileInput = $("fileInput");
  var pickPhotoBtn = $("pickPhotoBtn");
  var photoInput = $("photoInput");
  var uploadList = $("uploadList");
  var progressList = $("progressList");
  var downloadList = $("downloadList");
  var clipArea = $("clipArea");
  var sendFilesBtn = $("sendFilesBtn");
  var clearFilesBtn = $("clearFilesBtn");
  var pendingFiles = [];
  var scannerStream = null;
  var scannerTimer = null;

  var params = new URLSearchParams(location.search);
  var session = params.get("session") || "";
  var ws = null;
  var reconnectDelay = 1000;
  var heartbeatTimer = null;
  var probeTimer = null;
  var reconnectTimer = null;
  var lastPong = 0;
  // 达到设备上限 / 被新页面替换时停止重连，否则会被拒后无限重试
  var fatal = false;
  var MAX_CHUNK = 512 * 1024;
  var transferProgress = {};   // id -> {name, size, received, parts: []}
  var uploadProgress = {};     // id -> {name, size, sent, itemEl, barEl, textEl}

  function setStatus(text, ok) {
    statusEl.textContent = text;
    statusEl.classList.toggle("ok", !!ok);
    if (typeof renderQueue === "function") renderQueue();
  }

  // 设备名只用于电脑端展示，从 UA 推断，识别不出时回退通用名
  function deviceName() {
    var ua = navigator.userAgent || "";
    if (/iPad/i.test(ua) || (/Macintosh/i.test(ua) && navigator.maxTouchPoints > 1)) return "iPad";
    if (/iPhone/i.test(ua)) return "iPhone";
    var m = ua.match(/Android[^;)]*;\s*([^;)]+?)\s*(?:Build\/|\))/i);
    if (m && m[1] && m[1].length <= 24) return "Android " + m[1].trim();
    if (/Android/i.test(ua)) return "Android 设备";
    if (/Windows/i.test(ua)) return "Windows 浏览器";
    if (/Macintosh|Mac OS X/i.test(ua)) return "Mac 浏览器";
    return "手机设备";
  }

  function connect() {
    if (fatal) return;
    // 立刻重连时取消尚未触发的退避计时器，避免同时开出两条连接。
    if (reconnectTimer) clearTimeout(reconnectTimer);
    reconnectTimer = null;
    var proto = location.protocol === "https:" ? "wss://" : "ws://";
    var url = proto + location.host + "/ws?session=" + encodeURIComponent(session);
    var socket = new WebSocket(url);
    ws = socket;

    // 旧 socket 的 close/error 可能在新连接建立后才到达，用实例比对避免误判。
    socket.onopen = function () {
      if (ws !== socket) return;
      setStatus("已连接", true);
      reconnectDelay = 1000;
      lastPong = Date.now();
      startHeartbeat();
      renderQueue();
      send({ type: "hello", session: session, name: deviceName() });
    };

    socket.onmessage = function (ev) {
      if (ws !== socket) return;
      var msg;
      try { msg = JSON.parse(ev.data); } catch (e) { return; }
      handleMessage(msg);
    };

    socket.onclose = function () {
      if (ws !== socket) return;
      stopHeartbeat();
      if (fatal) return;
      setStatus("连接断开，重连中…", false);
      renderQueue();
      scheduleReconnect();
    };

    socket.onerror = function () {
      try { socket.close(); } catch (e) {}
    };
  }

  function scheduleReconnect() {
    if (fatal) return;
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) return;
    if (reconnectTimer) clearTimeout(reconnectTimer);
    reconnectTimer = setTimeout(function () {
      reconnectTimer = null;
      connect();
    }, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 15000);
  }

  // 熄屏/切后台时页面会被冻结，连接可能表面还开着、实际已经死了。
  // 每 20 秒发一次应用层 ping，60 秒没有 pong 就主动重连。
  function startHeartbeat() {
    stopHeartbeat();
    heartbeatTimer = setInterval(function () {
      if (!isConnected()) return;
      if (Date.now() - lastPong > 60000) {
        forceReconnect();
        return;
      }
      send({ type: "ping" });
    }, 20000);
  }

  function stopHeartbeat() {
    if (heartbeatTimer) clearInterval(heartbeatTimer);
    heartbeatTimer = null;
    if (probeTimer) clearTimeout(probeTimer);
    probeTimer = null;
  }

  function forceReconnect() {
    if (!session || fatal) return;
    stopHeartbeat();
    var old = ws;
    ws = null;
    if (old) { try { old.close(); } catch (e) {} }
    reconnectDelay = 1000;
    connect();
  }

  // 从熄屏/后台回来：没连上就立刻重连；还连着就先探测，5 秒无响应再重连。
  function handleResume() {
    if (document.hidden || !session || fatal) return;
    if (ws && ws.readyState === WebSocket.CONNECTING) return;
    if (!isConnected()) {
      forceReconnect();
      return;
    }
    lastPong = Date.now();
    if (!send({ type: "ping" })) {
      forceReconnect();
      return;
    }
    if (probeTimer) clearTimeout(probeTimer);
    probeTimer = setTimeout(function () {
      probeTimer = null;
      if (Date.now() - lastPong >= 5000) forceReconnect();
    }, 5000);
  }

  document.addEventListener("visibilitychange", handleResume);
  window.addEventListener("focus", handleResume);
  window.addEventListener("online", handleResume);

  function send(obj) {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(obj));
      return true;
    }
    return false;
  }

  function handleMessage(msg) {
    switch (msg.type) {
      case "hello-ack":
        serverInfoEl.textContent = "已连接到 " + (msg.name || "文件互传");
        break;
      case "ping":
        send({ type: "pong" });
        break;
      case "file-meta":
        transferProgress[msg.id] = { name: msg.name, size: msg.size, received: 0, parts: [] };
        break;
      case "file-chunk":
        var t = transferProgress[msg.id];
        if (t) {
          t.parts.push(msg.data);
          t.received += estimateBytes(msg.data);
        }
        break;
      case "file-complete":
        finishDownload(msg.id);
        break;
      case "file-ack":
        handleUploadAck(msg);
        break;
      case "clipboard":
        showClipboard(msg.text);
        break;
      case "pong":
        lastPong = Date.now();
        if (probeTimer) {
          clearTimeout(probeTimer);
          probeTimer = null;
        }
        break;
      case "error":
        if (msg.code === "device-cap" || msg.code === "device-replaced" || msg.code === "session-expired") {
          enterFatal(msg.message || (msg.code === "device-cap"
            ? "电脑端已连接满 5 台设备，请先在电脑端断开一台"
            : msg.code === "session-expired"
              ? "会话已失效，请重新扫码"
              : "同一设备已在新页面连接，此页面已停止重连"));
        } else {
          setStatus(msg.message || "发生错误", false);
        }
        break;
    }
  }

  // 终止态：不再重连，避免被拒后每秒重试刷屏
  function enterFatal(text) {
    fatal = true;
    stopHeartbeat();
    if (reconnectTimer) clearTimeout(reconnectTimer);
    reconnectTimer = null;
    var old = ws;
    ws = null;
    if (old) { try { old.close(); } catch (e) {} }
    setStatus(text, false);
    showToast(text);
  }

  function estimateBytes(b64) {
    return Math.floor(b64.length * 3 / 4);
  }

  function finishDownload(id) {
    var t = transferProgress[id];
    if (!t) return;
    // 按分块转成多个 ArrayBuffer，避免拼成一条超大二进制字符串把手机内存打爆
    var buffers = [];
    var total = 0;
    try {
      for (var i = 0; i < t.parts.length; i++) {
        var bin = atob(t.parts[i]);
        var arr = new Uint8Array(bin.length);
        for (var j = 0; j < bin.length; j++) arr[j] = bin.charCodeAt(j);
        buffers.push(arr);
        total += arr.length;
      }
    } catch (e) {
      showToast("文件解码失败");
      send({ type: "file-ack", id: id, status: "error", message: "手机端解码失败" });
      delete transferProgress[id];
      return;
    }
    var name = t.name || "received-file";
    var blob = new Blob(buffers, { type: guessMime(name) });
    var url = URL.createObjectURL(blob);
    var a = document.createElement("a");
    a.href = url;
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(function () { URL.revokeObjectURL(url); }, 10000);

    addDownloadItem(name, total);
    delete transferProgress[id];
    send({ type: "file-ack", id: id, status: "ok" });
  }

  // 电脑确认接收结果：成功才真正标成完成，失败保留记录并提示原因
  function handleUploadAck(msg) {
    var p = uploadProgress[msg.id];
    if (!p) return;
    if (p.ackTimer) {
      clearTimeout(p.ackTimer);
      p.ackTimer = null;
    }
    if (msg.status === "ok") {
      p.textEl.textContent = "已保存";
      if (p.barEl.firstChild) p.barEl.firstChild.style.width = "100%";
      setTimeout(function () {
        dropUploadRow(msg.id);
      }, 1200);
    } else {
      // 失败原因用 toast 说清，行只短暂停留，避免进度区越堆越多
      p.textEl.textContent = "保存失败";
      showToast(msg.message || "电脑保存失败");
      setTimeout(function () {
        dropUploadRow(msg.id);
      }, 4000);
    }
  }

  function dropUploadRow(id) {
    var p = uploadProgress[id];
    if (!p) return;
    if (p.ackTimer) {
      clearTimeout(p.ackTimer);
      p.ackTimer = null;
    }
    p.barEl.remove();
    p.itemEl.remove();
    delete uploadProgress[id];
  }

  // 按扩展名给 MIME，让手机浏览器对图片/音视频/ PDF 走预览而不是纯下载
  function guessMime(name) {
    var lower = String(name || "").toLowerCase();
    if (/\.png$/.test(lower)) return "image/png";
    if (/\.jpe?g$/.test(lower)) return "image/jpeg";
    if (/\.gif$/.test(lower)) return "image/gif";
    if (/\.webp$/.test(lower)) return "image/webp";
    if (/\.bmp$/.test(lower)) return "image/bmp";
    if (/\.svg$/.test(lower)) return "image/svg+xml";
    if (/\.pdf$/.test(lower)) return "application/pdf";
    if (/\.mp4$/.test(lower)) return "video/mp4";
    if (/\.webm$/.test(lower)) return "video/webm";
    if (/\.mp3$/.test(lower)) return "audio/mpeg";
    if (/\.m4a$/.test(lower)) return "audio/mp4";
    if (/\.txt$/.test(lower)) return "text/plain";
    return "application/octet-stream";
  }

  function addDownloadItem(name, size) {
    var empty = downloadList.querySelector(".empty");
    if (empty) empty.remove();
    var item = document.createElement("div");
    item.className = "file-item";
    var sizeText = formatSize(size);
    item.innerHTML = '<span class="file-name">' + escapeHtml(name) + '</span><span class="muted">' + sizeText + "</span>";
    downloadList.appendChild(item);
  }

  function showClipboard(text) {
    if (!text) {
      clipArea.innerHTML = '<div class="empty">电脑剪贴板为空</div>';
      return;
    }
    clipArea.innerHTML =
      '<div class="clip-text"></div><div class="row"><button class="ghost" id="copyBtn">复制到本机剪贴板</button></div>';
    clipArea.querySelector(".clip-text").textContent = text;
    clipArea.querySelector("#copyBtn").addEventListener("click", function () {
      copyText(text);
    });
  }

  function copyText(text) {
    var done = function () { showToast("已复制"); };
    var fail = function () {
      var ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      try { document.execCommand("copy"); done(); } catch (e) { showToast("复制失败"); }
      ta.remove();
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, fail);
    } else {
      fail();
    }
  }

  function queueFile(file) {
    pendingFiles.push(file);
    renderQueue();
  }

  function renderQueue() {
    uploadList.innerHTML = "";
    pendingFiles.forEach(function (file) {
      var item = document.createElement("div");
      item.className = "file-item";
      item.innerHTML = '<span class="file-name"></span><span class="muted"></span>';
      item.querySelector(".file-name").textContent = file.name;
      item.querySelector(".muted").textContent = formatSize(file.size);
      uploadList.appendChild(item);
    });
    sendFilesBtn.disabled = !pendingFiles.length || !isConnected();
    clearFilesBtn.disabled = !pendingFiles.length;
    $("pickHint").textContent = pendingFiles.length ? "队列中 " + pendingFiles.length + " 个文件" : "可分批添加文件";
  }

  function isConnected() {
    return !!(ws && ws.readyState === WebSocket.OPEN);
  }

  async function uploadFile(file) {
    var id = "up-" + Date.now() + "-" + Math.random().toString(36).slice(2, 8);
    uploadProgress[id] = {
      name: file.name,
      size: file.size,
      sent: 0,
      itemEl: document.createElement("div"),
      barEl: document.createElement("div"),
      textEl: document.createElement("span")
    };
    var p = uploadProgress[id];
    p.itemEl.className = "file-item";
    p.itemEl.innerHTML = '<span class="file-name"></span><span class="muted"></span>';
    p.itemEl.querySelector(".file-name").textContent = file.name;
    p.itemEl.querySelector(".muted").appendChild(p.textEl);
    // 进度区独立于待发送队列，避免 renderQueue 重绘时把进度条一起清掉
    progressList.appendChild(p.itemEl);
    p.barEl.className = "progress";
    p.barEl.innerHTML = "<div></div>";
    progressList.appendChild(p.barEl);

    if (!send({ type: "file-meta", id: id, name: file.name, size: file.size })) {
      p.textEl.textContent = "连接断开";
      setTimeout(function () { dropUploadRow(id); }, 1000);
      return false;
    }
    return sendChunks(id, file, 0);
  }

  // 返回 false 表示传输中断（连接断开或读文件失败），调用方据此重新排队
  function sendChunks(id, file, offset) {
    if (offset >= file.size) {
      var p = uploadProgress[id];
      if (!send({ type: "file-complete", id: id })) {
        if (p) p.textEl.textContent = "连接断开";
        setTimeout(function () { dropUploadRow(id); }, 1000);
        return Promise.resolve(false);
      }
      if (p) {
        // 真正的完成状态以电脑回传的 file-ack 为准，避免「发完就显示成功」
        p.textEl.textContent = "等待电脑确认…";
        p.ackTimer = setTimeout(function () {
          if (!uploadProgress[id]) return;
          dropUploadRow(id);
          showToast("电脑未确认，请检查是否已保存");
        }, 30000);
      }
      return Promise.resolve(true);
    }
    var slice = file.slice(offset, Math.min(offset + MAX_CHUNK, file.size));
    var reader = new FileReader();
    return new Promise(function (resolve) {
    reader.onload = function () {
      var b64 = reader.result.split(",")[1];
      if (!send({
        type: "file-chunk",
        id: id,
        seq: Math.floor(offset / MAX_CHUNK),
        data: b64
      })) {
        var disconnected = uploadProgress[id];
        if (disconnected) disconnected.textEl.textContent = "连接断开";
        setTimeout(function () { dropUploadRow(id); }, 1000);
        resolve(false);
        return;
      }
      var p = uploadProgress[id];
      if (p) {
        p.sent = Math.min(file.size, offset + MAX_CHUNK);
        var pct = Math.round(p.sent * 100 / file.size);
        p.barEl.firstChild.style.width = pct + "%";
        p.textEl.textContent = pct + "%";
      }
      setTimeout(function () {
        sendChunks(id, file, offset + MAX_CHUNK).then(resolve);
      }, 0);
    };
    reader.onerror = function () {
      var p = uploadProgress[id];
      if (p) p.textEl.textContent = "读取失败";
      setTimeout(function () { dropUploadRow(id); }, 1000);
      resolve(false);
    };
    reader.readAsDataURL(slice);
    });
  }

  function formatSize(n) {
    if (n < 1024) return n + " B";
    if (n < 1048576) return (n / 1024).toFixed(1) + " KB";
    if (n < 1073741824) return (n / 1048576).toFixed(1) + " MB";
    return (n / 1073741824).toFixed(2) + " GB";
  }

  function escapeHtml(s) {
    return s.replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }

  var toastTimer = null;
  function showToast(text) {
    var el = document.getElementById("toast");
    if (!el) {
      el = document.createElement("div");
      el.id = "toast";
      el.style.cssText =
        "position:fixed;left:50%;bottom:28px;transform:translateX(-50%);background:rgba(17,24,39,.9);color:#fff;padding:10px 18px;border-radius:10px;font-size:14px;z-index:99;transition:opacity .3s;";
      document.body.appendChild(el);
    }
    el.textContent = text;
    el.style.opacity = "1";
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { el.style.opacity = "0"; }, 1800);
  }

  pickBtn.addEventListener("click", function () { fileInput.click(); });
  fileInput.addEventListener("change", function () {
    var files = Array.prototype.slice.call(fileInput.files || []);
    files.forEach(queueFile);
    fileInput.value = "";
  });
  // 相册入口：带 accept 的 input 才会唤起安卓系统相册，两个 input 共用同一个上传队列
  pickPhotoBtn.addEventListener("click", function () { photoInput.click(); });
  photoInput.addEventListener("change", function () {
    var files = Array.prototype.slice.call(photoInput.files || []);
    files.forEach(queueFile);
    photoInput.value = "";
  });
  sendFilesBtn.addEventListener("click", function () {
    if (!isConnected()) {
      forceReconnect();
      setStatus("正在重新连接，请稍候…", false);
      showToast("连接恢复后请再点一次发送");
      return;
    }
    var files = pendingFiles.slice();
    pendingFiles = [];
    renderQueue();
    (async function () {
      for (var i = 0; i < files.length; i++) {
        var ok = await uploadFile(files[i]);
        if (!ok) {
          // 断网时把当前及未发的文件放回队列，重连后用户再点发送即可续传（重传整份）
          var rest = files.slice(i);
          pendingFiles = rest.concat(pendingFiles);
          renderQueue();
          showToast("连接断开，已重新排队 " + rest.length + " 个文件");
          return;
        }
      }
    })();
  });
  clearFilesBtn.addEventListener("click", function () {
    pendingFiles = [];
    renderQueue();
  });
  $("getClipBtn").addEventListener("click", function () {
    if (!send({ type: "clipboard-request" })) {
      forceReconnect();
      showToast("正在重连，请稍候再试");
    }
  });
  $("sendClipBtn").addEventListener("click", function () {
    var text = $("clipInput").value;
    if (!text.trim()) {
      showToast("内容为空");
      return;
    }
    if (!send({ type: "clipboard", text: text })) {
      forceReconnect();
      setStatus("正在重新连接，请稍候…", false);
      showToast("连接恢复后请再点一次发送");
      return;
    }
    showToast("已发送到电脑");
    $("clipInput").value = "";
  });

  setStatus("连接中…", false);
  serverInfoEl.textContent = "会话 " + (session ? session.slice(0, 6) + "…" : "无效");
  if (!session) {
    setStatus("链接无效，请重新扫码", false);
  } else {
    connect();
  }

  async function openScanner() {
    if (!window.BarcodeDetector || !navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
      showToast("当前浏览器不支持相机扫码，请使用手机相机扫描二维码");
      return;
    }
    var scanner = $("scanner");
    var video = $("scannerVideo");
    scanner.style.display = "block";
    try {
      scannerStream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: "environment" } } });
      video.srcObject = scannerStream;
      var detector = new BarcodeDetector({ formats: ["qr_code"] });
      scannerTimer = setInterval(async function () {
        if (!video.videoWidth) return;
        var codes = await detector.detect(video);
        if (codes.length && codes[0].rawValue) location.href = codes[0].rawValue;
      }, 300);
    } catch (e) {
      closeScanner();
      showToast("无法打开相机，请检查浏览器权限");
    }
  }

  function closeScanner() {
    if (scannerTimer) clearInterval(scannerTimer);
    scannerTimer = null;
    if (scannerStream) scannerStream.getTracks().forEach(function (track) { track.stop(); });
    scannerStream = null;
    $("scannerVideo").srcObject = null;
    $("scanner").style.display = "none";
  }

  $("scanBtn").addEventListener("click", openScanner);
  $("closeScannerBtn").addEventListener("click", closeScanner);
  renderQueue();
})();
