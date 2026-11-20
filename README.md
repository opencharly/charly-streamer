# charly-streamer

The cstream server side: a Wayland desktop streamed to a browser over WebRTC.

- **`cstream-streamer`** *is* the Wayland parent. It embeds gst-wayland-display's
  compositor (`waylanddisplaysrc`), encodes on the render node's own GPU, and serves
  the stream with webrtcsink.
- **`cstream-gateway`** is the one network-facing port (`:8080`). It serves the browser
  player, logs users in through PAM, and proxies the WebRTC signalling to the streamer,
  which listens on loopback only.
- **`cstream-leader`** authenticates a user through PAM and drops privileges.
- **`cstream-input`** is the test client. It joins the stream as a viewer and drives
  the desktop through the same control channel the browser uses.

## Encoding follows the render node

| render node's driver | capture | encoder |
|---|---|---|
| `nvidia` | `memory:CUDAMemory` (`cuda-device-id` in PCI bus order) | `nvh264enc` |
| `amdgpu`, `i915`, `xe` | `memory:DMABuf` → `vapostproc` | `vah264enc` |
| anything else | system memory | software (only under `CSTREAM_ENCODE=auto`) |

The render node is `CSTREAM_RENDER_NODE`, else charly's injected `DRINODE`.
`CSTREAM_ENCODE` is one of:
- `auto`: hardware, else software;
- `hardware`: fail rather than fall back;
- `software`.

## Browser and input

The player (`cmd/cstream-gateway/web/`):
1. logs in (`POST /login`);
2. opens the authenticated `/signalling` WebSocket;
3. plays the H.264 track;
4. sends keyboard, pointer and wheel input as GstNavigation events over webrtcsink's
   `control` data channel, addressed to the VIDEO stream's `mid`.

waylanddisplaysrc maps those events onto its seat. The address matters: with no `mid`,
webrtcsink pushes an event into every input stream (audio too) and acknowledges it if
any of them took it, so the acknowledgement would not prove the desktop received
anything. Addressed to the video stream, the acknowledgement is waylanddisplaysrc's own
verdict. `cstream-input` (the test client) does the same.

The stream follows the viewer's size live (the gateway's control socket, `resize W H`).
Any size inside the chosen encoder's own limits applies exactly, provided it is even
in both dimensions, the one constraint 4:2:0 imposes. So 1366x768, a portrait 390x844
and 4096x2160 on NVENC all stream at their own size. The limits are read from the
encoder's sink template, not hard-coded: NVENC H.264 accepts 160..4096 x 64..4096,
NVENC H.265 up to 8192.

## Why these modules exist

Each covers something that fails **silently** if it regresses:

- **`rank`** decides the backend from the render node and promotes ONLY that
  backend's encoders. webrtcsink enumerates encoders once, so a late or wrong promotion
  streams on the CPU, or on the other GPU.
- **`display`** creates the source *from the registry*, never as a crate, so this
  crate's gstreamer-rs generation stays independent of GWD's. The render node is always
  a real DRM node: GWD creates `zwp_linux_dmabuf_v1` only on the hardware path.
- **`keys`** maps text to US-layout physical keys. A shifted keysym name (`exclam`)
  would press the key without Shift and type the wrong character.
- **`lifecycle`** locks the desktop through `org.freedesktop.ScreenSaver.Lock` once the
  last viewer has been gone long enough (`CSTREAM_LOCK_AFTER`).
