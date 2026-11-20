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

// KeyboardEvent.code (the PHYSICAL key) -> the X keysym name waylanddisplaysrc maps
// to an evdev scancode. Physical, not KeyboardEvent.key: the remote seat applies its
// own XKB layout, so sending the logical character would double-apply the layout.
const CODE_TO_KEYSYM = (() => {
  const m = {
    Escape: "Escape", Backspace: "BackSpace", Tab: "Tab", Enter: "Return", Space: "space",
    Minus: "minus", Equal: "equal", BracketLeft: "bracketleft", BracketRight: "bracketright",
    Backslash: "backslash", IntlBackslash: "backslash", Semicolon: "semicolon", Quote: "apostrophe",
    Backquote: "grave", Comma: "comma", Period: "period", Slash: "slash",
    ShiftLeft: "Shift_L", ShiftRight: "Shift_R", ControlLeft: "Control_L", ControlRight: "Control_R",
    AltLeft: "Alt_L", AltRight: "Alt_R", MetaLeft: "Super_L", MetaRight: "Super_R",
    CapsLock: "Caps_Lock", NumLock: "Num_Lock", ScrollLock: "Scroll_Lock", Pause: "Pause",
    ArrowUp: "Up", ArrowDown: "Down", ArrowLeft: "Left", ArrowRight: "Right",
    Home: "Home", End: "End", PageUp: "Page_Up", PageDown: "Page_Down", Insert: "Insert", Delete: "Delete",
    NumpadEnter: "KP_Enter", NumpadAdd: "KP_Add", NumpadSubtract: "KP_Subtract",
    NumpadMultiply: "KP_Multiply", NumpadDivide: "KP_Divide", NumpadDecimal: "KP_Decimal",
  };
  for (let c = 65; c <= 90; c++) m["Key" + String.fromCharCode(c)] = String.fromCharCode(c + 32);
  for (let d = 0; d <= 9; d++) { m["Digit" + d] = String(d); m["Numpad" + d] = "KP_" + d; }
  for (let f = 1; f <= 12; f++) m["F" + f] = "F" + f;
  return m;
})();

// GstNavigationModifierType as gstreamer-rs' serde writes it: the flag nicks joined
// by "+", or "" for none. The field is REQUIRED -- webrtcsink drops an event without
// it as invalid -- and an unknown nick fails the same way, so only these four are sent.
function modifierState(e) {
  const m = [];
  if (e.shiftKey) m.push("shift-mask");
  if (e.ctrlKey) m.push("control-mask");
  if (e.altKey) m.push("mod1-mask");
  if (e.metaKey) m.push("super-mask");
  return m.join("+");
}

function sendNav(e, event) {
  if (!control || control.readyState !== "open") return;
  event.modifier_state = modifierState(e);
  control.send(JSON.stringify({ id: controlId++, mid: null, request: { type: "navigationEvent", event } }));
}

// Map a client point on the <video> element to stream pixels, accounting for the
// letterboxing object-fit: contain introduces.
function streamPoint(e) {
  const r = video.getBoundingClientRect();
  const vw = video.videoWidth || 1, vh = video.videoHeight || 1;
  const scale = Math.min(r.width / vw, r.height / vh);
  const ox = r.left + (r.width - vw * scale) / 2;
  const oy = r.top + (r.height - vh * scale) / 2;
  const x = Math.min(Math.max((e.clientX - ox) / scale, 0), vw - 1);
  const y = Math.min(Math.max((e.clientY - oy) / scale, 0), vh - 1);
  return { x, y };
}

// DOM button numbering (0 left, 1 middle, 2 right) -> GstNavigation (1, 2, 3).
const navButton = (b) => (b === 1 ? 2 : b === 2 ? 3 : 1);

video.addEventListener("keydown", (e) => {
  const key = CODE_TO_KEYSYM[e.code];
  if (!key) return;
  e.preventDefault();
  sendNav(e, { event: "KeyPress", key });
});
video.addEventListener("keyup", (e) => {
  const key = CODE_TO_KEYSYM[e.code];
  if (!key) return;
  e.preventDefault();
  sendNav(e, { event: "KeyRelease", key });
});
video.addEventListener("mousemove", (e) => {
  const p = streamPoint(e);
  sendNav(e, { event: "MouseMove", x: p.x, y: p.y });
});
video.addEventListener("mousedown", (e) => {
  video.focus();
  const p = streamPoint(e);
  e.preventDefault();
  sendNav(e, { event: "MouseButtonPress", button: navButton(e.button), x: p.x, y: p.y });
});
video.addEventListener("mouseup", (e) => {
  const p = streamPoint(e);
  e.preventDefault();
  sendNav(e, { event: "MouseButtonRelease", button: navButton(e.button), x: p.x, y: p.y });
});
video.addEventListener("wheel", (e) => {
  const p = streamPoint(e);
  e.preventDefault();
  sendNav(e, { event: "MouseScroll", x: p.x, y: p.y, delta_x: e.deltaX, delta_y: e.deltaY });
}, { passive: false });
video.addEventListener("contextmenu", (e) => e.preventDefault());

// ---- boot ------------------------------------------------------------------

checkSession().then((ok) => (ok ? start() : setState("login")));
