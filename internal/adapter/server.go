package adapter

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/nats-io/nats.go"
)

type Config struct {
	Region     string
	JS         nats.JetStreamContext
	NC         *nats.Conn
	DemoToken  string
	ControlURL string
}

type Server struct {
	cfg     Config
	mux     *http.ServeMux
	started time.Time
	keys    *KeyCache
}

func New(cfg Config) *Server {
	s := &Server{
		cfg:     cfg,
		mux:     http.NewServeMux(),
		started: time.Now(),
		keys:    NewKeyCache(cfg.JS, cfg.DemoToken, cfg.ControlURL),
	}
	s.routes()
	return s
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
		"status": "ok",
		"region": s.cfg.Region,
		"uptime": time.Since(s.started).String(),
	})
}

func (s *Server) handleCluster(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	info, _ := s.cfg.JS.AccountInfo()
	_ = json.NewEncoder(w).Encode(map[string]any{
		"region":      s.cfg.Region,
		"server":      s.cfg.NC.ConnectedServerName(),
		"cluster":     s.cfg.NC.ConnectedClusterName(),
		"account":     info,
		"keys_loaded": s.keys.Size(),
	})
}

func (s *Server) handleListBuckets(w http.ResponseWriter, r *http.Request) {
	if !s.authOK(r) {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	out := []map[string]any{}
	for name := range s.cfg.JS.KeyValueStoreNames() {
		out = append(out, s.bucketSummary(name))
	}
	_ = json.NewEncoder(w).Encode(map[string]any{"buckets": out, "served_by": s.cfg.Region})
}

// bucketSummary enriches a bucket with replica/leader/lag for topology UI.
// Also discovers any mirror streams (KV_<bucket>_mirror_*) and reports their
// placement + lag so the UI can render the full consistency-domain shape.
func (s *Server) bucketSummary(name string) map[string]any {
	entry := map[string]any{"name": name}
	kv, err := s.cfg.JS.KeyValue(name)
	if err != nil {
		entry["error"] = err.Error()
		return entry
	}
	st, _ := kv.Status()
	if st != nil {
		entry["values"] = st.Values()
		entry["history"] = st.History()
		entry["bytes"] = st.Bytes()
	}
	streamName := "KV_" + name

	// Find mirror streams pointing at this bucket.
	mirrors := []map[string]any{}
	for sn := range s.cfg.JS.StreamNames() {
		if !strings.HasPrefix(sn, streamName+"_mirror_") {
			continue
		}
		mi, err := s.cfg.JS.StreamInfo(sn)
		if err != nil || mi == nil {
			continue
		}
		m := map[string]any{
			"stream": sn,
			"messages": mi.State.Msgs,
		}
		if mi.Mirror != nil {
			m["lag_msgs"] = mi.Mirror.Lag
			m["active_ms"] = mi.Mirror.Active.Milliseconds()
		}
		if mi.Config.Placement != nil {
			m["placement_tags"] = mi.Config.Placement.Tags
		}
		if mi.Cluster != nil {
			m["leader"] = mi.Cluster.Leader
			m["cluster"] = mi.Cluster.Name
		}
		mirrors = append(mirrors, m)
	}
	if len(mirrors) > 0 {
		entry["mirrors"] = mirrors
	}

	// Stream backing the KV bucket has name "KV_<bucket>"
	if info, err := s.cfg.JS.StreamInfo(streamName); err == nil && info != nil {
		entry["replicas"] = info.Config.Replicas
		entry["mirror"] = info.Config.Mirror
		entry["sources"] = info.Config.Sources
		entry["placement_tags"] = nil
		if info.Config.Placement != nil {
			entry["placement_tags"] = info.Config.Placement.Tags
			entry["placement_cluster"] = info.Config.Placement.Cluster
		}
		if info.Cluster != nil {
			peers := []map[string]any{}
			peers = append(peers, map[string]any{
				"name":    info.Cluster.Leader,
				"role":    "leader",
				"current": true,
				"lag_ms":  0,
			})
			for _, p := range info.Cluster.Replicas {
				peers = append(peers, map[string]any{
					"name":    p.Name,
					"role":    "replica",
					"current": p.Current,
					"active":  p.Active.Milliseconds(),
					"lag_ms":  p.Lag,
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
	return s.cfg.JS.CreateKeyValue(&nats.KeyValueConfig{
		Bucket:      bucket,
		History:     8,
		Storage:     nats.FileStorage,
		Replicas:    1,
		Description: "auto-created bucket",
	})
}

func (s *Server) kvGet(w http.ResponseWriter, r *http.Request, bucket, key string) {
	start := time.Now()
	kv, err := s.ensureBucket(bucket)
	if err != nil {
		writeErr(w, err, http.StatusInternalServerError)
		return
	}
	if revStr := r.URL.Query().Get("revision"); revStr != "" {
		rev, _ := strconv.ParseUint(revStr, 10, 64)
		entry, err := kv.GetRevision(key, rev)
		if err != nil {
			writeErr(w, err, http.StatusNotFound)
			return
		}
		writeEntry(w, entry, start)
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
	writeEntry(w, entry, start)
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
