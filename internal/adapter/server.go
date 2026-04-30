package adapter

import (
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/nats-io/nats.go"
)

type Config struct {
	Region     string
	JS         nats.JetStreamContext
	NC         *nats.Conn
	DemoToken  string
	ControlURL string
	// AdminToken gates /v1/admin/* — same value the control plane uses.
	// Empty disables admin endpoints entirely (safer than open).
	AdminToken string
}

type Server struct {
	cfg     Config
	mux     *http.ServeMux
	started time.Time
	keys    *KeyCache

	mu             sync.RWMutex
	localMirrorFor map[string]string // bucket -> KV_<bucket>_mirror_<region> on this node
}

func New(cfg Config) *Server {
	s := &Server{
		cfg:            cfg,
		mux:            http.NewServeMux(),
		started:        time.Now(),
		keys:           NewKeyCache(cfg.JS, cfg.DemoToken, cfg.ControlURL),
		localMirrorFor: map[string]string{},
	}
	s.routes()
	go s.localMirrorRefreshLoop()
	return s
}

// refreshLocalMirrors scans current streams for KV_<bucket>_mirror_<region>
// matching this adapter's region and rebuilds the bucket->mirror map. Reads
// then prefer the local mirror via DirectGet on its specific stream name —
// without that, NATS distributes direct gets across all mirror replicas of the
// source and reads land in arbitrary regions.
func (s *Server) refreshLocalMirrors() {
	suffix := "_mirror_" + s.cfg.Region
	next := map[string]string{}
	for sn := range s.cfg.JS.StreamNames() {
		if !strings.HasPrefix(sn, "KV_") || !strings.HasSuffix(sn, suffix) {
			continue
		}
		bucket := strings.TrimSuffix(strings.TrimPrefix(sn, "KV_"), suffix)
		next[bucket] = sn
	}
	s.mu.Lock()
	s.localMirrorFor = next
	s.mu.Unlock()
}

func (s *Server) localMirrorRefreshLoop() {
	// Tick fast until the meta layer has surfaced streams, then back off.
	d := 2 * time.Second
	for {
		s.refreshLocalMirrors()
		s.mu.RLock()
		populated := len(s.localMirrorFor) > 0
		s.mu.RUnlock()
		if populated && d < 30*time.Second {
			d = 30 * time.Second
		}
		time.Sleep(d)
	}
}

func (s *Server) localMirrorName(bucket string) (string, bool) {
	s.mu.RLock()
	n, ok := s.localMirrorFor[bucket]
	s.mu.RUnlock()
	return n, ok
}

func (s *Server) Handler() http.Handler {
	return s.middleware(s.mux)
}

func (s *Server) middleware(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("X-Served-By", "kv-"+s.cfg.Region)
		w.Header().Set("X-Cluster-Geo", geoOf(s.cfg.Region))
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET,PUT,POST,DELETE,OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Authorization,Content-Type,If-Match,If-None-Match")
		w.Header().Set("Access-Control-Expose-Headers", "X-Served-By,X-Cluster-Geo,X-Replication-Lag-Ms,X-Revision,X-Bucket-Tenant,X-Latency-Ms")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		start := time.Now()
		h.ServeHTTP(w, r)
		_ = start
	})
}

func (s *Server) routes() {
	s.mux.HandleFunc("/v1/health", s.handleHealth)
	s.mux.HandleFunc("/v1/admin/cluster", s.handleCluster)
	s.mux.HandleFunc("/v1/admin/buckets", s.handleListBuckets)
	s.mux.HandleFunc("/v1/kv/", s.handleKV) // /v1/kv/:bucket/:key (and /history, /incr suffixes)
	s.mux.HandleFunc("/", s.handleIndex)
}

