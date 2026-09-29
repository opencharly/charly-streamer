# AGENTS.md — opencharly/charly-streamer

The `cstream` server side: a Wayland desktop streamed over WebRTC.
`cstream-streamer` embeds `gst-wayland-display` as the Wayland parent, encodes
with VA-API where the host has it, and transports over WebRTC. This is PRODUCT
code (Rust + Go), not a candy — it ships no `charly.yml`.

Canonical files:

- `Cargo.toml` — the Cargo workspace (`crates/cstream-streamer`,
  `crates/cstream-leader`).
- `crates/cstream-streamer/src/` — `rank.rs` (encoder ranking — `vah264enc`
  promotion), `display.rs` (registry-created source, real DRM node),
  `input.rs` (the typed GWD wire contract), plus the pipeline/webrtc/audio/
  control/lifecycle modules.
- `crates/cstream-leader/` — the leader binary (`pam-sys` + bindgen).
- `cmd/cstream-gateway/` + `go.mod` — the Go gateway.
- `.github/workflows/rust.yml` — the gates: `cargo fmt --check`, `cargo clippy
  -D warnings`, `cargo build`, `cargo test`, and the Go build/test/vet.
- `README.md` — user overview only; never agent guidance.

## Load these skills before starting (R0)

- `/charly-internals:repo-setup` — the org landing automation (required workflow,
  native auto-merge, tag-on-merge CalVer) and the new-repo checklist.
- `/charly-distros:omarchy-cstream` — the Omarchy desktop streamed to a browser
  via the cstream transport spine, the consumer of this server side.

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

- The three documented modules each cover something that fails SILENTLY if it
  regresses (`rank`, `display`, `input`); preserve their invariants (late VA-API
  promotion, registry-created source with a real DRM node, typed GWD event
  fields) and the tests that lock them.
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
