// Package bridge implements the nats-kv → nats-mq change-stream bridge per
// ADR-007 (nats-mq C9). On bucket-create with `change_stream_topic: <name>`,
// the control plane persists the mapping in kv-admin-change-streams-v1.
// This bridge:
//
//  1. Watches kv-admin-change-streams-v1 for mappings.
//  2. For each mapping (bucket_name → target_topic), subscribes to
//     $KV.<bucket_name>.> so every write echoes here.
//  3. Republishes the message to t.<tenant_id>__<target_topic>.<key> on the
//     same NATS cluster (shared-cluster deployment per ADR-002).
//  4. Cross-cluster deployments swap step 3 for an HTTP POST to
//     mq-adapter — same mapping schema; only the egress path changes.
//
// Tombstone (mapping delete) → bridge tears down the subscription.
package bridge

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strings"
	"sync"
	"time"

	"github.com/nats-io/nats.go"
)

const mappingBucket = "kv-admin-change-streams-v1"

type Mapping struct {
	TenantID    string    `json:"tenant_id"`
	BucketName  string    `json:"bucket_name"`
	TargetTopic string    `json:"target_topic"`
	CreatedAt   time.Time `json:"created_at"`
}

type ChangeStream struct {
	nc *nats.Conn
	js nats.JetStreamContext

	mu      sync.Mutex
	active  map[string]*activeBridge // bucket_name -> running subscription
}

type activeBridge struct {
	mapping Mapping
	sub     *nats.Subscription
}

func New(nc *nats.Conn, js nats.JetStreamContext) *ChangeStream {
	return &ChangeStream{
		nc:     nc,
		js:     js,
		active: make(map[string]*activeBridge),
	}
}

// Run blocks until ctx is cancelled, watching the mapping bucket and
// reconciling per-bucket bridges.
func (cs *ChangeStream) Run(ctx context.Context) error {
	kv, err := cs.js.KeyValue(mappingBucket)
	if err != nil {
		// No mappings yet → noop. Bucket gets created lazily by control plane
		// on first change_stream_topic use. Re-poll every 30s for first appearance.
		log.Printf("[bridge] mapping bucket not present; will retry: %v", err)
		t := time.NewTicker(30 * time.Second)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-t.C:
				if k, err := cs.js.KeyValue(mappingBucket); err == nil {
					kv = k
					goto watch
				}
			}
		}
	}
watch:
	w, err := kv.WatchAll(nats.Context(ctx))
	if err != nil {
		return fmt.Errorf("watch %s: %w", mappingBucket, err)
	}
	defer w.Stop()
	log.Printf("[bridge] watcher started (bucket=%s)", mappingBucket)
	for {
		select {
		case <-ctx.Done():
			cs.stopAll()
			return ctx.Err()
		case e, ok := <-w.Updates():
			if !ok {
				cs.stopAll()
				return nil
			}
			if e == nil {
				cs.mu.Lock()
				log.Printf("[bridge] caught up, %d active mappings", len(cs.active))
				cs.mu.Unlock()
				continue
			}
			switch e.Operation() {
			case nats.KeyValuePut:
				var m Mapping
				if err := json.Unmarshal(e.Value(), &m); err != nil {
					log.Printf("[bridge] decode %s: %v", e.Key(), err)
					continue
				}
				cs.startBridge(m)
			case nats.KeyValueDelete, nats.KeyValuePurge:
				cs.stopBridge(e.Key())
			}
		}
	}
}

func (cs *ChangeStream) startBridge(m Mapping) {
	cs.mu.Lock()
	if existing, ok := cs.active[m.BucketName]; ok && existing.mapping.TargetTopic == m.TargetTopic {
		cs.mu.Unlock()
		return // idempotent
	}
	cs.mu.Unlock()

	// Tear down old if topic changed
	cs.stopBridge(m.BucketName)

	subj := "$KV." + m.BucketName + ".>"
	// Use a NATS core subscription (no consumer state) — bridge is best-effort
	// fanout; durable replay is the responsibility of the destination MQ stream.
	sub, err := cs.nc.Subscribe(subj, func(msg *nats.Msg) {
		// $KV.<bucket>.<key> → t.<tenant_id>__<target_topic>.<key>
		key := strings.TrimPrefix(msg.Subject, "$KV."+m.BucketName+".")
		target := "t." + m.TenantID + "__" + m.TargetTopic + "." + key
		if err := cs.nc.Publish(target, msg.Data); err != nil {
			log.Printf("[bridge] publish %s: %v", target, err)
		}
	})
	if err != nil {
		log.Printf("[bridge] subscribe %s: %v", subj, err)
		return
	}
	cs.mu.Lock()
	cs.active[m.BucketName] = &activeBridge{mapping: m, sub: sub}
	cs.mu.Unlock()
	log.Printf("[bridge] %s → t.%s__%s.> active", m.BucketName, m.TenantID, m.TargetTopic)
}

func (cs *ChangeStream) stopBridge(bucketName string) {
	cs.mu.Lock()
	a, ok := cs.active[bucketName]
	if ok {
		delete(cs.active, bucketName)
	}
	cs.mu.Unlock()
	if !ok {
		return
	}
	_ = a.sub.Unsubscribe()
	log.Printf("[bridge] %s tombstoned", bucketName)
}

func (cs *ChangeStream) stopAll() {
	cs.mu.Lock()
	defer cs.mu.Unlock()
	for k, a := range cs.active {
		_ = a.sub.Unsubscribe()
		delete(cs.active, k)
	}
}

// ActiveCount for diagnostics.
func (cs *ChangeStream) ActiveCount() int {
	cs.mu.Lock()
	defer cs.mu.Unlock()
	return len(cs.active)
}
