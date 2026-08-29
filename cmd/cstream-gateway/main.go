// cstream-gateway serves the browser-facing surface of a cstream desktop.
//
// It is deliberately dependency-free and CGO_ENABLED=0: it is the one component
// exposed to the network, so its build should be reproducible and its attack
// surface small.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"time"
)

// Status is what /healthz and the page report. It is deliberately a struct rather
// than an ad-hoc map so the shape is checkable from a test and from a cdp: probe.
type Status struct {
	Service    string `json:"service"`
	Ready      bool   `json:"ready"`
	RenderNode string `json:"render_node"`
	Display    string `json:"display"`
}

// probeStatus reports what the gateway can actually observe, not what it hopes.
//
// Ready means the two things a browser client genuinely depends on: a DRM render
// node exists (there is no software fallback that can host a nested compositor)
// and the compositor has published a Wayland socket. Reporting ready without
// those would let a page load and then stream nothing.
func probeStatus(renderNode, runtimeDir, display string) Status {
	s := Status{Service: "cstream-gateway", RenderNode: renderNode, Display: display}
	if renderNode == "" || display == "" {
		return s
	}
	if _, err := os.Stat(renderNode); err != nil {
		return s
	}
	if _, err := os.Stat(filepath.Join(runtimeDir, display)); err != nil {
		return s
	}
	s.Ready = true
	return s
}

// page renders the browser surface. The marker is what a cdp: text probe asserts;
// it is only emitted when the stack is genuinely ready, so a CDP check cannot pass
// against a half-started pod.
func page(s Status) string {
	state := "NOT-READY"
	if s.Ready {
		state = "CSTREAM-READY"
	}
	return fmt.Sprintf(`<!doctype html>
<title>cstream</title>
<h1>cstream</h1>
<p id="state">%s</p>
<p id="render-node">%s</p>
<p id="display">%s</p>
`, state, s.RenderNode, s.Display)
}

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	renderNode := flag.String("render-node", envOr("CSTREAM_RENDER_NODE", "/dev/dri/renderD128"), "DRM render node")
	runtimeDir := flag.String("runtime-dir", envOr("XDG_RUNTIME_DIR", "/tmp/cstream-rt"), "XDG runtime dir")
	display := flag.String("display", envOr("WAYLAND_DISPLAY", "wayland-1"), "wayland display")
	flag.Parse()

	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		s := probeStatus(*renderNode, *runtimeDir, *display)
		w.Header().Set("Content-Type", "application/json")
		if !s.Ready {
			w.WriteHeader(http.StatusServiceUnavailable)
		}
		_ = json.NewEncoder(w).Encode(s)
	})
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		_, _ = w.Write([]byte(page(probeStatus(*renderNode, *runtimeDir, *display))))
	})

	srv := &http.Server{
		Addr:              *addr,
		Handler:           mux,
		ReadHeaderTimeout: 10 * time.Second,
	}
	log.Printf("cstream-gateway listening on %s (render-node=%s display=%s)", *addr, *renderNode, *display)
	log.Fatal(srv.ListenAndServe())
}

func envOr(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}
