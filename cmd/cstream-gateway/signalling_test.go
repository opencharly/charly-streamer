package main

import (
	"bufio"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"testing"
	"time"
)

// echoUpgradeServer stands in for the streamer's signalling server: it accepts an
// HTTP Upgrade, records the request it saw, then echoes bytes. That is the whole
// contract the proxy relies on -- an upgrade followed by an opaque byte stream.
type echoUpgradeServer struct {
	*httptest.Server
	mu   sync.Mutex
	seen []*http.Request
}

func newEchoUpgradeServer(t *testing.T) *echoUpgradeServer {
	e := &echoUpgradeServer{}
	e.Server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		e.mu.Lock()
		e.seen = append(e.seen, r.Clone(r.Context()))
		e.mu.Unlock()
		conn, rw, err := w.(http.Hijacker).Hijack()
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.Close()
		fmt.Fprint(rw, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
		rw.Flush()
		_, _ = io.Copy(conn, rw)
	}))
	return e
}

// dialSignalling opens a raw upgrade request to the gateway's /signalling.
func dialSignalling(t *testing.T, gw *httptest.Server, cookie *http.Cookie, origin string) (net.Conn, *bufio.Reader, int) {
	t.Helper()
	u, _ := url.Parse(gw.URL)
	conn, err := net.Dial("tcp", u.Host)
	if err != nil {
		t.Fatal(err)
	}
	req := "GET /signalling HTTP/1.1\r\nHost: " + u.Host + "\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
		"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
	if cookie != nil {
		req += "Cookie: " + cookie.Name + "=" + cookie.Value + "\r\n"
	}
	if origin != "" {
		req += "Origin: " + origin + "\r\n"
	}
	if _, err := io.WriteString(conn, req+"\r\n"); err != nil {
		t.Fatal(err)
	}
	br := bufio.NewReader(conn)
	resp, err := http.ReadResponse(br, nil)
	if err != nil {
		t.Fatal(err)
	}
	return conn, br, resp.StatusCode
}

func loggedInCookie(t *testing.T, g *gateway) *http.Cookie {
	t.Helper()
	tok, err := g.sessions.create("alice")
	if err != nil {
		t.Fatal(err)
	}
	return &http.Cookie{Name: SessionCookie, Value: tok}
}

func TestSignallingRefusesAnonymousBrowsers(t *testing.T) {
	up := newEchoUpgradeServer(t)
	defer up.Close()
	g, _ := newTestGateway(t, up.URL)
	gw := httptest.NewServer(g.routes())
	defer gw.Close()
	conn, _, code := dialSignalling(t, gw, nil, "")
	conn.Close()
	if code != http.StatusUnauthorized {
		t.Fatalf("anonymous /signalling = %d, want 401", code)
	}
	if len(up.seen) != 0 {
		t.Fatal("an anonymous request reached the streamer's signalling server")
	}
}

func TestSignallingRefusesCrossOrigin(t *testing.T) {
	up := newEchoUpgradeServer(t)
	defer up.Close()
	g, _ := newTestGateway(t, up.URL)
	gw := httptest.NewServer(g.routes())
	defer gw.Close()
	conn, _, code := dialSignalling(t, gw, loggedInCookie(t, g), "http://evil.example")
	conn.Close()
	if code != http.StatusForbidden {
		t.Fatalf("cross-origin /signalling = %d, want 403", code)
	}
}

// With a session, the upgrade reaches the streamer at "/", the gateway's cookie is
// stripped, bytes flow both ways, and the viewer count tracks the open session.
func TestSignallingProxiesAnAuthenticatedUpgradeAndCountsTheViewer(t *testing.T) {
	up := newEchoUpgradeServer(t)
	defer up.Close()
	g, _ := newTestGateway(t, up.URL)
	gw := httptest.NewServer(g.routes())
	defer gw.Close()
	u, _ := url.Parse(gw.URL)
	conn, br, code := dialSignalling(t, gw, loggedInCookie(t, g), "http://"+u.Host)
	if code != http.StatusSwitchingProtocols {
		t.Fatalf("authenticated /signalling = %d, want 101", code)
	}
	if _, err := io.WriteString(conn, "ping\n"); err != nil {
		t.Fatal(err)
	}
	line, err := br.ReadString('\n')
	if err != nil || line != "ping\n" {
		t.Fatalf("echo through the proxy = %q, %v", line, err)
	}
	if v := g.viewers.Load(); v != 1 {
		t.Fatalf("viewers during the session = %d, want 1", v)
	}
	up.mu.Lock()
	r := up.seen[0]
	up.mu.Unlock()
	if r.URL.Path != "/" {
		t.Errorf("upstream saw path %q, want /", r.URL.Path)
	}
	if r.Header.Get("Cookie") != "" || strings.Contains(r.Header.Get("Cookie"), SessionCookie) {
		t.Error("the gateway's session cookie leaked to the streamer")
	}
	conn.Close()
	// The proxy's ServeHTTP returns once both copy directions see the close; wait
	// for THAT condition, bounded, rather than for a fixed time.
	deadline := time.Now().Add(5 * time.Second)
	for g.viewers.Load() != 0 {
		if time.Now().After(deadline) {
			t.Fatalf("viewers after close = %d, want 0", g.viewers.Load())
		}
		time.Sleep(10 * time.Millisecond)
	}
}
