package control

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/bapley/project-nats-kv/internal/placement"
	"github.com/bapley/project-nats-kv/internal/tenant"
	"github.com/nats-io/nats.go"
)

type Server struct {
	store      *Store
	mux        *http.ServeMux
	adminToken string // bearer required on /v1/admin/*
	pubBaseURL string // for building claim URLs
	nc         *nats.Conn        // raw NATS connection — used for stream-leader-stepdown API requests
	js         nats.JetStreamContext
	placer     *placement.Engine // nil disables auto-placement (falls back to NATS default)
}

func New(store *Store, adminToken, pubBaseURL string, nc *nats.Conn, js nats.JetStreamContext, placer *placement.Engine) *Server {
	s := &Server{store: store, mux: http.NewServeMux(), adminToken: adminToken, pubBaseURL: strings.TrimRight(pubBaseURL, "/"), nc: nc, js: js, placer: placer}
	s.routes()
	return s
}

func (s *Server) Handler() http.Handler { return s.cors(s.mux) }

func (s *Server) cors(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET,POST,DELETE,OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Authorization,Content-Type")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		h.ServeHTTP(w, r)
	})
}

func (s *Server) routes() {
	s.mux.HandleFunc("/v1/health", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 200, map[string]any{"status": "ok", "service": "nats-kv-control"})
	})

	// Admin endpoints — bearer admin token required.
	s.mux.HandleFunc("/v1/admin/invites", s.adminOnly(s.invitesHandler))
	s.mux.HandleFunc("/v1/admin/invites/", s.adminOnly(s.inviteByTokenHandler))
	s.mux.HandleFunc("/v1/admin/tenants", s.adminOnly(s.listTenantsHandler))
	s.mux.HandleFunc("/v1/admin/tenants/", s.adminOnly(s.tenantByIDHandler))

	// Claim flow — token in URL path, no admin auth.
	s.mux.HandleFunc("/v1/claim/", s.claimHandler)

	// Internal — adapters poll for active key list. Open to all (returns hashes,
	// not plaintexts; nodes share an L7 firewall envelope).
	s.mux.HandleFunc("/v1/internal/keys", s.internalKeysHandler)
	s.mux.HandleFunc("/v1/internal/buckets/ensure", s.internalEnsureBucketHandler)

	// User self-service — bearer = user API key
	s.mux.HandleFunc("/v1/me", s.userOnly(s.meHandler))
	s.mux.HandleFunc("/v1/me/buckets", s.userOnly(s.userBucketsHandler))

	// Placement preview — open (no auth) so the dashboard can show the
	// "what would auto pick?" panel before the user commits to creating a
	// bucket. Read-only; doesn't touch JetStream.
	s.mux.HandleFunc("/v1/placement/preview", s.placementPreviewHandler)
}

// placementPreviewHandler runs the placement engine without creating a bucket,
// so the UI can render the latency-driven decision as a hover/preview before
// the user clicks Create. Query params: replicas (default 3), anchor (default
// us-ord), mode (default "anchor"). Uses the live RTT matrix.
func (s *Server) placementPreviewHandler(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	if s.placer == nil {
		writeJSON(w, 503, map[string]any{"error": "placement engine not configured"})
		return
	}
	replicas := 3
	if v := r.URL.Query().Get("replicas"); v != "" {
		n, err := strconv.Atoi(v)
		if err == nil {
			replicas = n
		}
	}
	anchor := r.URL.Query().Get("anchor")
	if anchor == "" {
		anchor = "us-ord"
	}
	mode := r.URL.Query().Get("mode")
	if mode == "" {
		mode = "anchor"
	}
	d, err := s.placer.Pick(r.Context(), replicas, anchor, mode)
	if err != nil {
		writeJSON(w, 400, map[string]any{"error": err.Error()})
		return
	}
	writeJSON(w, 200, d)
}

