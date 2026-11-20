package main

import (
	"net/http"
	"net/http/httputil"
	"net/url"
)

// newSignallingProxy forwards the signalling WebSocket to the streamer's loopback
// server. httputil.ReverseProxy carries the Upgrade handshake and then copies both
// directions until either side closes, so ServeHTTP returns exactly when the
// browser's signalling session ends -- which is what makes the viewer count exact.
func newSignallingProxy(upstream *url.URL) *httputil.ReverseProxy {
	return &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			pr.SetURL(upstream)
			// The streamer's signalling server serves its WebSocket at "/".
			pr.Out.URL.Path = "/"
			pr.Out.URL.RawPath = ""
			// The session cookie authenticates the browser to the GATEWAY; the
			// streamer has no use for it, so it never leaves this process.
			pr.Out.Header.Del("Cookie")
		},
	}
}

// handleSignalling admits a browser to the stream only with a valid session, and
// only from this origin. SameSite=Strict already keeps the cookie off cross-site
// requests; the Origin check is the second lock, because a WebSocket is not
// subject to CORS and a browser will open one to any host.
func (g *gateway) handleSignalling(w http.ResponseWriter, r *http.Request) {
	if g.sessionUser(r) == "" {
		http.Error(w, "login required", http.StatusUnauthorized)
		return
	}
	if o := r.Header.Get("Origin"); o != "" {
		u, err := url.Parse(o)
		if err != nil || u.Host != r.Host {
			http.Error(w, "cross-origin signalling refused", http.StatusForbidden)
			return
		}
	}
	g.viewers.Add(1)
	defer g.viewers.Add(-1)
	g.proxy.ServeHTTP(w, r)
}
