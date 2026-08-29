# charly-streamer

The cstream server side: a Wayland desktop streamed over WebRTC.

`cstream-streamer` embeds `gst-wayland-display` as the Wayland parent, encodes with
VA-API where the host has it, and transports over WebRTC.

## Why these three modules exist

Each covers something that fails **silently** if it regresses:

- **`rank`** — webrtcsink enumerates encoders once into a `LazyLock`, and `vah264enc`
  registers at `GST_RANK_NONE`. Promote it late and the stream quietly runs on the CPU.
- **`display`** — the source is created *from the registry*, never as a crate, so this
  crate's gstreamer-rs generation stays independent of the one GWD was built against.
  The render node is always a real DRM node: GWD creates `zwp_linux_dmabuf_v1` only on
  the hardware path, and a nested compositor hard-requires that global.
- **`input`** — GWD `.expect()`s each event field at a specific type. A mismatch panics
  the element thread rather than degrading, so the types are the wire contract.
