// cstream player input: the PURE half of the player's input path -- keyboard
// mapping, modifier flags, pointer scaling and the control-channel envelope -- with
// no DOM, so it is unit-tested (input.test.js) and the player only wires events to it.
//
// Loaded by the player as a classic script (global `CstreamInput`) and by the tests
// as a CommonJS module.
"use strict";

const CstreamInput = (() => {
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

// KeyboardEvent.key -> [keysym name, needs Shift on a US layout], for the events that
// carry no usable `code`: synthesized input (automation, on-screen keyboards) often
// sends only `key`. Mirrors the streamer's keys.rs: a shifted character is its BASE
// key plus Shift, because a shifted keysym name (`exclam`) presses the key WITHOUT
// Shift and types the unshifted character.
const KEY_TO_KEYSYM = (() => {
  const m = {
    Enter: ["Return", false], Escape: ["Escape", false], Backspace: ["BackSpace", false],
    Tab: ["Tab", false], " ": ["space", false], Delete: ["Delete", false],
    ArrowUp: ["Up", false], ArrowDown: ["Down", false], ArrowLeft: ["Left", false], ArrowRight: ["Right", false],
    Home: ["Home", false], End: ["End", false], PageUp: ["Page_Up", false], PageDown: ["Page_Down", false],
  };
  const plain = { "-": "minus", "=": "equal", "[": "bracketleft", "]": "bracketright", "\\": "backslash",
    ";": "semicolon", "'": "apostrophe", "`": "grave", ",": "comma", ".": "period", "/": "slash" };
  const shifted = { "_": "minus", "+": "equal", "{": "bracketleft", "}": "bracketright", "|": "backslash",
    ":": "semicolon", '"': "apostrophe", "~": "grave", "<": "comma", ">": "period", "?": "slash",
    "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7", "*": "8", "(": "9", ")": "0" };
  for (const [k, v] of Object.entries(plain)) m[k] = [v, false];
  for (const [k, v] of Object.entries(shifted)) m[k] = [v, true];
  for (let c = 97; c <= 122; c++) {
    const ch = String.fromCharCode(c);
    m[ch] = [ch, false];
    m[ch.toUpperCase()] = [ch, true];
  }
  for (let d = 0; d <= 9; d++) m[String(d)] = [String(d), false];
  for (let f = 1; f <= 12; f++) m["F" + f] = ["F" + f, false];
  return m;
})();

// The keysym for an event and whether Shift must be synthesized around it: the
// physical `code` first, else the logical `key` (needing Shift only when the event
// did not already hold it).
function keyFor(e) {
  const byCode = CODE_TO_KEYSYM[e.code];
  if (byCode) return { key: byCode, shift: false };
  const byKey = KEY_TO_KEYSYM[e.key];
  if (byKey) return { key: byKey[0], shift: byKey[1] && !e.shiftKey };
  return null;
}

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


// Map a client point on the <video> element to stream pixels, accounting for the
// letterboxing object-fit: contain introduces. `rect` is the element's client rect
// and `vw`/`vh` the stream's size.
function streamPoint(rect, vw, vh, clientX, clientY) {
  vw = vw || 1;
  vh = vh || 1;
  const scale = Math.min(rect.width / vw, rect.height / vh);
  const ox = rect.left + (rect.width - vw * scale) / 2;
  const oy = rect.top + (rect.height - vh * scale) / 2;
  const x = Math.min(Math.max((clientX - ox) / scale, 0), vw - 1);
  const y = Math.min(Math.max((clientY - oy) / scale, 0), vh - 1);
  return { x, y };
}

// DOM button numbering (0 left, 1 middle, 2 right) -> GstNavigation (1, 2, 3).
const navButton = (b) => (b === 1 ? 2 : b === 2 ? 3 : 1);

// The control-channel message webrtcsink parses: a request envelope around one
// GstNavigation event, which must carry modifier_state. `mid` names the VIDEO stream:
// with a null mid webrtcsink pushes the event into every input stream (audio too) and
// answers "no error" if ANY of them took it, so the answer would not say whether the
// desktop received the input. Addressed to the video mid, it does.
function controlMessage(id, event, modifiers, mid) {
  if (!mid) throw new Error("controlMessage needs the video stream's mid");
  return JSON.stringify({ id, mid, request: { type: "navigationEvent", event: Object.assign({}, event, { modifier_state: modifiers }) } });
}

// A DOM wheel event -> GstNavigation MouseScroll deltas, in the unit the GWD element
// maps onto the seat's wheel axis: 120 per notch, positive = up/left (GStreamer's sink
// convention; the DOM's positive deltaY is DOWN). deltaMode says what a DOM delta counts:
// pixels (a notch is ~100 px in Chromium), lines (3 lines per notch) or pages (1 notch).
const WHEEL_UNITS_PER = [120 / 100, 120 / 3, 120];
function wheelDeltas(deltaX, deltaY, deltaMode) {
  const k = WHEEL_UNITS_PER[deltaMode] ?? WHEEL_UNITS_PER[0];
  return { delta_x: -deltaX * k || 0, delta_y: -deltaY * k || 0 };
}

return { CODE_TO_KEYSYM, KEY_TO_KEYSYM, keyFor, modifierState, streamPoint, navButton, controlMessage, wheelDeltas };
})();

if (typeof module !== "undefined") module.exports = CstreamInput;
