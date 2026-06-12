package adapter

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nuid"
)

// Atomic batch publishing (NATS 2.12, ADR-50) for the hot KV write path.
//
// Plain PUTs (no CAS, no TTL) are coalesced per bucket into one atomic batch:
// staged messages carry Nats-Batch-Id/-Sequence with no reply subject, the
// final message adds Nats-Batch-Commit and its single PubAck covers the whole
// batch. One RAFT append + fsync per batch instead of per write. HTTP 200s are
// released only after the commit ack, so durability semantics are unchanged.
//
// The target stream needs allow_atomic=true; a commit against a stream without
// it fails the whole batch (every caller gets the error — nothing is dropped
// silently). Batching is therefore opt-in per bucket via BATCH_BUCKETS.
//
// Env:
//   BATCH_BUCKETS    comma-separated bucket names to batch ("" disables)
//   BATCH_MAX        max messages per batch         (default 128, server cap 1000)
//   BATCH_WINDOW_US  flush window after first write (default 1000µs)

const (
	batchIdHdr     = "Nats-Batch-Id"
	batchSeqHdr    = "Nats-Batch-Sequence"
	batchCommitHdr = "Nats-Batch-Commit"
)

type batchPut struct {
	subj string
	data []byte
	resp chan batchResult
}

type batchResult struct {
	rev uint64
	err error
}

type batcher struct {
	nc      *nats.Conn
	js      nats.JetStreamContext
	max     int
	window  time.Duration
	buckets map[string]bool

	mu     sync.Mutex
	queues map[string]chan batchPut // bucket -> queue (one flusher each)
}

func newBatcherFromEnv(nc *nats.Conn, js nats.JetStreamContext) *batcher {
	raw := strings.TrimSpace(os.Getenv("BATCH_BUCKETS"))
	if raw == "" {
		return nil
	}
	buckets := map[string]bool{}
	for _, b := range strings.Split(raw, ",") {
		if b = strings.TrimSpace(b); b != "" {
			buckets[b] = true
		}
	}
	max := 128
	if v, err := strconv.Atoi(os.Getenv("BATCH_MAX")); err == nil && v > 1 && v <= 1000 {
		max = v
	}
	window := 1000 * time.Microsecond
	if v, err := strconv.Atoi(os.Getenv("BATCH_WINDOW_US")); err == nil && v > 0 {
		window = time.Duration(v) * time.Microsecond
	}
	return &batcher{
		nc: nc, js: js, max: max, window: window,
		buckets: buckets,
		queues:  map[string]chan batchPut{},
	}
}

func (b *batcher) enabledFor(bucket string) bool {
	return b != nil && b.buckets[bucket]
}

// put queues one write and blocks until its batch commits (or fails).
func (b *batcher) put(bucket, key string, data []byte) (uint64, error) {
	b.mu.Lock()
	q, ok := b.queues[bucket]
	if !ok {
		// Queue depth bounds memory: 4096 pending 10KB writes ≈ 40MB worst case.
		q = make(chan batchPut, 4096)
		b.queues[bucket] = q
		go b.flushLoop(q)
	}
	b.mu.Unlock()

	resp := make(chan batchResult, 1)
	q <- batchPut{subj: fmt.Sprintf("$KV.%s.%s", bucket, key), data: data, resp: resp}
	r := <-resp
	return r.rev, r.err
}

func (b *batcher) flushLoop(q chan batchPut) {
	for first := range q {
		batch := []batchPut{first}
		timer := time.NewTimer(b.window)
	fill:
		for len(batch) < b.max {
			select {
			case m := <-q:
				batch = append(batch, m)
			case <-timer.C:
				break fill
			}
		}
		timer.Stop()
		b.flush(batch)
	}
}

func (b *batcher) flush(batch []batchPut) {
	fail := func(err error) {
		for _, m := range batch {
			m.resp <- batchResult{err: err}
		}
	}

	// A batch of one gains nothing from the batch protocol — publish directly.
	if len(batch) == 1 {
		ack, err := b.js.PublishMsg(&nats.Msg{Subject: batch[0].subj, Data: batch[0].data})
		if err != nil {
			fail(err)
			return
		}
		batch[0].resp <- batchResult{rev: ack.Sequence}
		return
	}

	id := nuid.Next()
	n := len(batch)
	for i, m := range batch[:n-1] {
		msg := nats.NewMsg(m.subj)
		msg.Data = m.data
		msg.Header.Set(batchIdHdr, id)
		msg.Header.Set(batchSeqHdr, strconv.Itoa(i+1))
		// Staged message: no reply subject, the commit ack covers it.
		if err := b.nc.PublishMsg(msg); err != nil {
			fail(err)
			return
		}
	}
	commit := nats.NewMsg(batch[n-1].subj)
	commit.Data = batch[n-1].data
	commit.Header.Set(batchIdHdr, id)
	commit.Header.Set(batchSeqHdr, strconv.Itoa(n))
	commit.Header.Set(batchCommitHdr, "1")
	ack, err := b.js.PublishMsg(commit)
	if err != nil {
		fail(err)
		return
	}
	// Atomic batch = contiguous stream sequences ending at the commit's.
	for i, m := range batch {
		m.resp <- batchResult{rev: ack.Sequence - uint64(n-1) + uint64(i)}
	}
}
