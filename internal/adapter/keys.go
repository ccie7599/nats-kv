package adapter

import (
	"crypto/tls"
	"encoding/json"
	"io"
	"log"
	"net/http"
	"sync"
	"time"

	"github.com/nats-io/nats.go"
)

// KeyCache periodically polls the control plane for the active key list and
// keeps an in-memory map of hash -> tenant_id.
//
// We tried JetStream KV watch — it doesn't reliably replay history when the
// stream's R1 leader is on a different cluster-mesh peer than the adapter.
// HTTP polling is simpler, robust, and cheap (27 adapters × 2 polls/min = 54
// req/min hitting the control plane).
type KeyCache struct {
	mu    sync.RWMutex
	keys  map[string]string // hash -> tenant_id (active only)
	js    nats.JetStreamContext
	demo  string
	url   string
	last  time.Time
}

type apiKeyRecord struct {
	ID        string    `json:"id"`
	TenantID  string    `json:"tenant_id"`
	Hash      string    `json:"hash"`
	Label     string    `json:"label"`
	CreatedAt time.Time `json:"created_at"`
	RevokedAt time.Time `json:"revoked_at,omitempty"`
}

type keysResponse struct {
	Keys []apiKeyRecord `json:"keys"`
}

func NewKeyCache(js nats.JetStreamContext, demoToken, controlURL string) *KeyCache {
	c := &KeyCache{
		keys: make(map[string]string),
		js:   js,
		demo: demoToken,
		url:  controlURL,
	}
	go c.pollLoop()
	return c
}

func (c *KeyCache) pollLoop() {
	// Initial poll immediately, then every 30s.
	t := time.NewTicker(30 * time.Second)
	defer t.Stop()
	c.refresh()
	for range t.C {
		c.refresh()
	}
}

func (c *KeyCache) refresh() {
	if c.url == "" {
		return
	}
	req, _ := http.NewRequest("GET", c.url+"/v1/internal/keys", nil)
	cl := &http.Client{
		Timeout: 8 * time.Second,
		Transport: &http.Transport{
			TLSClientConfig: &tls.Config{InsecureSkipVerify: true}, // self-issued chain — not for prod
		},
	}
	resp, err := cl.Do(req)
	if err != nil {
		log.Printf("keys: poll error: %v", err)
		return
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		body, _ := io.ReadAll(resp.Body)
		log.Printf("keys: poll status %d: %s", resp.StatusCode, string(body))
		return
	}
	var body keysResponse
	if err := json.NewDecoder(resp.Body).Decode(&body); err != nil {
		log.Printf("keys: decode: %v", err)
		return
	}
	next := make(map[string]string, len(body.Keys))
	for _, k := range body.Keys {
		if !k.RevokedAt.IsZero() {
			continue
		}
		next[k.Hash] = k.TenantID
	}
	c.mu.Lock()
	c.keys = next
	c.last = time.Now()
	c.mu.Unlock()
	log.Printf("keys: refreshed, %d active", len(next))
}

// Validate returns (tenant_id, ok). Demo token returns "demo".
func (c *KeyCache) Validate(plaintext string) (string, bool) {
	if plaintext == c.demo {
		return "demo", true
	}
	hash := sha256Hex(plaintext)
	c.mu.RLock()
	t, ok := c.keys[hash]
	c.mu.RUnlock()
	return t, ok
}

// Size for debugging.
func (c *KeyCache) Size() int {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return len(c.keys)
}

func sha256Hex(s string) string {
	h := newSHA256()
	h.Write([]byte(s))
	sum := h.Sum(nil)
	const hex = "0123456789abcdef"
	out := make([]byte, len(sum)*2)
	for i, b := range sum {
		out[i*2] = hex[b>>4]
		out[i*2+1] = hex[b&0x0f]
	}
	return string(out)
}
