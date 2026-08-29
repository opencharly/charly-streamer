package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// The gateway's readiness is what a cdp: probe ultimately asserts, so "ready" must
// mean the two things a browser client genuinely depends on. Reporting ready
// without them would let a page load and then stream nothing -- a green check over
// a dead desktop.

func TestNotReadyWithoutRenderNode(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "wayland-1"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	s := probeStatus("/definitely/not/a/render/node", dir, "wayland-1")
	if s.Ready {
		t.Fatal("ready without a render node — there is no software fallback that can host a nested compositor")
	}
	if strings.Contains(page(s), "CSTREAM-READY") {
		t.Fatal("the page must not emit the ready marker when not ready")
	}
}

func TestNotReadyWithoutWaylandSocket(t *testing.T) {
	dir := t.TempDir()
	node := filepath.Join(dir, "renderD128")
	if err := os.WriteFile(node, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	s := probeStatus(node, dir, "wayland-1") // socket absent
	if s.Ready {
		t.Fatal("ready without a compositor socket — the page would load and stream nothing")
	}
}

func TestReadyWithBoth(t *testing.T) {
	dir := t.TempDir()
	node := filepath.Join(dir, "renderD128")
	for _, f := range []string{node, filepath.Join(dir, "wayland-1")} {
		if err := os.WriteFile(f, nil, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	s := probeStatus(node, dir, "wayland-1")
	if !s.Ready {
		t.Fatal("both preconditions present but not ready")
	}
	p := page(s)
	if !strings.Contains(p, "CSTREAM-READY") {
		t.Fatalf("the ready marker a cdp: probe asserts is missing:\n%s", p)
	}
	if !strings.Contains(p, node) {
		t.Error("the page should name the render node it is using")
	}
}

// An empty display or node is a misconfiguration, not a degraded mode.
func TestEmptyConfigIsNotReady(t *testing.T) {
	dir := t.TempDir()
	if probeStatus("", dir, "wayland-1").Ready {
		t.Error("empty render node must not be ready")
	}
	if probeStatus("/dev/null", dir, "").Ready {
		t.Error("empty display must not be ready")
	}
}
