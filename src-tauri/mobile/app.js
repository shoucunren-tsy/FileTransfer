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
  var MAX_CHUNK = 512 * 1024;
  var transferProgress = {};   // id -> {name, size, received, parts: []}
  var uploadProgress = {};     // id -> {name, size, sent, itemEl, barEl, textEl}

  function setStatus(text, ok) {
    statusEl.textContent = text;
    statusEl.classList.toggle("ok", !!ok);
    if (typeof renderQueue === "function") renderQueue();
  }

  function connect() {
    var proto = location.protocol === "https:" ? "wss://" : "ws://";
    var url = proto + location.host + "/ws?session=" + encodeURIComponent(session);
    ws = new WebSocket(url);

    ws.onopen = function () {
      setStatus("已连接", true);
      reconnectDelay = 1000;
      renderQueue();
      send({ type: "hello", session: session });
    };

    ws.onmessage = function (ev) {
      var msg;
      try { msg = JSON.parse(ev.data); } catch (e) { return; }
      handleMessage(msg);
    };

    ws.onclose = function () {
      setStatus("连接断开，重连中…", false);
      renderQueue();
      scheduleReconnect();
    };

    ws.onerror = function () {
      try { ws.close(); } catch (e) {}
    };
  }

  function scheduleReconnect() {
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) return;
    setTimeout(connect, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 15000);
  }

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
      case "clipboard":
        showClipboard(msg.text);
        break;
      case "pong":
        break;
      case "error":
        setStatus(msg.message || "发生错误", false);
        break;
    }
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
    uploadList.appendChild(p.itemEl);
    p.barEl.className = "progress";
    p.barEl.innerHTML = "<div></div>";
    uploadList.appendChild(p.barEl);

    if (!send({ type: "file-meta", id: id, name: file.name, size: file.size })) {
      p.textEl.textContent = "连接断开";
      return;
    }
    await sendChunks(id, file, 0);
  }

  function sendChunks(id, file, offset) {
    if (offset >= file.size) {
      send({ type: "file-complete", id: id });
      var p = uploadProgress[id];
      if (p) {
        p.textEl.textContent = "完成";
        setTimeout(function () {
          p.barEl.remove();
          p.itemEl.remove();
          delete uploadProgress[id];
        }, 1200);
      }
      return Promise.resolve();
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
        resolve();
        return;
      }
      var p = uploadProgress[id];
      if (p) {
        p.sent = Math.min(file.size, offset + MAX_CHUNK);
        var pct = Math.round(p.sent * 100 / file.size);
        p.barEl.firstChild.style.width = pct + "%";
        p.textEl.textContent = pct + "%";
      }
      setTimeout(function () { sendChunks(id, file, offset + MAX_CHUNK).then(resolve); }, 0);
    };
    reader.onerror = function () {
      var p = uploadProgress[id];
      if (p) p.textEl.textContent = "读取失败";
      resolve();
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
      setStatus("连接断开，请重新扫码", false);
      showToast("当前未连接电脑");
      return;
    }
    var files = pendingFiles.slice();
    pendingFiles = [];
    renderQueue();
    (async function () {
      for (var i = 0; i < files.length; i++) await uploadFile(files[i]);
    })();
  });
  clearFilesBtn.addEventListener("click", function () {
    pendingFiles = [];
    renderQueue();
  });
  $("getClipBtn").addEventListener("click", function () {
    if (!send({ type: "clipboard-request" })) showToast("当前未连接电脑");
  });
  $("sendClipBtn").addEventListener("click", function () {
    var text = $("clipInput").value;
    if (!text.trim()) {
      showToast("内容为空");
      return;
    }
    if (!send({ type: "clipboard", text: text })) {
      setStatus("连接断开，请重新扫码", false);
      showToast("当前未连接电脑");
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
