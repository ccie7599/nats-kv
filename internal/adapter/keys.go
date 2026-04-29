package adapter

import (
	"encoding/json"
	"log"
	"sync"
	"time"

	"github.com/nats-io/nats.go"
)

// KeyCache watches the kv-admin-keys bucket and maintains an in-memory map of
// active key hashes -> tenant ID. Used by the adapter's auth middleware to
// accept any control-plane-issued bearer token without per-request lookup.
//
// Sub-100ms global propagation: control plane writes a key into the bucket;
// every adapter's watch fires; in-memory map updates atomically.
type KeyCache struct {
	mu    sync.RWMutex
	keys  map[string]string // hash -> tenant_id (active only)
	js    nats.JetStreamContext
	demo  string // shared demo token for backwards compat (always accepted)
}

type apiKeyRecord struct {
	ID        string    `json:"id"`
	TenantID  string    `json:"tenant_id"`
	Hash      string    `json:"hash"`
	Label     string    `json:"label"`
	CreatedAt time.Time `json:"created_at"`
	RevokedAt time.Time `json:"revoked_at,omitempty"`
}

// NewKeyCache opens (or waits for) the kv-admin-keys bucket and starts a watch
// that updates the in-memory map. Returns immediately even if the bucket
// doesn't exist yet — re-tries every 10s in the background until it appears.
func NewKeyCache(js nats.JetStreamContext, demoToken string) *KeyCache {
	c := &KeyCache{
		keys: make(map[string]string),
		js:   js,
		demo: demoToken,
	}
	go c.run()
	return c
}

func (c *KeyCache) run() {
	for {
		kv, err := c.js.KeyValue("kv-admin-keys")
		if err != nil {
			// Bucket may not be replicated to this node yet, or control plane hasn't started.
			time.Sleep(10 * time.Second)
			continue
		}
		w, err := kv.WatchAll()
		if err != nil {
			log.Printf("keys watch error: %v — retrying in 10s", err)
			time.Sleep(10 * time.Second)
			continue
		}
		log.Printf("keys watch started on kv-admin-keys")
		for upd := range w.Updates() {
			if upd == nil {
				// nil = end of initial replay, watch is now live
				continue
			}
			c.apply(upd)
		}
		log.Printf("keys watch closed — restarting")
		time.Sleep(2 * time.Second)
	}
}

func (c *KeyCache) apply(e nats.KeyValueEntry) {
	hash := e.Key()
	switch e.Operation() {
	case nats.KeyValueDelete, nats.KeyValuePurge:
		c.mu.Lock()
		delete(c.keys, hash)
		c.mu.Unlock()
		return
	}
	var rec apiKeyRecord
	if err := json.Unmarshal(e.Value(), &rec); err != nil {
		log.Printf("keys: bad record %s: %v", hash, err)
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if !rec.RevokedAt.IsZero() {
		delete(c.keys, hash)
	} else {
		c.keys[hash] = rec.TenantID
	}
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
	h := sha256New()
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

// indirection to keep import list clean
func sha256New() hashWriter { return newSHA256() }
