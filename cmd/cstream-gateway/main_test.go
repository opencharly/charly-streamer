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

// The input targets are what make a CDP input probe non-vacuous: a click on an
// inert page cannot be distinguished from a dropped one. These assert the page
// ships the elements a probe drives AND the pre-interaction sentinels it must
// see change -- a page that shipped the RESULT text without the handler would
// let an input check pass without any input.
func TestPageShipsInputTargetsAndSentinels(t *testing.T) {
	dir := t.TempDir()
	node := filepath.Join(dir, "renderD128")
	for _, f := range []string{node, filepath.Join(dir, "wayland-1")} {
		if err := os.WriteFile(f, nil, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	p := page(probeStatus(node, dir, "wayland-1"))

	for _, id := range []string{"probe-button", "probe-input", "click-result", "type-result", "key-result"} {
		if !strings.Contains(p, `id="`+id+`"`) {
			t.Errorf("page is missing input target %q, so a CDP probe cannot drive it", id)
		}
	}
	// Pre-interaction sentinels must be present...
	for _, sentinel := range []string{"no-click", "no-input", "no-key"} {
		if !strings.Contains(p, sentinel) {
			t.Errorf("page is missing the pre-interaction sentinel %q", sentinel)
		}
	}
	// ...and the post-interaction markers must NOT already be in the served page,
	// or an input assertion would pass without any input having happened.
	for _, marker := range []string{"CLICK-RECEIVED", "KEY-RECEIVED:Escape"} {
		if strings.Count(p, marker) != 1 {
			t.Errorf("marker %q must appear exactly once (in its handler), not in the rendered text", marker)
		}
	}
	if strings.Contains(p, ">TYPED:") {
		t.Error("the typed marker must not be pre-rendered into the page body")
	}
}
