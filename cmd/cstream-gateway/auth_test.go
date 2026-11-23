package main

import (
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// fakeLeader is a stand-in for cstream-leader with the SAME contract: argv is
// <service> <user>, the password arrives on stdin, exit 0 accepts, 1 refuses and
// anything else is an environment error. It records its argv so a test can prove
// the password never appears there.
const fakeLeader = `#!/bin/sh
printf '%s\n' "$*" >> "$(dirname "$0")/argv.log"
read -r pw
case "$2:$pw" in
  alice:good) exit 0 ;;
  broken:*) exit 2 ;;
  *) exit 1 ;;
esac
`

func newTestGateway(t *testing.T, signalling string) (*gateway, string) {
	t.Helper()
	dir := t.TempDir()
	leader := filepath.Join(dir, "leader")
	if err := os.WriteFile(leader, []byte(fakeLeader), 0o755); err != nil {
		t.Fatal(err)
	}
	if signalling == "" {
		signalling = "http://127.0.0.1:1"
	}
	g, err := newGateway(gatewayConfig{
		RuntimeDir: dir,
		Display:    "wayland-1",
		Signalling: signalling,
		Leader:     leader,
		PAMService: "cstream",
		SessionTTL: time.Hour,
	})
	if err != nil {
		t.Fatal(err)
	}
	return g, dir
}

func postLogin(t *testing.T, srv *httptest.Server, user, pw string) *http.Response {
	t.Helper()
	resp, err := http.PostForm(srv.URL+"/login", url.Values{"user": {user}, "password": {pw}})
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	return resp
}

func sessionCookie(resp *http.Response) *http.Cookie {
	for _, c := range resp.Cookies() {
		if c.Name == SessionCookie {
			return c
		}
	}
	return nil
}

func TestLoginAcceptsAndIssuesAStrictHttpOnlyCookie(t *testing.T) {
	g, _ := newTestGateway(t, "")
	srv := httptest.NewServer(g.routes())
	defer srv.Close()
	resp := postLogin(t, srv, "alice", "good")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("good credential: status %d", resp.StatusCode)
	}
	c := sessionCookie(resp)
	if c == nil || c.Value == "" {
		t.Fatal("no session cookie issued")
	}
	if !c.HttpOnly || c.SameSite != http.SameSiteStrictMode {
		t.Fatalf("cookie must be HttpOnly + SameSite=Strict, got %+v", c)
	}
	if g.sessions.lookup(c.Value) != "alice" {
		t.Fatal("the issued token does not resolve to the user")
	}
}

func TestLoginRefusesABadPasswordWithoutASession(t *testing.T) {
	g, _ := newTestGateway(t, "")
	srv := httptest.NewServer(g.routes())
	defer srv.Close()
	resp := postLogin(t, srv, "alice", "wrong")
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("bad credential: status %d, want 401", resp.StatusCode)
	}
	if sessionCookie(resp) != nil {
		t.Fatal("a refused login must not issue a cookie")
	}
}

// A leader that fails for an environment reason is a 500, not a 401: telling the
// user "wrong password" when PAM itself is broken sends them looking at the wrong
// thing.
func TestLeaderFailureIsNotReportedAsABadPassword(t *testing.T) {
	g, _ := newTestGateway(t, "")
	srv := httptest.NewServer(g.routes())
	defer srv.Close()
	if resp := postLogin(t, srv, "broken", "x"); resp.StatusCode != http.StatusInternalServerError {
		t.Fatalf("leader environment failure: status %d, want 500", resp.StatusCode)
	}
}

// The password goes to the leader's stdin, never its argv, which any local user
// can read through /proc/<pid>/cmdline. And a crafted user name never reaches the
// leader at all.
func TestPasswordNeverInArgvAndUserIsValidated(t *testing.T) {
	g, dir := newTestGateway(t, "")
	srv := httptest.NewServer(g.routes())
	defer srv.Close()
	postLogin(t, srv, "alice", "good")
	for _, bad := range []string{"--help", "Alice", "a b", "../etc", ""} {
		if resp := postLogin(t, srv, bad, "good"); resp.StatusCode != http.StatusBadRequest {
			t.Errorf("user %q: status %d, want 400", bad, resp.StatusCode)
		}
	}
	argv, err := os.ReadFile(filepath.Join(dir, "argv.log"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(argv), "good") {
		t.Fatalf("the password reached the leader's argv: %q", argv)
	}
	if strings.TrimSpace(string(argv)) != "cstream alice" {
		t.Fatalf("leader argv = %q, want exactly one call: cstream alice", argv)
	}
}

func TestSessionEndpointAndLogout(t *testing.T) {
	g, _ := newTestGateway(t, "")
	srv := httptest.NewServer(g.routes())
	defer srv.Close()
	get := func(c *http.Cookie) int {
		req, _ := http.NewRequest("GET", srv.URL+"/session", nil)
		if c != nil {
			req.AddCookie(c)
		}
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		return resp.StatusCode
	}
	if code := get(nil); code != http.StatusUnauthorized {
		t.Fatalf("no cookie: /session = %d, want 401", code)
	}
	c := sessionCookie(postLogin(t, srv, "alice", "good"))
	if code := get(c); code != http.StatusOK {
		t.Fatalf("valid cookie: /session = %d, want 200", code)
	}
	req, _ := http.NewRequest("POST", srv.URL+"/logout", nil)
	req.AddCookie(c)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if code := get(c); code != http.StatusUnauthorized {
		t.Fatalf("after logout: /session = %d, want 401", code)
	}
}

func TestSessionsExpire(t *testing.T) {
	s := newSessionStore(time.Minute)
	now := time.Unix(1000, 0)
	s.now = func() time.Time { return now }
	tok, err := s.create("alice")
	if err != nil {
		t.Fatal(err)
	}
	if s.lookup(tok) != "alice" {
		t.Fatal("fresh session not found")
	}
	now = now.Add(time.Minute)
	if s.lookup(tok) != "" {
		t.Fatal("a session must expire at its TTL")
	}
	if len(s.m) != 0 {
		t.Fatal("an expired session must be deleted, not kept")
	}
}

// An abandoned browser never calls lookup again, so lookup's on-sight deletion cannot
// reclaim its entry: the store must purge expired sessions on the path that grows it
// (review of #24, Block 2). Without the purge in create, this test fails with three
// lingering entries.
func TestCreatePurgesExpiredSessions(t *testing.T) {
	s := newSessionStore(time.Minute)
	now := time.Now()
	s.now = func() time.Time { return now }

	for i := 0; i < 3; i++ {
		if _, err := s.create("probe"); err != nil {
			t.Fatalf("create: %v", err)
		}
	}
	// Every one of them expires without ever being looked up again.
	now = now.Add(2 * time.Minute)
	if _, err := s.create("probe"); err != nil {
		t.Fatalf("create: %v", err)
	}

	s.mu.Lock()
	got := len(s.m)
	s.mu.Unlock()
	if got != 1 {
		t.Fatalf("after the expired ones were abandoned and a new session created, the store holds %d sessions, want 1", got)
	}
}
