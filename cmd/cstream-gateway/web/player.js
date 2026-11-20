// cstream player: log in, receive the desktop over WebRTC, send input back.
//
// Signalling speaks the gst-plugins-rs webrtcsink protocol through the gateway's
// authenticated /signalling proxy. Input goes over the "control" data channel that
// webrtcsink opens (enable-control-data-channel) as GstNavigation events, which
// waylanddisplaysrc turns into Wayland input on its own seat.
//
// Every phase is mirrored into data-state attributes (body: login|connecting|
// playing|error; video: idle|connecting|playing|ended) so a check can wait on the
// state a user would see instead of on a delay.
"use strict";

const $ = (id) => document.getElementById(id);
const body = document.body;
const video = $("screen");

function setState(s, detail) {
  body.dataset.state = s;
  video.dataset.state = s === "login" ? "idle" : s;
  if (detail) body.dataset.detail = detail;
}

// ---- login -----------------------------------------------------------------

async function checkSession() {
  const r = await fetch("/session", { credentials: "same-origin" });
  return r.ok;
}

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("login-error").textContent = "";
  const form = new URLSearchParams(new FormData(e.target));
  const r = await fetch("/login", { method: "POST", body: form, credentials: "same-origin" });
  if (r.ok) {
    $("login-password").value = "";
    start();
  } else {
    $("login-error").textContent = r.status === 401 ? "Login refused." : "Login failed (" + r.status + ").";
    setState("login");
  }
});

// ---- signalling + WebRTC ---------------------------------------------------

let ws = null;
let pc = null;
let sessionId = null;
let control = null;
// The video stream's mid (from its transceiver): input is addressed to it, see controlMessage.
let videoMid = null;
let controlId = 1;

function send(msg) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg));
}

function start() {
  $("login").hidden = true;
  video.hidden = false;
  setState("connecting");
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(proto + "//" + location.host + "/signalling");
  ws.onmessage = (ev) => onSignal(JSON.parse(ev.data));
  ws.onclose = () => {
    if (body.dataset.state !== "playing") setState("error", "signalling closed");
    else setState("ended");
  };
}

function startSession(producerId) {
  if (sessionId) return;
  send({ type: "startSession", peerId: producerId });
}

function onSignal(msg) {
  switch (msg.type) {
    case "welcome":
      send({ type: "setPeerStatus", roles: ["listener"], meta: null });
      send({ type: "list" });
      break;
    case "list":
      if (msg.producers && msg.producers.length) startSession(msg.producers[0].id);
      break;
    case "peerStatusChanged":
      if (msg.roles && msg.roles.includes("producer")) startSession(msg.peerId);
      break;
    case "sessionStarted":
      sessionId = msg.sessionId;
      break;
    case "peer":
      onPeer(msg);
      break;
    case "endSession":
      sessionId = null;
      if (pc) pc.close();
      setState("ended");
      break;
    case "error":
      setState("error", msg.details || "signalling error");
      break;
  }
}

function ensurePeerConnection() {
  if (pc) return pc;
  // No ICE servers: the browser and the pod share a network, and host candidates
  // are the whole path. A STUN server would only add an external dependency.
  pc = new RTCPeerConnection({ iceServers: [] });
  pc.ontrack = (ev) => {
    if (ev.track.kind === "video") videoMid = ev.transceiver.mid;
    if (!video.srcObject) video.srcObject = ev.streams[0] || new MediaStream([ev.track]);
  };
  pc.ondatachannel = (ev) => {
    if (ev.channel.label === "control") control = ev.channel;
  };
  pc.onicecandidate = (ev) => {
    if (ev.candidate) {
      send({ type: "peer", sessionId, ice: { candidate: ev.candidate.candidate, sdpMLineIndex: ev.candidate.sdpMLineIndex } });
    }
  };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === "failed") setState("error", "peer connection failed");
  };
  return pc;
}

async function onPeer(msg) {
  const conn = ensurePeerConnection();
  if (msg.sdp) {
    await conn.setRemoteDescription(msg.sdp);
    if (msg.sdp.type === "offer") {
      const answer = await conn.createAnswer();
      await conn.setLocalDescription(answer);
      send({ type: "peer", sessionId, sdp: { type: answer.type, sdp: answer.sdp } });
    }
  } else if (msg.ice) {
    await conn.addIceCandidate(msg.ice);
  }
}

video.addEventListener("playing", () => {
  setState("playing");
  video.focus();
});

// ---- input -----------------------------------------------------------------

const { keyFor, modifierState, streamPoint, navButton, controlMessage, wheelDeltas } = CstreamInput;

function sendNav(e, event) {
  if (!control || control.readyState !== "open" || !videoMid) return;
  control.send(controlMessage(controlId++, event, modifierState(e), videoMid));
}

const pointAt = (e) => streamPoint(video.getBoundingClientRect(), video.videoWidth, video.videoHeight, e.clientX, e.clientY);

video.addEventListener("keydown", (e) => {
  const k = keyFor(e);
  if (!k) return;
  e.preventDefault();
  if (k.shift) sendNav(e, { event: "KeyPress", key: "Shift_L" });
  sendNav(e, { event: "KeyPress", key: k.key });
  // A synthesized shifted character gets its release right away: a source that
  // sends no `code` gives no guarantee its keyup will carry the same `key`.
  if (k.shift) {
    sendNav(e, { event: "KeyRelease", key: k.key });
    sendNav(e, { event: "KeyRelease", key: "Shift_L" });
  }
});
video.addEventListener("keyup", (e) => {
  const k = keyFor(e);
  if (!k || k.shift) return;
  e.preventDefault();
  sendNav(e, { event: "KeyRelease", key: k.key });
});
video.addEventListener("mousemove", (e) => {
  const p = pointAt(e);
  sendNav(e, { event: "MouseMove", x: p.x, y: p.y });
});
video.addEventListener("mousedown", (e) => {
  video.focus();
  const p = pointAt(e);
  e.preventDefault();
  sendNav(e, { event: "MouseButtonPress", button: navButton(e.button), x: p.x, y: p.y });
});
video.addEventListener("mouseup", (e) => {
  const p = pointAt(e);
  e.preventDefault();
  sendNav(e, { event: "MouseButtonRelease", button: navButton(e.button), x: p.x, y: p.y });
});
video.addEventListener("wheel", (e) => {
  const p = pointAt(e);
  e.preventDefault();
  sendNav(e, Object.assign({ event: "MouseScroll", x: p.x, y: p.y }, wheelDeltas(e.deltaX, e.deltaY, e.deltaMode)));
}, { passive: false });
video.addEventListener("contextmenu", (e) => e.preventDefault());

// ---- boot ------------------------------------------------------------------

checkSession().then((ok) => (ok ? start() : setState("login")));
