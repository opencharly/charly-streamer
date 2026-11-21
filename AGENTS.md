# AGENTS.md — opencharly/charly-streamer

The `cstream` server side: a Wayland desktop streamed over WebRTC.
`cstream-streamer` embeds `gst-wayland-display` as the Wayland parent, encodes on
the render node's own GPU (NVENC via CUDA, or VA-API), and transports over WebRTC;
`cstream-gateway` serves the browser player, PAM login and the authenticated
signalling proxy; `cstream-input` drives a stream the way a browser does. This is PRODUCT
code (Rust + Go), not a candy — it ships no `charly.yml`.

Canonical files:

- `Cargo.toml` — the Cargo workspace (`crates/cstream-streamer`,
  `crates/cstream-leader`).
- `crates/cstream-streamer/src/` holds:
  - `rank.rs`: the per-render-node encode backend and its encoder promotion;
  - `display.rs`: the registry-created source and the render-node rule;
  - `keys.rs`: text to US-layout physical keys;
  - `bin/cstream-input.rs`: the stream-driving test client;
  - the pipeline, webrtc, audio, control, lifecycle and status modules.
  - `tests/contract.rs` asserts the player's input JSON against webrtcsink's
    NavigationEvent type.
- `crates/cstream-leader/` — the leader binary (`pam-sys` + bindgen).
- `cmd/cstream-gateway/` + `go.mod` is the Go gateway (login, session, signalling
  proxy, `/healthz`), with the embedded player under `cmd/cstream-gateway/web/`.
- `.github/workflows/rust.yml` — the gates: `cargo fmt --check`, `cargo clippy
  -D warnings`, `cargo build`, `cargo test`, and the Go build/test/vet.
- `README.md` — user overview only; never agent guidance.

## Load these skills before starting (R0)

- `/charly-internals:repo-setup` — the org landing automation (required workflow,
  native auto-merge, tag-on-merge CalVer) and the new-repo checklist.
- `/charly-selkies:kde-cstream`: pod-kde-cstream, the KDE Plasma desktop streamed
  through this server side, and its beds (the R10 gate for a change here).

## Build / validate / test

- `cargo fmt --all --check` — formatting.
- `cargo clippy --workspace --all-targets -- -D warnings` — lint (warnings are
  errors).
- `cargo build --workspace` and `cargo test --workspace` — build + tests (the
  ranking test skips itself when the runner has no VA-API encoder).
- `CGO_ENABLED=0 go build ./... && CGO_ENABLED=0 go test ./... && go vet ./...` —
  the Go gateway gates.
- The merge gate is the **org-wide** `charly/pr-validator` (required check
  `validate / validate`, defined in `opencharly/.github`); this repo carries no
  per-repo candy gate.

## Modify this repo

- The modules the README lists each cover something that fails SILENTLY if it
  regresses (`rank`, `display`, `keys`, `lifecycle`). Preserve their invariants and
  the tests that lock them:
  - the backend comes from the render node's driver, and only its encoders are
    promoted, before the first webrtcsink;
  - the source is created from the registry, on a real DRM node with no literal
    default;
  - typing uses base keys plus Shift.
- **Input has one path.** The player and `cstream-input` send the same `control`
  channel JSON, so the beds' input proof covers the browser's path. A new event
  shape is added to both, and to `tests/contract.rs`.
- The build needs system GStreamer `-dev` packages and, for the leader,
  `libpam0g-dev` + `libclang-dev` (bindgen); a missing one fails in `pkg-config`
  or `clang-sys`, not in this repo's code.

## Landing

- PR-only. Every change lands through a pull request; the org-required
  `charly/pr-validator` validates the diff and body and arms native auto-merge on
  PASS. Direct pushes to `main` are blocked.
- History lives in `CHANGELOG/` (written by `tag-on-merge` at merge time); the PR
  body IS the changelog.
- The authoritative rulebook is the umbrella `AGENTS.md` in
  `opencharly/opencharly` and `charly/AGENTS.md` in the charly repo. Do not
  restate its rules here.
