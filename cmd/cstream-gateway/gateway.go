package main

import (
	"embed"
	"encoding/json"
	"fmt"
	"io/fs"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"path/filepath"
	"sync/atomic"
	"time"
)

//go:embed web
var webFS embed.FS

// StreamerStatusFile is the file the streamer writes into XDG_RUNTIME_DIR once its
// encoder has OPENED (not merely registered). The gateway reports it verbatim, so the
// encoder a check sees is the one the streamer actually runs, not a guess made here.
const StreamerStatusFile = "cstream-streamer.json"

type gatewayConfig struct {
	RenderNode string
	RuntimeDir string
	Display    string
	Signalling string
	Leader     string
	PAMService string
	SessionTTL time.Duration
}

type gateway struct {
	cfg      gatewayConfig
	sessions *sessionStore
	proxy    *httputil.ReverseProxy
	viewers  atomic.Int64
}

func newGateway(cfg gatewayConfig) (*gateway, error) {
	up, err := url.Parse(cfg.Signalling)
	if err != nil || up.Host == "" {
		return nil, fmt.Errorf("invalid signalling upstream %q", cfg.Signalling)
	}
	if up.Scheme != "http" {
		return nil, fmt.Errorf("signalling upstream %q must be http:// (the streamer's loopback server)", cfg.Signalling)
	}
	g := &gateway{cfg: cfg, sessions: newSessionStore(cfg.SessionTTL)}
	g.proxy = newSignallingProxy(up)
	return g, nil
}

func (g *gateway) routes() http.Handler {
	mux := http.NewServeMux()
	static, _ := fs.Sub(webFS, "web")
	mux.Handle("GET /", http.FileServerFS(static))
	mux.HandleFunc("GET /healthz", g.handleHealthz)
	mux.HandleFunc("GET /session", g.handleSession)
	mux.HandleFunc("POST /login", g.handleLogin)
	mux.HandleFunc("POST /logout", g.handleLogout)
	mux.HandleFunc("GET /signalling", g.handleSignalling)
	return mux
}

// Status is what /healthz reports. It is a struct rather than an ad-hoc map so the
// shape is checkable from a test and from a `cstream: status` probe, which returns
// this body verbatim.
type Status struct {
	Service    string `json:"service"`
	Ready      bool   `json:"ready"`
	RenderNode string `json:"render_node"`
	Display    string `json:"display"`
	// Encoder is the streamer's own report of the encoder that OPENED, e.g.
	// "hardware:nvh264enc"; empty until the streamer has written it.
	Encoder string `json:"encoder"`
	// Viewers is the number of browsers currently holding a signalling session.
	Viewers int64 `json:"viewers"`
}

// probeStatus reports what the gateway can actually observe, not what it hopes.
//
// Ready means the two things a browser client genuinely depends on: a DRM render
// node exists (there is no software fallback that can host a nested compositor)
// and the compositor has published a Wayland socket. Reporting ready without
// those would let a page load and then stream nothing.
func probeStatus(renderNode, runtimeDir, display string) Status {
	s := Status{Service: "cstream-gateway", RenderNode: renderNode, Display: display}
	s.Encoder = readStreamerEncoder(runtimeDir)
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

// readStreamerEncoder returns the encoder label the streamer published, or "" when
// it has not (yet) published one. An unreadable or malformed file reads as "": the
// field is a report of what the streamer said, never a value the gateway invents.
func readStreamerEncoder(runtimeDir string) string {
	b, err := os.ReadFile(filepath.Join(runtimeDir, StreamerStatusFile))
	if err != nil {
		return ""
	}
	var st struct {
		Encoder string `json:"encoder"`
	}
	if json.Unmarshal(b, &st) != nil {
		return ""
	}
	return st.Encoder
}

func (g *gateway) handleHealthz(w http.ResponseWriter, _ *http.Request) {
	s := probeStatus(g.cfg.RenderNode, g.cfg.RuntimeDir, g.cfg.Display)
	s.Viewers = g.viewers.Load()
	w.Header().Set("Content-Type", "application/json")
	if !s.Ready {
		w.WriteHeader(http.StatusServiceUnavailable)
	}
	_ = json.NewEncoder(w).Encode(s)
}
