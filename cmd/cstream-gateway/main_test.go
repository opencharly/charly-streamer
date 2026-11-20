package main

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// The gateway's readiness is what a `cstream: status` probe ultimately asserts, so
// "ready" must mean the two things a browser client genuinely depends on. Reporting
// ready without them would let a page load and then stream nothing -- a green
// check over a dead desktop.

func touch(t *testing.T, paths ...string) {
	t.Helper()
	for _, p := range paths {
		if err := os.WriteFile(p, nil, 0o600); err != nil {
			t.Fatal(err)
		}
	}
}

func TestNotReadyWithoutRenderNode(t *testing.T) {
	dir := t.TempDir()
	touch(t, filepath.Join(dir, "wayland-1"))
	if probeStatus("/definitely/not/a/render/node", dir, "wayland-1").Ready {
		t.Fatal("ready without a render node — there is no software fallback that can host a nested compositor")
	}
}

func TestNotReadyWithoutWaylandSocket(t *testing.T) {
	dir := t.TempDir()
	node := filepath.Join(dir, "renderD128")
	touch(t, node)
	if probeStatus(node, dir, "wayland-1").Ready {
		t.Fatal("ready without a compositor socket — the page would load and stream nothing")
	}
}

func TestReadyWithBoth(t *testing.T) {
	dir := t.TempDir()
	node := filepath.Join(dir, "renderD128")
	touch(t, node, filepath.Join(dir, "wayland-1"))
	s := probeStatus(node, dir, "wayland-1")
	if !s.Ready {
		t.Fatal("both preconditions present but not ready")
	}
	if s.RenderNode != node {
		t.Errorf("status should name the render node it observed, got %q", s.RenderNode)
	}
}

// An empty display or node is a misconfiguration, not a degraded mode. The render
// node has NO built-in default: it comes from CSTREAM_RENDER_NODE only, so a pod
// that forgot to set it reports not-ready instead of silently using renderD128.
func TestEmptyConfigIsNotReady(t *testing.T) {
	dir := t.TempDir()
	if probeStatus("", dir, "wayland-1").Ready {
		t.Error("empty render node must not be ready")
	}
	if probeStatus("/dev/null", dir, "").Ready {
		t.Error("empty display must not be ready")
	}
}

// The encoder field is the STREAMER's report, read from the file it writes once the
// encoder has opened. Absent or malformed, the field is empty -- never a guess.
func TestEncoderIsTheStreamersReport(t *testing.T) {
	dir := t.TempDir()
	if got := probeStatus("", dir, "wayland-1").Encoder; got != "" {
		t.Fatalf("no streamer report yet, but encoder=%q", got)
	}
	if err := os.WriteFile(filepath.Join(dir, StreamerStatusFile), []byte(`{"encoder":"hardware:nvh264enc"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if got := probeStatus("", dir, "wayland-1").Encoder; got != "hardware:nvh264enc" {
		t.Fatalf("encoder=%q, want the streamer's hardware:nvh264enc", got)
	}
	if err := os.WriteFile(filepath.Join(dir, StreamerStatusFile), []byte(`not json`), 0o600); err != nil {
		t.Fatal(err)
	}
	if got := probeStatus("", dir, "wayland-1").Encoder; got != "" {
		t.Fatalf("a malformed report must read as empty, got %q", got)
	}
}

func TestHealthzCarriesEncoderAndViewers(t *testing.T) {
	g, dir := newTestGateway(t, "")
	node := filepath.Join(dir, "renderD128")
	touch(t, node, filepath.Join(dir, "wayland-1"))
	g.cfg.RenderNode = node
	if err := os.WriteFile(filepath.Join(dir, StreamerStatusFile), []byte(`{"encoder":"hardware:vah264enc"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	g.viewers.Store(2)
	srv := httptest.NewServer(g.routes())
	defer srv.Close()
	resp, err := http.Get(srv.URL + "/healthz")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var s Status
	if err := json.NewDecoder(resp.Body).Decode(&s); err != nil {
		t.Fatal(err)
	}
	if !s.Ready || s.Encoder != "hardware:vah264enc" || s.Viewers != 2 {
		t.Fatalf("healthz = %+v, want ready, hardware:vah264enc, 2 viewers", s)
	}
}

// The player is served from the embedded web tree, and it carries the hooks a
// browser check drives: the login form and the video element with its state.
func TestPlayerIsServed(t *testing.T) {
	g, _ := newTestGateway(t, "")
	srv := httptest.NewServer(g.routes())
	defer srv.Close()
	for path, want := range map[string][]string{
		"/":          {`id="login-form"`, `id="login-user"`, `id="login-password"`, `id="screen"`, `data-state="idle"`, `src="player.js"`},
		"/player.js": {`"/signalling"`, `"navigationEvent"`, `"control"`, `CODE_TO_KEYSYM`},
	} {
		resp, err := http.Get(srv.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		b, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if resp.StatusCode != http.StatusOK {
			t.Fatalf("GET %s = %d", path, resp.StatusCode)
		}
		for _, w := range want {
			if !strings.Contains(string(b), w) {
				t.Errorf("GET %s lacks %s", path, w)
			}
		}
	}
}
