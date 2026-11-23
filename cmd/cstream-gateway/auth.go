package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"log"
	"net/http"
	"os/exec"
	"regexp"
	"sync"
	"time"
)

// SessionCookie names the cookie that carries a browser's session token.
const SessionCookie = "cstream_session"

// leaderTimeout bounds one PAM authentication. A bound, not a delay: the wait ends
// the moment the leader exits, and a leader wedged on a PAM module fails the login
// instead of holding the request forever.
const leaderTimeout = 15 * time.Second

// maxLoginBody bounds the login form. A credential is a few hundred bytes at most.
const maxLoginBody = 4 << 10

// validUser is the POSIX portable user-name shape. It is checked BEFORE the name
// reaches the leader's argv, so a crafted name can never look like a flag.
var validUser = regexp.MustCompile(`^[a-z_][a-z0-9_-]{0,31}$`)

type session struct {
	user    string
	expires time.Time
}

type sessionStore struct {
	mu  sync.Mutex
	ttl time.Duration
	m   map[string]session
	now func() time.Time
}

func newSessionStore(ttl time.Duration) *sessionStore {
	return &sessionStore{ttl: ttl, m: map[string]session{}, now: time.Now}
}

func (s *sessionStore) create(user string) (string, error) {
	b := make([]byte, 32)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	tok := hex.EncodeToString(b)
	s.mu.Lock()
	defer s.mu.Unlock()
	// Purge on the only path that GROWS the map. lookup's on-sight deletion cannot
	// reclaim an abandoned browser (it never calls again), so without this the map
	// grows by one entry per login that is never logged out or re-looked-up.
	s.purgeExpiredLocked()
	s.m[tok] = session{user: user, expires: s.now().Add(s.ttl)}
	return tok, nil
}

// purgeExpiredLocked drops every expired session. Callers must hold s.mu.
func (s *sessionStore) purgeExpiredLocked() {
	now := s.now()
	for tok, ss := range s.m {
		if !now.Before(ss.expires) {
			delete(s.m, tok)
		}
	}
}

// lookup returns the session's user, or "" when the token is unknown or expired.
// An expired token is deleted on sight. That alone does NOT bound the map: an
// abandoned browser never comes back, so create purges expired entries on every
// insert, which is the only place the map grows.
func (s *sessionStore) lookup(tok string) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	ss, ok := s.m[tok]
	if !ok {
		return ""
	}
	if !s.now().Before(ss.expires) {
		delete(s.m, tok)
		return ""
	}
	return ss.user
}

func (s *sessionStore) drop(tok string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.m, tok)
}

// sessionUser returns the authenticated user of a request, or "".
func (g *gateway) sessionUser(r *http.Request) string {
	c, err := r.Cookie(SessionCookie)
	if err != nil || c.Value == "" {
		return ""
	}
	return g.sessions.lookup(c.Value)
}

func (g *gateway) handleSession(w http.ResponseWriter, r *http.Request) {
	user := g.sessionUser(r)
	if user == "" {
		http.Error(w, "not logged in", http.StatusUnauthorized)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]string{"user": user})
}

// errLoginRejected is a refused credential; every other error is the leader failing.
var errLoginRejected = errors.New("authentication refused")

// authenticate runs the REAL session leader, the same binary and PAM service the
// login probe drives. The password goes to the leader's stdin, never its argv:
// argv is world-readable through /proc/<pid>/cmdline.
//
// The leader's exit code is its contract: 0 authenticated, 1 refused, anything
// else a usage or environment error.
func (g *gateway) authenticate(ctx context.Context, user, password string) error {
	ctx, cancel := context.WithTimeout(ctx, leaderTimeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, g.cfg.Leader, g.cfg.PAMService, user)
	cmd.Stdin = bytes.NewBufferString(password + "\n")
	var out bytes.Buffer
	cmd.Stdout = &out
	cmd.Stderr = &out
	err := cmd.Run()
	var exit *exec.ExitError
	switch {
	case err == nil:
		return nil
	case errors.As(err, &exit) && exit.ExitCode() == 1:
		return errLoginRejected
	default:
		return errors.New("session leader failed: " + err.Error() + ": " + out.String())
	}
}

func (g *gateway) handleLogin(w http.ResponseWriter, r *http.Request) {
	r.Body = http.MaxBytesReader(w, r.Body, maxLoginBody)
	if err := r.ParseForm(); err != nil {
		http.Error(w, "bad login form", http.StatusBadRequest)
		return
	}
	user, password := r.PostForm.Get("user"), r.PostForm.Get("password")
	if !validUser.MatchString(user) || password == "" {
		http.Error(w, "invalid user name or empty password", http.StatusBadRequest)
		return
	}
	switch err := g.authenticate(r.Context(), user, password); {
	case errors.Is(err, errLoginRejected):
		http.Error(w, "login refused", http.StatusUnauthorized)
		return
	case err != nil:
		log.Printf("cstream-gateway: login for %q: %v", user, err)
		http.Error(w, "login unavailable", http.StatusInternalServerError)
		return
	}
	tok, err := g.sessions.create(user)
	if err != nil {
		http.Error(w, "session unavailable", http.StatusInternalServerError)
		return
	}
	http.SetCookie(w, &http.Cookie{
		Name:     SessionCookie,
		Value:    tok,
		Path:     "/",
		HttpOnly: true,
		Secure:   r.TLS != nil,
		SameSite: http.SameSiteStrictMode,
		MaxAge:   int(g.cfg.SessionTTL / time.Second),
	})
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]string{"user": user})
}

func (g *gateway) handleLogout(w http.ResponseWriter, r *http.Request) {
	if c, err := r.Cookie(SessionCookie); err == nil {
		g.sessions.drop(c.Value)
	}
	http.SetCookie(w, &http.Cookie{Name: SessionCookie, Value: "", Path: "/", MaxAge: -1, HttpOnly: true, SameSite: http.SameSiteStrictMode})
	w.WriteHeader(http.StatusNoContent)
}
