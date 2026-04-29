// Package probe runs a periodic end-to-end replication-delay probe across the
// 27-region cluster. Every probeInterval it writes a timestamped payload to a
// shared probe bucket and direct-reads each region's mirror, computing
// (now - msg.ts) per region. The result is the user-visible "how stale is
// data in <region> right now" signal — much more honest than NATS's internal
// per-peer heartbeat counters, which only show keepalive cadence.
package probe

import (
	"context"
	"encoding/json"
	"errors"
	"log"
	"sync"
	"time"

	"github.com/bapley/project-nats-kv/internal/placement"
	"github.com/nats-io/nats.go"
)

const (
	BucketName    = "topology_probe"
	probeKey      = "ping"
	probeInterval = 5 * time.Second
	probeTimeout  = 3 * time.Second
)

// Result is the per-region freshness reading from the latest probe cycle.
type Result struct {
	Region    string    `json:"region"`
	DeltaMs   float64   `json:"delta_ms"`           // ms between probe write and the sample of this region's mirror
	StreamSeq uint64    `json:"stream_seq"`         // the seq of the last message visible at this region
	Source    string    `json:"source"`             // "local-mirror" | "raft-replica" | "source-fallback"
	Error     string    `json:"error,omitempty"`
	SampledAt time.Time `json:"sampled_at"`
}

// Snapshot is what the HTTP endpoint returns: the most recent probe cycle's
// per-region results plus metadata.
type Snapshot struct {
	LastProbeAt  time.Time         `json:"last_probe_at"`
	LastWriteSeq uint64            `json:"last_write_seq"`
	IntervalMs   int64             `json:"interval_ms"`
	Regions      map[string]Result `json:"regions"`
}

// Prober owns the probe loop. Construct via New, start with Run.
type Prober struct {
	js      nats.JetStreamContext
	regions []string

	mu   sync.RWMutex
	snap Snapshot
}

// New returns a Prober that will sample every region in placement.AllRegions.
func New(js nats.JetStreamContext) *Prober {
	return &Prober{
		js:      js,
		regions: placement.AllRegions,
		snap: Snapshot{
			IntervalMs: probeInterval.Milliseconds(),
			Regions:    map[string]Result{},
		},
	}
}

// Snapshot returns a copy of the latest probe results.
func (p *Prober) Snapshot() Snapshot {
	p.mu.RLock()
	defer p.mu.RUnlock()
	out := p.snap
	out.Regions = make(map[string]Result, len(p.snap.Regions))
	for k, v := range p.snap.Regions {
		out.Regions[k] = v
	}
	return out
}

// Run blocks running the probe loop until ctx is cancelled. Use as a goroutine
// from cmd/control/main.go after the topology_probe bucket has been bootstrapped.
func (p *Prober) Run(ctx context.Context) {
	// Fire one immediately so the snapshot isn't empty at startup.
	p.probe(ctx)
	t := time.NewTicker(probeInterval)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			p.probe(ctx)
		}
	}
}

func (p *Prober) probe(ctx context.Context) {
	now := time.Now()
	payload, _ := json.Marshal(map[string]any{"ts_unix_nanos": now.UnixNano()})

	kv, err := p.js.KeyValue(BucketName)
	if err != nil {
		log.Printf("probe: kv open: %v", err)
		return
	}
	rev, err := kv.Put(probeKey, payload)
	if err != nil {
		log.Printf("probe: put: %v", err)
		return
	}

	results := make(map[string]Result, len(p.regions))
	var rmu sync.Mutex
	var wg sync.WaitGroup
	for _, region := range p.regions {
		wg.Add(1)
		go func(region string) {
			defer wg.Done()
			r := p.sampleRegion(ctx, region, now)
			rmu.Lock()
			results[region] = r
			rmu.Unlock()
		}(region)
	}
	wg.Wait()

	p.mu.Lock()
	p.snap.LastProbeAt = now
	p.snap.LastWriteSeq = rev
	p.snap.Regions = results
	p.mu.Unlock()
}

// sampleRegion direct-gets the latest probe message from the named region's
// mirror stream (or, if that region hosts a RAFT replica of the source and
// thus has no mirror, from the source by name). Computes delta = now - ts.
func (p *Prober) sampleRegion(ctx context.Context, region string, writeStart time.Time) Result {
	mirrorName := "KV_" + BucketName + "_mirror_" + region
	subject := "$KV." + BucketName + "." + probeKey

	ctx2, cancel := context.WithTimeout(ctx, probeTimeout)
	defer cancel()

	source := "local-mirror"
	msg, err := p.js.GetLastMsg(mirrorName, subject, nats.DirectGet(), nats.Context(ctx2))
	if err != nil && (errors.Is(err, nats.ErrStreamNotFound) || errors.Is(err, nats.ErrMsgNotFound)) {
		// Region hosts a RAFT replica of the source (we don't create mirrors
		// where the source already lives). Fall back to addressing the source
		// stream — in that case the answer is "synchronous", delta ~0ms.
		source = "raft-replica"
		msg, err = p.js.GetLastMsg("KV_"+BucketName, subject, nats.DirectGet(), nats.Context(ctx2))
	}
	if err != nil {
		return Result{Region: region, Error: err.Error(), SampledAt: time.Now()}
	}

	var inner struct {
		TsUnixNanos int64 `json:"ts_unix_nanos"`
	}
	if jerr := json.Unmarshal(msg.Data, &inner); jerr != nil {
		return Result{Region: region, Error: "decode: " + jerr.Error(), SampledAt: time.Now()}
	}

	sampledAt := time.Now()
	// Delta from the write moment we just performed; that's the freshest
	// possible "what's the propagation delay right now" reading.
	delta := sampledAt.Sub(writeStart)
	// If this region was sampled with a *previous* iteration's payload (the
	// mirror hadn't replicated yet), use the message timestamp instead so the
	// number reflects how stale the data is.
	if inner.TsUnixNanos != writeStart.UnixNano() {
		delta = sampledAt.Sub(time.Unix(0, inner.TsUnixNanos))
	}
	return Result{
		Region:    region,
		DeltaMs:   float64(delta.Microseconds()) / 1000.0,
		StreamSeq: msg.Sequence,
		Source:    source,
		SampledAt: sampledAt,
	}
}