// userOnly resolves the bearer token to a tenant; rejects if unknown.
func (s *Server) userOnly(h func(w http.ResponseWriter, r *http.Request, t *tenant.Tenant)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		tok, ok := tenant.ParseBearer(r.Header.Get("Authorization"))
		if !ok {
			writeJSON(w, 401, map[string]any{"error": "bearer key required"})
			return
		}
		key, err := s.store.GetKeyByHash(tenant.HashKey(tok))
		if err != nil || key == nil || !key.Active() {
			writeJSON(w, 401, map[string]any{"error": "unknown or revoked key"})
			return
		}
		t, err := s.store.GetTenant(key.TenantID)
		if err != nil {
			writeJSON(w, 401, map[string]any{"error": "tenant not found"})
			return
		}
		if t.Suspended {
			writeJSON(w, 403, map[string]any{"error": "tenant suspended"})
			return
		}
		h(w, r, t)
	}
}

func (s *Server) meHandler(w http.ResponseWriter, r *http.Request, t *tenant.Tenant) {
	writeJSON(w, 200, map[string]any{
		"tenant_id":   t.ID,
		"tag":         t.Tag,
		"created_at":  t.CreatedAt,
		"quotas":      t.Quotas,
		"endpoint":    "https://edge.nats-kv.connected-cloud.io",
	})
}

type createBucketReq struct {
	Name      string `json:"name"`     // user-friendly; gets prefixed with tenant id
	Replicas  int    `json:"replicas"` // 1/3/5
	Geo       string `json:"geo"`      // na/eu/ap/sa/oc | auto | anchor:<region> — RAFT placement
	Anchor    string `json:"anchor,omitempty"` // hint to placement engine when geo=auto (e.g. "fr-par-2")
	History   uint8  `json:"history"`  // KV history depth
	NoMirrors bool   `json:"no_mirrors,omitempty"` // opt-OUT — by default we mirror to every region
}

// allRegions: the 27 KV cluster peers, used to spread per-region read mirrors.
// Kept in code (not derived) so the control plane doesn't depend on cluster
// introspection at request time. Updated when new regions are added.
var allRegions = []string{
	"us-ord", "us-east", "us-central", "us-west", "us-southeast",
	"us-lax", "us-mia", "us-sea", "ca-central", "br-gru",
	"gb-lon", "eu-central", "de-fra-2", "fr-par-2", "nl-ams",
	"se-sto", "it-mil",
	"ap-south", "sg-sin-2", "ap-northeast", "jp-tyo-3", "jp-osa",
	"ap-west", "in-bom-2", "in-maa", "id-cgk", "ap-southeast",
}

