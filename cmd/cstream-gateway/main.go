// cstream-gateway serves the browser-facing surface of a cstream desktop.
//
// It is the ONE component exposed to the network, so it stays dependency-free and
// CGO_ENABLED=0 (a reproducible build, a small attack surface), and it is the only
// path to the stream: the streamer's signalling server listens on loopback, and the
// gateway proxies it to authenticated browsers only.
//
// Routes:
//
//	GET  /            the player (login form, <video>, input capture)
//	GET  /healthz     readiness, the active encoder and the viewer count (JSON)
//	GET  /session     200 + the user when the session cookie is valid, 401 otherwise
//	POST /login       PAM authentication through cstream-leader; sets the session cookie
//	POST /logout      drops the session
//	GET  /signalling  the WebRTC signalling WebSocket, proxied to the streamer (auth required)
package main

import (
	"flag"
	"log"
	"net/http"
	"os"
	"time"
)

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	// The streamer's rule: CSTREAM_RENDER_NODE when the deployment names one, else
	// charly's injected DRINODE. No literal default -- an unset node reports not-ready.
	renderNode := flag.String("render-node", envOr("CSTREAM_RENDER_NODE", os.Getenv("DRINODE")), "DRM render node (CSTREAM_RENDER_NODE, else DRINODE)")
	runtimeDir := flag.String("runtime-dir", envOr("XDG_RUNTIME_DIR", "/tmp/cstream-rt"), "XDG runtime dir")
	display := flag.String("display", envOr("WAYLAND_DISPLAY", "wayland-1"), "wayland display")
	signalling := flag.String("signalling", envOr("CSTREAM_SIGNALLING_UPSTREAM", "http://127.0.0.1:8443"), "the streamer's loopback signalling server")
	leader := flag.String("leader", envOr("CSTREAM_LEADER", "/usr/local/bin/cstream-leader"), "the PAM session leader binary")
	pamService := flag.String("pam-service", envOr("CSTREAM_PAM_SERVICE", "cstream"), "the PAM service the leader authenticates against")
	flag.Parse()

	g, err := newGateway(gatewayConfig{
		RenderNode: *renderNode,
		RuntimeDir: *runtimeDir,
		Display:    *display,
		Signalling: *signalling,
		Leader:     *leader,
		PAMService: *pamService,
		SessionTTL: 12 * time.Hour,
	})
	if err != nil {
		log.Fatalf("cstream-gateway: %v", err)
	}

	srv := &http.Server{
		Addr:              *addr,
		Handler:           g.routes(),
		ReadHeaderTimeout: 10 * time.Second,
	}
	log.Printf("cstream-gateway listening on %s (render-node=%s display=%s signalling=%s)",
		*addr, *renderNode, *display, *signalling)
	log.Fatal(srv.ListenAndServe())
}

func envOr(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}