func (s *Server) handleIndex(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{
		"service": "nats-kv adapter",
		"region":  s.cfg.Region,
		"docs":    "https://github.com/bapley/project-nats-kv",
		"endpoints": []string{
			"GET /v1/health",
			"GET /v1/admin/cluster",
			"GET /v1/admin/buckets",
			"GET /v1/kv/:bucket/:key",
			"PUT /v1/kv/:bucket/:key",
			"DELETE /v1/kv/:bucket/:key",
			"GET /v1/kv/:bucket/:key/history",
			"POST /v1/kv/:bucket/:key/incr",
			"GET /v1/kv/:bucket/keys?match=pattern",
		},
	})
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{
		"status":      "ok",
		"region":      s.cfg.Region,
		"uptime":      time.Since(s.started).String(),
		"caller_ip":   r.RemoteAddr,
		"x_forwarded": r.Header.Get("X-Forwarded-For"),
		"x_real_ip":   r.Header.Get("X-Real-IP"),
		"user_agent":  r.Header.Get("User-Agent"),
	})
}

func (s *Server) handleCluster(w http.ResponseWriter, r *http.Request) {
	if !s.adminOK(r) {
		http.Error(w, "admin token required", http.StatusUnauthorized)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	info, _ := s.cfg.JS.AccountInfo()
	s.mu.RLock()
	mirrors := make(map[string]string, len(s.localMirrorFor))
	for k, v := range s.localMirrorFor {
		mirrors[k] = v
	}
	s.mu.RUnlock()
	_ = json.NewEncoder(w).Encode(map[string]any{
		"region":         s.cfg.Region,
		"server":         s.cfg.NC.ConnectedServerName(),
		"cluster":        s.cfg.NC.ConnectedClusterName(),
		"account":        info,
		"keys_loaded":    s.keys.Size(),
		"local_mirrors":  mirrors,
	})
}

func (s *Server) handleListBuckets(w http.ResponseWriter, r *http.Request) {
	tenant, ok := s.authTenant(r)
	if !ok {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	isAdmin := s.adminOK(r)
	w.Header().Set("Content-Type", "application/json")
	// Snapshot all stream names ONCE so each bucketSummary doesn't re-enumerate
	// the cluster (was the second-biggest contributor to slow topology refresh).
	allStreams := []string{}
	for sn := range s.cfg.JS.StreamNames() {
		allStreams = append(allStreams, sn)
	}
	// Filter the bucket list before fanning out summaries:
	//   - admin bearer:    every bucket on the cluster (current behavior)
	//   - any other bearer: tenant's own buckets (prefix `<tenant_id>__`) + the
	//                       shared `demo` bucket. System buckets (kv-admin-*)
	//                       are never returned to non-admin callers.
	prefix := tenant + "__"
	names := []string{}
	for n := range s.cfg.JS.KeyValueStoreNames() {
		if isAdmin {
			names = append(names, n)
			continue
		}
		if strings.HasPrefix(n, "kv-admin") {
			continue
		}
		if n == "demo" || strings.HasPrefix(n, prefix) {
			names = append(names, n)
		}
	}
	// Parallelize per-bucket lookups — each StreamInfo crosses to the source's
	// leader region, so 5 sequential = 5 × cross-region RTT (~3s). Fan out and
	// the wall-clock cost becomes max(per-bucket) ≈ one cross-region RTT.
	out := make([]map[string]any, len(names))
	var wg sync.WaitGroup
	for i, n := range names {
		wg.Add(1)
		go func(i int, n string) {
			defer wg.Done()
			out[i] = s.bucketSummary(n, allStreams)
		}(i, n)
	}
	wg.Wait()
	_ = json.NewEncoder(w).Encode(map[string]any{"buckets": out, "served_by": s.cfg.Region})
}

// bucketSummary enriches a bucket with replica/leader/lag for topology UI.
// Also discovers any mirror streams (KV_<bucket>_mirror_*) and reports their
// placement so the UI can render the full consistency-domain shape.
// allStreams is the pre-fetched list of stream names so we don't re-enumerate
// the cluster's stream catalog once per bucket.
func (s *Server) bucketSummary(name string, allStreams []string) map[string]any {
	entry := map[string]any{"name": name}
	streamName := "KV_" + name

	// Find mirror streams pointing at this bucket. We synthesize the per-mirror
	// fields directly from the stream name (`KV_<bucket>_mirror_<region>`) —
	// leader = `kv-<region>`, placement_tag = `region:<region>`. This avoids
	// one StreamInfo() round-trip per mirror, which was 27 cross-region calls
	// per bucket (5 buckets × 27 = ~140 sequential calls = ~24s topology
	// refresh). The UI doesn't show per-mirror lag/active anymore so the
	// missing State/Mirror/Cluster details aren't needed.
	mirrors := []map[string]any{}
	prefix := streamName + "_mirror_"
	for _, sn := range allStreams {
		if !strings.HasPrefix(sn, prefix) {
			continue
		}
		region := strings.TrimPrefix(sn, prefix)
		mirrors = append(mirrors, map[string]any{
			"stream":         sn,
			"leader":         "kv-" + region,
			"placement_tags": []string{"region:" + region},
		})
	}
	if len(mirrors) > 0 {
		entry["mirrors"] = mirrors
	}

	// Single StreamInfo fetches everything we need: config, cluster, state.
	// kv.Status() under the hood also calls StreamInfo, so calling both was
	// two cross-region round-trips per bucket; sticking to one cuts list
	// latency in half.
	if info, err := s.cfg.JS.StreamInfo(streamName); err == nil && info != nil {
		entry["replicas"] = info.Config.Replicas
		entry["mirror"] = info.Config.Mirror
		entry["sources"] = info.Config.Sources
		entry["history"] = info.Config.MaxMsgsPerSubject
		entry["values"] = info.State.Msgs
		entry["bytes"] = info.State.Bytes
		entry["placement_tags"] = nil
		if info.Config.Placement != nil {
			entry["placement_tags"] = info.Config.Placement.Tags
			entry["placement_cluster"] = info.Config.Placement.Cluster
		}
		if info.Cluster != nil {
			peers := []map[string]any{}
			peers = append(peers, map[string]any{
				"name":      info.Cluster.Leader,
				"role":      "leader",
				"current":   true,
				"lag_msgs":  0, // leader has no lag (it IS the source); rendered as "—" in UI
				"active_ms": 0, // ditto for last-seen
			})
			for _, p := range info.Cluster.Replicas {
				peers = append(peers, map[string]any{
					"name":      p.Name,
					"role":      "replica",
					"current":   p.Current,
					"active_ms": p.Active.Milliseconds(), // ms since this peer's last heartbeat from leader's view
					"lag_msgs":  p.Lag,                   // ops behind leader (NOT milliseconds — nats.go ClusterInfo.Replicas[].Lag is uint64 sequence delta)
				})
			}
			entry["peers"] = peers
			entry["cluster"] = info.Cluster.Name
		}
		entry["state"] = map[string]any{
			"messages": info.State.Msgs,
			"bytes":    info.State.Bytes,
			"first_ts": info.State.FirstTime,
			"last_ts":  info.State.LastTime,
		}
	}
	return entry
}

func (s *Server) handleKV(w http.ResponseWriter, r *http.Request) {
	if !s.authOK(r) {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	parts := strings.SplitN(strings.TrimPrefix(r.URL.Path, "/v1/kv/"), "/", 4)
	if len(parts) < 1 || parts[0] == "" {
		http.Error(w, "bucket required", http.StatusBadRequest)
		return
	}
	bucket := parts[0]

	// /v1/kv/:bucket/keys?match=pattern
	if len(parts) == 2 && parts[1] == "keys" {
		s.kvListKeys(w, r, bucket)
		return
	}

	if len(parts) < 2 || parts[1] == "" {
		http.Error(w, "key required", http.StatusBadRequest)
		return
	}
	key := parts[1]

	// /v1/kv/:bucket/:key/history
	if len(parts) == 3 && parts[2] == "history" {
		s.kvHistory(w, r, bucket, key)
		return
	}
	// /v1/kv/:bucket/:key/incr
	if len(parts) == 3 && parts[2] == "incr" && r.Method == http.MethodPost {
		s.kvIncr(w, r, bucket, key)
		return
	}

	switch r.Method {
	case http.MethodGet:
		s.kvGet(w, r, bucket, key)
	case http.MethodPut:
		s.kvPut(w, r, bucket, key)
	case http.MethodDelete:
		s.kvDelete(w, r, bucket, key)
	default:
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
	}
}

func (s *Server) authOK(r *http.Request) bool {
	got := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	if got == "" {
		return false
	}
	_, ok := s.keys.Validate(got)
	return ok
}

// adminOK gates /v1/admin/* endpoints. Constant-time compare to the
// configured admin token; if the token is empty, ALL admin endpoints
// are denied — fail closed when the deployment forgets to wire it.
func (s *Server) adminOK(r *http.Request) bool {
	if s.cfg.AdminToken == "" {
		return false
	}
	got := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	if got == "" {
		return false
	}
	return subtle.ConstantTimeCompare([]byte(got), []byte(s.cfg.AdminToken)) == 1
}

// authTenant returns (tenant_id, ok) for the request's bearer token.
// "demo" for the shared demo token, the real tenant ID otherwise.
func (s *Server) authTenant(r *http.Request) (string, bool) {
	got := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	if got == "" {
		return "", false
	}
	return s.keys.Validate(got)
}

func (s *Server) ensureBucket(bucket string) (nats.KeyValue, error) {
	kv, err := s.cfg.JS.KeyValue(bucket)
	if err == nil {
		return kv, nil
	}
	if !errors.Is(err, nats.ErrBucketNotFound) {
		return nil, err
	}
	// Ask the control plane to create the bucket using *this adapter's* region
	// as the placement anchor. The control plane runs the placement engine and
	// fans out per-region mirrors — so the demo bucket lands near whoever first
	// hit it, with read locality everywhere. R1 auto-creates were the prior
	// behavior; they pinned demo data to whichever peer JetStream picked,
	// which is how we ended up with a Japan-resident `demo` bucket after the
	// 2026-04-29 cluster wipe (see ADR-022 / ADR-023).
	if err := s.requestBucketEnsure(bucket); err != nil {
		return nil, fmt.Errorf("ensure bucket via control plane: %w", err)
	}
	return s.cfg.JS.KeyValue(bucket)
}

// requestBucketEnsure POSTs to the control plane's internal ensure endpoint,
// passing the local adapter region as the placement anchor.
func (s *Server) requestBucketEnsure(bucket string) error {
	if s.cfg.ControlURL == "" {
		return errors.New("control URL not configured")
	}
	body, _ := json.Marshal(map[string]any{
		"name":         bucket,
		"anchor":       s.cfg.Region,
		"replicas":     3,
		"history":      8,
		"with_mirrors": true,
	})
	req, _ := http.NewRequest(http.MethodPost, s.cfg.ControlURL+"/v1/internal/buckets/ensure", strings.NewReader(string(body)))
	req.Header.Set("Content-Type", "application/json")
	cli := &http.Client{Timeout: 30 * time.Second} // mirror fan-out is 24 stream creates; allow time
	resp, err := cli.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		b, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("control plane %d: %s", resp.StatusCode, string(b))
	}
	return nil
}

func (s *Server) kvGet(w http.ResponseWriter, r *http.Request, bucket, key string) {
	start := time.Now()

	// Versioned reads go through the source — mirrors don't expose the same
	// revision indexing path for arbitrary historical sequences.
	if revStr := r.URL.Query().Get("revision"); revStr != "" {
		kv, err := s.ensureBucket(bucket)
		if err != nil {
			writeErr(w, err, http.StatusInternalServerError)
			return
		}
		rev, _ := strconv.ParseUint(revStr, 10, 64)
		entry, err := kv.GetRevision(key, rev)
		if err != nil {
			writeErr(w, err, http.StatusNotFound)
			return
		}
		w.Header().Set("X-Read-Source", "source")
		writeEntry(w, entry, start)
		return
	}

	// Latest read: address the local mirror by stream name so direct gets stay
	// on this server. Without this, NATS load-balances direct gets across every
	// mirror of the source and most reads land cross-region.
	if mirrorName, ok := s.localMirrorName(bucket); ok {
		subject := "$KV." + bucket + "." + key
		msg, err := s.cfg.JS.GetLastMsg(mirrorName, subject, nats.DirectGet())
		if err == nil {
			if op := msg.Header.Get("KV-Operation"); op == "DEL" || op == "PURGE" {
				http.Error(w, "not found", http.StatusNotFound)
				return
			}
			w.Header().Set("X-Read-Source", "local-mirror")
			w.Header().Set("X-Read-Stream", mirrorName)
			writeRawMsg(w, msg, start)
			return
		}
		if errors.Is(err, nats.ErrMsgNotFound) {
			http.Error(w, "not found", http.StatusNotFound)
			return
		}
		// Other errors (transient API/timeout) — fall through to source.
	}

	kv, err := s.ensureBucket(bucket)
	if err != nil {
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	entry, err := kv.Get(key)
	if err != nil {
		if errors.Is(err, nats.ErrKeyNotFound) {
			http.Error(w, "not found", http.StatusNotFound)
			return
		}
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	w.Header().Set("X-Read-Source", "source")
	writeEntry(w, entry, start)
}

func writeRawMsg(w http.ResponseWriter, m *nats.RawStreamMsg, start time.Time) {
	w.Header().Set("X-Revision", strconv.FormatUint(m.Sequence, 10))
	w.Header().Set("X-Latency-Ms", strconv.FormatInt(time.Since(start).Milliseconds(), 10))
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Header().Set("X-Created-At", m.Time.Format(time.RFC3339Nano))
	_, _ = w.Write(m.Data)
}

func writeEntry(w http.ResponseWriter, entry nats.KeyValueEntry, start time.Time) {
	w.Header().Set("X-Revision", strconv.FormatUint(entry.Revision(), 10))
	w.Header().Set("X-Latency-Ms", strconv.FormatInt(time.Since(start).Milliseconds(), 10))
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Header().Set("X-Created-At", entry.Created().Format(time.RFC3339Nano))
	_, _ = w.Write(entry.Value())
}

func (s *Server) kvPut(w http.ResponseWriter, r *http.Request, bucket, key string) {
	start := time.Now()
	kv, err := s.ensureBucket(bucket)
	if err != nil {
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	body, err := io.ReadAll(io.LimitReader(r.Body, 1<<20)) // 1 MB cap
	if err != nil {
		writeErr(w, err, http.StatusBadRequest)
		return
	}
	var rev uint64
	ifMatch := r.Header.Get("If-Match")
	ifNoneMatch := r.Header.Get("If-None-Match")
	if ifNoneMatch == "*" {
		rev, err = kv.Create(key, body)
	} else if ifMatch != "" {
		var prev uint64
		prev, err = strconv.ParseUint(ifMatch, 10, 64)
		if err != nil {
			http.Error(w, "If-Match must be numeric revision", http.StatusBadRequest)
			return
		}
		rev, err = kv.Update(key, body, prev)
	} else {
		rev, err = kv.Put(key, body)
	}
	if err != nil {
		writeErr(w, err, http.StatusPreconditionFailed)
		return
	}
	w.Header().Set("X-Revision", strconv.FormatUint(rev, 10))
	w.Header().Set("X-Latency-Ms", strconv.FormatInt(time.Since(start).Milliseconds(), 10))
	w.WriteHeader(http.StatusOK)
	_, _ = fmt.Fprintf(w, `{"revision":%d}`, rev)
}

func (s *Server) kvDelete(w http.ResponseWriter, r *http.Request, bucket, key string) {
	kv, err := s.ensureBucket(bucket)
	if err != nil {
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	if err := kv.Delete(key); err != nil {
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) kvHistory(w http.ResponseWriter, r *http.Request, bucket, key string) {
	kv, err := s.ensureBucket(bucket)
	if err != nil {
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	entries, err := kv.History(key)
	if err != nil {
		writeErr(w, err, http.StatusNotFound)
		return
	}
	out := make([]map[string]any, 0, len(entries))
	for _, e := range entries {
		out = append(out, map[string]any{
			"revision":  e.Revision(),
			"value_b64": e.Value(),
			"created":   e.Created(),
			"operation": e.Operation().String(),
		})
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{"key": key, "history": out})
}

func (s *Server) kvIncr(w http.ResponseWriter, r *http.Request, bucket, key string) {
	delta := int64(1)
	if d := r.URL.Query().Get("delta"); d != "" {
		if i, err := strconv.ParseInt(d, 10, 64); err == nil {
			delta = i
		}
	}
	kv, err := s.ensureBucket(bucket)
	if err != nil {
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	for attempt := 0; attempt < 5; attempt++ {
		entry, getErr := kv.Get(key)
		var current int64
		var rev uint64
		if errors.Is(getErr, nats.ErrKeyNotFound) {
			current = 0
		} else if getErr != nil {
			writeErr(w, getErr, http.StatusInternalServerError)
			return
		} else {
			rev = entry.Revision()
			current, _ = strconv.ParseInt(string(entry.Value()), 10, 64)
		}
		next := current + delta
		nextBytes := []byte(strconv.FormatInt(next, 10))
		var newRev uint64
		var putErr error
		if rev == 0 {
			newRev, putErr = kv.Create(key, nextBytes)
		} else {
			newRev, putErr = kv.Update(key, nextBytes, rev)
		}
		if putErr == nil {
			w.Header().Set("X-Revision", strconv.FormatUint(newRev, 10))
			w.Header().Set("Content-Type", "application/json")
			_, _ = fmt.Fprintf(w, `{"value":%d,"revision":%d}`, next, newRev)
			return
		}
		// retry on CAS conflict
	}
	http.Error(w, "incr CAS contention", http.StatusConflict)
}

func (s *Server) kvListKeys(w http.ResponseWriter, r *http.Request, bucket string) {
	kv, err := s.ensureBucket(bucket)
	if err != nil {
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	keys, err := kv.Keys()
	if err != nil {
		// no keys is normal
		keys = []string{}
	}
	match := r.URL.Query().Get("match")
	if match != "" {
		filtered := keys[:0]
		for _, k := range keys {
			if subjectMatch(match, k) {
				filtered = append(filtered, k)
			}
		}
		keys = filtered
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{"bucket": bucket, "count": len(keys), "keys": keys})
}

// subjectMatch implements NATS subject wildcard matching: * matches one token, > matches the rest.
func subjectMatch(pattern, subject string) bool {
	pTokens := strings.Split(pattern, ".")
	sTokens := strings.Split(subject, ".")
	for i, p := range pTokens {
		if p == ">" {
			return true
		}
		if i >= len(sTokens) {
			return false
		}
		if p == "*" {
			continue
		}
		if p != sTokens[i] {
			return false
		}
	}
	return len(pTokens) == len(sTokens)
}

func writeErr(w http.ResponseWriter, err error, code int) {
	http.Error(w, err.Error(), code)
}

// geoOf maps a region short code to a coarse geo bucket.
func geoOf(region string) string {
	switch {
	case strings.HasPrefix(region, "us-"), strings.HasPrefix(region, "ca-"):
		return "na"
	case strings.HasPrefix(region, "eu-"), strings.HasPrefix(region, "de-"), strings.HasPrefix(region, "fr-"),
		strings.HasPrefix(region, "gb-"), strings.HasPrefix(region, "it-"), strings.HasPrefix(region, "nl-"),
		strings.HasPrefix(region, "se-"), strings.HasPrefix(region, "es-"):
		return "eu"
	case strings.HasPrefix(region, "ap-"), strings.HasPrefix(region, "jp-"), strings.HasPrefix(region, "id-"),
		strings.HasPrefix(region, "in-"), strings.HasPrefix(region, "sg-"):
		return "ap"
	case strings.HasPrefix(region, "br-"):
		return "sa"
	case strings.HasPrefix(region, "au-"):
		return "oc"
	default:
		return "unknown"
	}
}