func (s *Server) userBucketsHandler(w http.ResponseWriter, r *http.Request, t *tenant.Tenant) {
	switch r.Method {
	case http.MethodGet:
		// List buckets owned by this tenant, enriched with per-bucket details
		// (replicas, leader, placement tags, mirror count). The dashboard uses
		// this single round trip — earlier two-call design (names from here +
		// admin/buckets via adapter) failed when the adapter response (~50KB
		// across all buckets) tripped Spin's body framing through FWF.
		prefix := t.ID + "__"
		names := []string{}
		details := []map[string]any{}
		for name := range s.js.KeyValueStoreNames() {
			if !strings.HasPrefix(name, prefix) {
				continue
			}
			names = append(names, name)
			details = append(details, s.bucketDetails(name))
		}
		writeJSON(w, 200, map[string]any{"buckets": names, "details": details, "tenant_id": t.ID})
	case http.MethodPost:
		s.createUserBucket(w, r, t)
	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

func (s *Server) createUserBucket(w http.ResponseWriter, r *http.Request, t *tenant.Tenant) {
	var req createBucketReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, 400, map[string]any{"error": "bad json"})
		return
	}
	if req.Name == "" || strings.ContainsAny(req.Name, ".$> *") {
		writeJSON(w, 400, map[string]any{"error": "name required, no dots/spaces/wildcards"})
		return
	}
	if req.Replicas == 0 {
		req.Replicas = 3
	}
	if req.Replicas != 1 && req.Replicas != 3 && req.Replicas != 5 {
		writeJSON(w, 400, map[string]any{"error": "replicas must be 1/3/5"})
		return
	}
	if req.History == 0 {
		req.History = 8
	}
	if req.Geo == "" {
		req.Geo = "auto"
	}

	bucketName := t.ID + "__" + req.Name

	// Resolve placement. Decision is non-nil when the engine ran (auto/anchor
	// modes) and is returned in the response so the UI can render the "why."
	decision, placementErr := s.resolvePlacement(r.Context(), &req)
	if placementErr != nil {
		writeJSON(w, 400, map[string]any{"error": placementErr.Error()})
		return
	}

	mirrors, err := s.materializeBucket(bucketName, req.Replicas, req.History, decision, !req.NoMirrors,
		"tenant="+t.ID+" name="+req.Name)
	if err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}

	auditMsg := req.Geo
	if decision != nil {
		auditMsg = decision.Mode + ":" + decision.ChosenGeo
	}
	s.store.Audit(r.Context(), t.ID, "bucket.create", "bucket", bucketName, auditMsg)

	resp := map[string]any{
		"bucket":       bucketName,
		"replicas":     req.Replicas,
		"geo":          req.Geo,
		"history":      req.History,
		"mirrors":      mirrors,
		"mirror_count": len(mirrors),
		"endpoint":     "https://edge.nats-kv.connected-cloud.io",
		"sample_url":   "https://edge.nats-kv.connected-cloud.io/v1/kv/" + bucketName + "/<key>",
	}
	if decision != nil {
		resp["placement"] = decision
	}
	writeJSON(w, 200, resp)
}

