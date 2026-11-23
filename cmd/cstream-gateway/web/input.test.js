// Unit tests for the player's pure input logic (input.js). Run: node --test cmd/cstream-gateway/web
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const C = require("./input.js");

// The physical key wins: the remote seat applies its own XKB layout, so the physical
// key is what must travel, whatever character the local layout would produce.
test("a physical key maps by its code", () => {
  assert.deepEqual(C.keyFor({ code: "KeyA", key: "q", shiftKey: false }), { key: "a", shift: false });
  assert.deepEqual(C.keyFor({ code: "Enter", key: "Enter" }), { key: "Return", shift: false });
  assert.deepEqual(C.keyFor({ code: "ShiftLeft", key: "Shift" }), { key: "Shift_L", shift: false });
});

// Synthesized input often carries no code. A shifted character is then its BASE key
// with Shift synthesized -- unless the event already holds Shift.
test("a code-less key maps by its character, with Shift for shifted characters", () => {
  assert.deepEqual(C.keyFor({ code: "", key: "t" }), { key: "t", shift: false });
  assert.deepEqual(C.keyFor({ code: "", key: "/" }), { key: "slash", shift: false });
  assert.deepEqual(C.keyFor({ code: "", key: "A", shiftKey: false }), { key: "a", shift: true });
  assert.deepEqual(C.keyFor({ code: "", key: "A", shiftKey: true }), { key: "a", shift: false });
  assert.deepEqual(C.keyFor({ code: "", key: "!" }), { key: "1", shift: true });
  assert.equal(C.keyFor({ code: "", key: "€" }), null);
});

// Every printable ASCII character reaches a key, and every key name is one
// waylanddisplaysrc's keysym table maps (single letters/digits or these names).
test("every printable ASCII character maps to a key the element knows", () => {
  const named = new Set(["space", "minus", "equal", "bracketleft", "bracketright", "backslash",
    "semicolon", "apostrophe", "grave", "comma", "period", "slash"]);
  for (let c = 32; c < 127; c++) {
    const ch = String.fromCharCode(c);
    const k = C.keyFor({ code: "", key: ch });
    assert.ok(k, `${JSON.stringify(ch)} has no key`);
    assert.ok(/^[a-z0-9]$/.test(k.key) || named.has(k.key), `${JSON.stringify(ch)} -> ${k.key}`);
  }
});

test("modifier flags serialize the way gstreamer-rs reads them", () => {
  assert.equal(C.modifierState({}), "");
  assert.equal(C.modifierState({ shiftKey: true, ctrlKey: true, altKey: true, metaKey: true }),
    "shift-mask+control-mask+mod1-mask+super-mask");
});

// object-fit: contain letterboxes the stream; a click on the bar must not land off
// the desktop, and a click on the picture must land on the matching stream pixel.
test("pointer coordinates are scaled through the letterbox", () => {
  const rect = { left: 0, top: 0, width: 1000, height: 1000 }; // 1920x1080 -> 1000x562.5, bars of 218.75
  const near = (got, want) => {
    assert.ok(Math.abs(got.x - want.x) < 1e-6 && Math.abs(got.y - want.y) < 1e-6,
      `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
  };
  near(C.streamPoint(rect, 1920, 1080, 500, 500), { x: 960, y: 540 });
  near(C.streamPoint(rect, 1920, 1080, 0, 0), { x: 0, y: 0 }); // on the top bar: clamped onto the desktop
  near(C.streamPoint(rect, 1920, 1080, 1000, 1000), { x: 1919, y: 1079 });
});

test("DOM buttons map to GstNavigation buttons", () => {
  assert.deepEqual([0, 1, 2].map(C.navButton), [1, 2, 3]);
});

// The envelope webrtcsink parses. The same event shapes are asserted to deserialize
// into gst_video::NavigationEvent in crates/cstream-streamer/tests/contract.rs.
test("the control message is the navigationEvent envelope with modifier_state, addressed to the video mid", () => {
  const m = JSON.parse(C.controlMessage(7, { event: "MouseScroll", x: 1, y: 2, delta_x: 0, delta_y: -120 }, "", "video0"));
  assert.deepEqual(m, {
    id: 7, mid: "video0",
    request: { type: "navigationEvent", event: { event: "MouseScroll", x: 1, y: 2, delta_x: 0, delta_y: -120, modifier_state: "" } },
  });
});

// A null mid would make webrtcsink push into the audio stream too and answer success
// whoever took it, so an unaddressed message is refused rather than sent.
test("a control message without the video mid is refused", () => {
  assert.throws(() => C.controlMessage(1, { event: "MouseMove", x: 1, y: 2 }, ""), /mid/);
});

// The wheel contract the GWD element maps onto the seat: 120 per notch, positive = up.
// The DOM's positive deltaY is DOWN, so one notch down (100 px in Chromium) is -120.
test("a wheel notch becomes one 120-unit step with GStreamer's sign", () => {
  assert.deepEqual(C.wheelDeltas(0, 100, 0), { delta_x: 0, delta_y: -120 });
  assert.deepEqual(C.wheelDeltas(0, -3, 1), { delta_x: 0, delta_y: 120 });
  assert.deepEqual(C.wheelDeltas(1, 0, 2), { delta_x: -120, delta_y: 0 });
  assert.deepEqual(C.wheelDeltas(-50, 25, 0), { delta_x: 60, delta_y: -30 });
});