// materializeBucket creates a KV source (with placement from decision if any)
// and, when withMirrors=true, fans out per-region mirror streams to every
// region the source's RAFT doesn't already cover. Idempotent on the source —
// if it already exists, mirrors are still ensured. Returns the list of mirror
// stream names created (or already present).
func (s *Server) materializeBucket(bucketName string, replicas int, history uint8, decision *placement.Decision, withMirrors bool, description string) ([]string, error) {
	cfg := &nats.KeyValueConfig{
		Bucket:      bucketName,
		History:     history,
		Storage:     nats.FileStorage,
		Replicas:    replicas,
		Description: description,
	}
	if decision != nil && decision.PlacementTag != "" {
		cfg.Placement = &nats.Placement{Tags: []string{decision.PlacementTag}}
	}

	if _, err := s.js.CreateKeyValue(cfg); err != nil {
		// Idempotency: if the bucket exists already, don't fail — drop through
		// to mirror reconciliation.
		if !errors.Is(err, nats.ErrStreamNameAlreadyInUse) {
			return nil, fmt.Errorf("create bucket: %w", err)
		}
	}

	// nats.go's CreateKeyValue only sets MirrorDirect=true when creating a
	// mirror bucket, not when creating a source. Without it, mirrors of this
	// source can't serve direct gets, which is the whole point of mirrors-
	// everywhere. Update the source stream config to enable it.
	srcStream := "KV_" + bucketName
	if si, infoErr := s.js.StreamInfo(srcStream); infoErr == nil && si != nil {
		if !si.Config.MirrorDirect {
			patched := si.Config
			patched.MirrorDirect = true
			patched.AllowDirect = true
			_, _ = s.js.UpdateStream(&patched)
		}
		// Force the leader to the engine's preferred region (chosen_regions[0])
		// so writes from that region's adapter don't pay a cross-cluster hop.
		// JetStream's placement.tags = [geo:<g>] picks any peer in the geo for
		// leadership, often a far-away one. We use the JS stream-leader-stepdown
		// API with a region tag to force election toward the top-ranked region.
		if decision != nil && len(decision.ChosenRegions) > 0 && si.Cluster != nil {
			preferred := decision.ChosenRegions[0]
			if !strings.HasSuffix(si.Cluster.Leader, preferred) {
				_, _ = s.nc.Request(
					"$JS.API.STREAM.LEADER.STEPDOWN."+srcStream,
					[]byte(`{"placement":{"tags":["region:`+preferred+`"]}}`),
					3*time.Second,
				)
				time.Sleep(500 * time.Millisecond) // let election settle before reading actual
				si, _ = s.js.StreamInfo(srcStream)
			}
		}
		// Record the actual placement so the UI can show what NATS chose vs
		// what the engine predicted (placement.tags=[geo:<g>] is coarse).
		if decision != nil && si != nil && si.Cluster != nil {
			actual := []string{}
			if si.Cluster.Leader != "" {
				actual = append(actual, strings.TrimPrefix(si.Cluster.Leader, "kv-"))
			}
			for _, p := range si.Cluster.Replicas {
				actual = append(actual, strings.TrimPrefix(p.Name, "kv-"))
			}
			decision.ActualRegions = actual
		}
	}

	mirrors := []string{}
	if !withMirrors {
		return mirrors, nil
	}
	// Create a local-read mirror in *every* region — including the regions that
	// host a RAFT replica of the source. The mirror is a separate stream, so
	// having both the source replica and the mirror on the same node is fine
	// (small extra disk per peer). This matters because the adapter's
	// local-mirror-by-name read path (ADR-020) only fires when there's a
	// `KV_<bucket>_mirror_<my-region>` stream to address. Skipping RAFT regions
	// caused those nodes to fall back to direct-get-on-source, which NATS
	// load-balances across the entire mirror fan-out — so reads from a node
	// that *should* have local data sometimes landed in Asia. With mirrors
	// everywhere, every region serves reads sub-ms.
	for _, region := range allRegions {
		mirrorName := srcStream + "_mirror_" + region
		_, err := s.js.AddStream(&nats.StreamConfig{
			Name:         mirrorName,
			Mirror:       &nats.StreamSource{Name: srcStream},
			Storage:      nats.FileStorage,
			Placement:    &nats.Placement{Tags: []string{"region:" + region}},
			Replicas:     1,
			AllowDirect:  true,
			MirrorDirect: true,
			Duplicates:   2 * time.Minute,
		})
		if err == nil || errors.Is(err, nats.ErrStreamNameAlreadyInUse) {
			mirrors = append(mirrors, mirrorName)
		}
	}
	return mirrors, nil
}

// internalEnsureBucketHandler is called by adapters when they see a request for
// a bucket that doesn't exist locally. The adapter passes its own region as
// `anchor` so placement runs from the caller's perspective — the bucket lands
// in the geo nearest to whoever first hit the playground.
//
// Restricted to a name allow-list to prevent random /v1/kv/<garbage>/key calls
// from spawning long-lived streams. No tenancy involvement (these are shared
// demo buckets).
func (s *Server) internalEnsureBucketHandler(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Name        string `json:"name"`
		Anchor      string `json:"anchor"`
		Replicas    int    `json:"replicas"`
		History     uint8  `json:"history"`
		WithMirrors bool   `json:"with_mirrors"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, 400, map[string]any{"error": "bad json"})
		return
	}
	if !isAutoCreatableBucket(req.Name) {
		writeJSON(w, 400, map[string]any{"error": "bucket name not on auto-create allow-list"})
		return
	}
	if req.Replicas == 0 {
		req.Replicas = 3
	}
	if req.History == 0 {
		req.History = 8
	}
	if req.Anchor == "" {
		req.Anchor = "us-ord"
	}

	// Already exists? Idempotent return.
	if _, err := s.js.StreamInfo("KV_" + req.Name); err == nil {
		writeJSON(w, 200, map[string]any{"bucket": req.Name, "status": "exists"})
		return
	}

	if s.placer == nil {
		writeJSON(w, 503, map[string]any{"error": "placement engine not configured"})
		return
	}
	d, err := s.placer.Pick(r.Context(), req.Replicas, req.Anchor, "auto")
	if err != nil {
		writeJSON(w, 500, map[string]any{"error": "placement: " + err.Error()})
		return
	}

	mirrors, err := s.materializeBucket(req.Name, req.Replicas, req.History, d, req.WithMirrors,
		"shared demo bucket; auto-placed from anchor "+req.Anchor)
	if err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}
	s.store.Audit(r.Context(), "internal", "bucket.ensure", "bucket", req.Name, "anchor="+req.Anchor+" geo="+d.ChosenGeo)
	writeJSON(w, 200, map[string]any{
		"bucket":       req.Name,
		"status":       "created",
		"placement":    d,
		"mirrors":      mirrors,
		"mirror_count": len(mirrors),
	})
}

// isAutoCreatableBucket gates which names the adapter can ask the control plane
// to auto-create. Tenant-prefixed buckets must go through /v1/me/buckets so we
// can charge them to a real tenant; only bare shared demo buckets are eligible
// for the on-first-access flow.
func isAutoCreatableBucket(name string) bool {
	switch name {
	case "demo":
		return true
	}
	return false
}

// resolvePlacement reads the createBucketReq's geo/anchor fields and produces
// the placement Decision the bucket should use. Returns (nil, nil) for legacy
// "manual" geo modes (na/eu/ap/sa/oc/any) so callers fall back to the simple
// "geo:<x>" tag — the engine's only purpose is to decide *which* geo when the
// caller doesn't know.
func (s *Server) resolvePlacement(ctx context.Context, req *createBucketReq) (*placement.Decision, error) {
	geo := strings.TrimSpace(req.Geo)
	mode := ""
	anchor := strings.TrimSpace(req.Anchor)
	anchorSrc := "request"

	switch {
	case geo == "auto":
		mode = "auto"
		if anchor == "" {
			anchor = "us-ord" // control plane lives here; reasonable default
			anchorSrc = "default-control-plane"
		}
	case strings.HasPrefix(geo, "anchor:"):
		mode = "anchor"
		anchor = strings.TrimPrefix(geo, "anchor:")
		if anchor == "" {
			return nil, errors.New("anchor:<region> requires a region")
		}
		anchorSrc = "request-geo-anchor"
	default:
		// Manual geo (na/eu/ap/...) or "any". Skip the engine.
		if geo != "" && geo != "any" {
			return &placement.Decision{
				Mode:         "manual",
				Replicas:     req.Replicas,
				ChosenGeo:    geo,
				PlacementTag: "geo:" + geo,
				GeneratedAt:  time.Now(),
				Notes:        []string{"manual geo selection — engine bypassed"},
			}, nil
		}
		return nil, nil
	}

	if s.placer == nil {
		return nil, errors.New("placement engine not configured (LATENCY_HUB_URL unset?)")
	}

	d, err := s.placer.Pick(ctx, req.Replicas, anchor, mode)
	if err != nil {
		return nil, err
	}
	d.AnchorSource = anchorSrc
	return d, nil
}

// bucketDetails returns per-bucket info for the dashboard list (replicas,
// leader, placement tags, mirror count). Lifted from the adapter's
// bucketSummary but kept lean — we don't need state size or per-mirror lag
// here; the topology page covers that.
func (s *Server) bucketDetails(name string) map[string]any {
	out := map[string]any{"name": name}
	streamName := "KV_" + name
	si, err := s.js.StreamInfo(streamName)
	if err != nil || si == nil {
		out["error"] = "stream-info: bucket missing or unreachable"
		return out
	}
	out["replicas"] = si.Config.Replicas
	if si.Config.Placement != nil {
		out["placement_tags"] = si.Config.Placement.Tags
	}
	if si.Cluster != nil {
		peers := []map[string]any{{"name": si.Cluster.Leader, "role": "leader"}}
		for _, p := range si.Cluster.Replicas {
			peers = append(peers, map[string]any{
				"name":    p.Name,
				"role":    "replica",
				"current": p.Current,
				"lag_ms":  p.Lag,
			})
		}
		out["peers"] = peers
		out["leader"] = si.Cluster.Leader
	}
	mirrorCount := 0
	for sn := range s.js.StreamNames() {
		if strings.HasPrefix(sn, streamName+"_mirror_") {
			mirrorCount++
		}
	}
	out["mirror_count"] = mirrorCount
	return out
}

// streamRegions returns the set of region IDs that already host a replica
// of the given stream. Used to avoid placing a mirror where the RAFT group
// already lives.
func (s *Server) streamRegions(streamName string) (map[string]bool, error) {
	out := map[string]bool{}
	info, err := s.js.StreamInfo(streamName)
	if err != nil || info == nil || info.Cluster == nil {
		return out, err
	}
	// Convert peer name "kv-<region>" → "<region>"
	if info.Cluster.Leader != "" {
		out[strings.TrimPrefix(info.Cluster.Leader, "kv-")] = true
	}
	for _, p := range info.Cluster.Replicas {
		out[strings.TrimPrefix(p.Name, "kv-")] = true
	}
	return out, nil
}

func (s *Server) internalKeysHandler(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	keys, err := s.store.AllKeys()
	if err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}
	writeJSON(w, 200, map[string]any{"keys": keys})
}

// --- Admin auth middleware ---

func (s *Server) adminOnly(h http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		tok, ok := tenant.ParseBearer(r.Header.Get("Authorization"))
		if !ok || tok != s.adminToken {
			writeJSON(w, 401, map[string]any{"error": "admin token required"})
			return
		}
		h(w, r)
	}
}

// --- Invites ---

type createInviteReq struct {
	Tag       string `json:"tag"`
	ExpiresIn string `json:"expires_in"` // e.g. "7d", "24h"
}

func (s *Server) invitesHandler(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodPost:
		s.createInvite(w, r)
	case http.MethodGet:
		s.listInvites(w, r)
	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

func (s *Server) createInvite(w http.ResponseWriter, r *http.Request) {
	var req createInviteReq
	_ = json.NewDecoder(r.Body).Decode(&req)
	if req.ExpiresIn == "" {
		req.ExpiresIn = "7d"
	}
	dur, err := parseDuration(req.ExpiresIn)
	if err != nil {
		writeJSON(w, 400, map[string]any{"error": "expires_in: " + err.Error()})
		return
	}
	now := time.Now().UTC()
	inv := &tenant.Invite{
		Token:     tenant.NewID("k_inv_"),
		Tag:       req.Tag,
		CreatedBy: "admin", // we know it's admin from the middleware
		CreatedAt: now,
		ExpiresAt: now.Add(dur),
	}
	if err := s.store.CreateInvite(r.Context(), inv); err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}
	s.store.Audit(r.Context(), "admin", "invite.create", "invite", inv.Token, inv.Tag)
	writeJSON(w, 200, map[string]any{
		"token":      inv.Token,
		"url":        s.pubBaseURL + "/claim/" + inv.Token,
		"expires_at": inv.ExpiresAt,
		"tag":        inv.Tag,
	})
}

func (s *Server) listInvites(w http.ResponseWriter, r *http.Request) {
	includeClaimed := r.URL.Query().Get("include_claimed") == "true"
	invs, err := s.store.ListInvites(r.Context(), includeClaimed)
	if err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}
	writeJSON(w, 200, map[string]any{"invites": invs})
}

func (s *Server) inviteByTokenHandler(w http.ResponseWriter, r *http.Request) {
	token := strings.TrimPrefix(r.URL.Path, "/v1/admin/invites/")
	if token == "" {
		w.WriteHeader(http.StatusBadRequest)
		return
	}
	switch r.Method {
	case http.MethodDelete:
		if err := s.store.RevokeInvite(r.Context(), token); err != nil {
			writeJSON(w, 500, map[string]any{"error": err.Error()})
			return
		}
		s.store.Audit(r.Context(), "admin", "invite.revoke", "invite", token, "")
		w.WriteHeader(http.StatusNoContent)
	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

// --- Tenants ---

func (s *Server) listTenantsHandler(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	ts, err := s.store.ListTenants()
	if err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}
	writeJSON(w, 200, map[string]any{"tenants": ts})
}

func (s *Server) tenantByIDHandler(w http.ResponseWriter, r *http.Request) {
	rest := strings.TrimPrefix(r.URL.Path, "/v1/admin/tenants/")
	parts := strings.SplitN(rest, "/", 2)
	if len(parts) == 0 || parts[0] == "" {
		w.WriteHeader(http.StatusBadRequest)
		return
	}
	id := parts[0]
	suffix := ""
	if len(parts) == 2 {
		suffix = parts[1]
	}

	switch {
	case suffix == "regen-key" && r.Method == http.MethodPost:
		s.regenKey(w, r, id)
	case suffix == "suspend" && r.Method == http.MethodPost:
		s.setSuspend(w, r, id, true)
	case suffix == "unsuspend" && r.Method == http.MethodPost:
		s.setSuspend(w, r, id, false)
	case suffix == "" && r.Method == http.MethodGet:
		t, err := s.store.GetTenant(id)
		if errors.Is(err, nats.ErrKeyNotFound) {
			writeJSON(w, 404, map[string]any{"error": "not found"})
			return
		}
		if err != nil {
			writeJSON(w, 500, map[string]any{"error": err.Error()})
			return
		}
		writeJSON(w, 200, t)
	case suffix == "" && r.Method == http.MethodDelete:
		s.deleteTenant(w, r, id)
	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

func (s *Server) regenKey(w http.ResponseWriter, r *http.Request, tenantID string) {
	t, err := s.store.GetTenant(tenantID)
	if err != nil {
		writeJSON(w, 404, map[string]any{"error": err.Error()})
		return
	}
	// Revoke all existing keys for this tenant.
	existing, _ := s.store.ListKeysByTenant(tenantID)
	for _, k := range existing {
		_ = s.store.RevokeKey(k.Hash)
	}
	// Issue new.
	plaintext, hash := tenant.IssueAPIKey()
	now := time.Now().UTC()
	nk := &tenant.APIKey{
		ID:        tenant.NewID("ak_"),
		TenantID:  tenantID,
		Hash:      hash,
		Label:     "default (regen)",
		CreatedAt: now,
	}
	if err := s.store.PutKey(nk); err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}
	s.store.Audit(r.Context(), "admin", "key.regen", "tenant", t.ID, "")
	writeJSON(w, 200, map[string]any{
		"tenant_id": tenantID,
		"key":       plaintext,
		"key_id":    nk.ID,
		"warning":   "shown once; copy now",
	})
}

func (s *Server) setSuspend(w http.ResponseWriter, r *http.Request, tenantID string, suspend bool) {
	t, err := s.store.GetTenant(tenantID)
	if err != nil {
		writeJSON(w, 404, map[string]any{"error": err.Error()})
		return
	}
	t.Suspended = suspend
	if err := s.store.PutTenant(t); err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}
	action := "tenant.suspend"
	if !suspend {
		action = "tenant.unsuspend"
	}
	s.store.Audit(r.Context(), "admin", action, "tenant", t.ID, "")
	writeJSON(w, 200, t)
}

func (s *Server) deleteTenant(w http.ResponseWriter, r *http.Request, tenantID string) {
	t, err := s.store.GetTenant(tenantID)
	if err != nil {
		writeJSON(w, 404, map[string]any{"error": err.Error()})
		return
	}
	// Revoke all keys first.
	existing, _ := s.store.ListKeysByTenant(tenantID)
	for _, k := range existing {
		_ = s.store.RevokeKey(k.Hash)
	}
	if err := s.store.DeleteTenant(tenantID); err != nil {
		writeJSON(w, 500, map[string]any{"error": err.Error()})
		return
	}
	s.store.Audit(r.Context(), "admin", "tenant.delete", "tenant", t.ID, t.Tag)
	w.WriteHeader(http.StatusNoContent)
}

// --- Claim flow (no admin auth; one-shot token in path) ---

type claimReq struct {
	Tag string `json:"tag"`
}

func (s *Server) claimHandler(w http.ResponseWriter, r *http.Request) {
	token := strings.TrimPrefix(r.URL.Path, "/v1/claim/")
	if token == "" {
		w.WriteHeader(http.StatusBadRequest)
		return
	}
	inv, err := s.store.GetInvite(r.Context(), token)
	if err != nil {
		writeJSON(w, 404, map[string]any{"error": "invite not found"})
		return
	}
	if err := inv.Valid(time.Now().UTC()); err != nil {
		writeJSON(w, 410, map[string]any{"error": err.Error()})
		return
	}
	switch r.Method {
	case http.MethodGet:
		writeJSON(w, 200, map[string]any{
			"valid":         true,
			"tag_suggested": inv.Tag,
			"expires_at":    inv.ExpiresAt,
		})
	case http.MethodPost:
		s.claim(w, r, inv)
	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

func (s *Server) claim(w http.ResponseWriter, r *http.Request, inv *tenant.Invite) {
	var req claimReq
	_ = json.NewDecoder(r.Body).Decode(&req)
	tag := req.Tag
	if tag == "" {
		tag = inv.Tag
	}

	// Provision tenant.
	now := time.Now().UTC()
	tenantID := tenant.NewID("t_")
	t := &tenant.Tenant{
		ID:          tenantID,
		Tag:         tag,
		CreatedAt:   now,
		CreatedBy:   inv.CreatedBy,
		NatsAccount: "T_" + strings.ToUpper(strings.TrimPrefix(tenantID, "t_")),
		Quotas:      tenant.DefaultQuotas(),
	}
	if err := s.store.PutTenant(t); err != nil {
		writeJSON(w, 500, map[string]any{"error": "tenant write: " + err.Error()})
		return
	}

	// Issue API key.
	plaintext, hash := tenant.IssueAPIKey()
	k := &tenant.APIKey{
		ID:        tenant.NewID("ak_"),
		TenantID:  tenantID,
		Hash:      hash,
		Label:     "default",
		CreatedAt: now,
	}
	if err := s.store.PutKey(k); err != nil {
		writeJSON(w, 500, map[string]any{"error": "key write: " + err.Error()})
		return
	}

	if err := s.store.ClaimInvite(r.Context(), inv.Token, tenantID); err != nil {
		// Tenant + key created but claim record failed; surface but don't block.
		s.store.Audit(r.Context(), "system", "claim.race", "invite", inv.Token, err.Error())
	}
	s.store.Audit(r.Context(), inv.CreatedBy+"->claim", "tenant.create", "tenant", tenantID, tag)

	writeJSON(w, 200, map[string]any{
		"tenant_id": tenantID,
		"tag":       tag,
		"key":       plaintext,
		"key_id":    k.ID,
		"warning":   "shown once; copy now",
		"endpoint":  "https://nats-kv-edge.connected-cloud.io",
	})
}

// --- helpers ---

func writeJSON(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(body)
}

// parseDuration accepts "Nh", "Nd", or stdlib duration formats.
func parseDuration(s string) (time.Duration, error) {
	if strings.HasSuffix(s, "d") {
		days := strings.TrimSuffix(s, "d")
		n, err := strconv.Atoi(days)
		if err != nil {
			return 0, err
		}
		return time.Duration(n) * 24 * time.Hour, nil
	}
	return time.ParseDuration(s)
}

var _ = context.TODO // ensure context import retained
