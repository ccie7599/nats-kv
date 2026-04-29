package control

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/bapley/project-nats-kv/internal/tenant"
	"github.com/nats-io/nats.go"
)

type Server struct {
	store      *Store
	mux        *http.ServeMux
	adminToken string // bearer required on /v1/admin/*
	pubBaseURL string // for building claim URLs
	js         nats.JetStreamContext
}

func New(store *Store, adminToken, pubBaseURL string, js nats.JetStreamContext) *Server {
	s := &Server{store: store, mux: http.NewServeMux(), adminToken: adminToken, pubBaseURL: strings.TrimRight(pubBaseURL, "/"), js: js}
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

	// User self-service — bearer = user API key
	s.mux.HandleFunc("/v1/me", s.userOnly(s.meHandler))
	s.mux.HandleFunc("/v1/me/buckets", s.userOnly(s.userBucketsHandler))
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
	Name        string `json:"name"`     // user-friendly; gets prefixed with tenant id
	Replicas    int    `json:"replicas"` // 1/3/5
	Geo         string `json:"geo"`      // na/eu/ap/sa/oc/auto
	History     uint8  `json:"history"`  // KV history depth
	WantMirrors bool   `json:"want_mirrors"`
}

func (s *Server) userBucketsHandler(w http.ResponseWriter, r *http.Request, t *tenant.Tenant) {
	switch r.Method {
	case http.MethodGet:
		// List buckets owned by this tenant.
		out := []string{}
		prefix := t.ID + "__"
		for name := range s.js.KeyValueStoreNames() {
			if strings.HasPrefix(name, prefix) {
				out = append(out, name)
			}
		}
		writeJSON(w, 200, map[string]any{"buckets": out, "tenant_id": t.ID})
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
	cfg := &nats.KeyValueConfig{
		Bucket:      bucketName,
		History:     req.History,
		Storage:     nats.FileStorage,
		Replicas:    req.Replicas,
		Description: "tenant=" + t.ID + " name=" + req.Name,
	}
	if req.Geo != "auto" && req.Geo != "any" {
		cfg.Placement = &nats.Placement{Tags: []string{"geo:" + req.Geo}}
	}
	kv, err := s.js.CreateKeyValue(cfg)
	if err != nil {
		writeJSON(w, 500, map[string]any{"error": "create bucket: " + err.Error()})
		return
	}
	_ = kv

	// Auto-create mirrors in the other geos for read-locality, if requested.
	mirrors := []string{}
	if req.WantMirrors && req.Replicas >= 1 {
		mirrorGeos := []string{"na", "eu", "ap", "sa"}
		for _, g := range mirrorGeos {
			if g == req.Geo {
				continue
			}
			mirrorName := "KV_" + bucketName + "_mirror_" + g
			_, err := s.js.AddStream(&nats.StreamConfig{
				Name:    mirrorName,
				Mirror:  &nats.StreamSource{Name: "KV_" + bucketName},
				Storage: nats.FileStorage,
				Placement: &nats.Placement{Tags: []string{"geo:" + g}},
				Replicas: 1,
				AllowDirect: true,
				MirrorDirect: true,
				Duplicates: 2 * time.Minute,
			})
			if err == nil {
				mirrors = append(mirrors, mirrorName)
			}
		}
	}

	s.store.Audit(r.Context(), t.ID, "bucket.create", "bucket", bucketName, req.Geo)
	writeJSON(w, 200, map[string]any{
		"bucket":   bucketName,
		"replicas": req.Replicas,
		"geo":      req.Geo,
		"history":  req.History,
		"mirrors":  mirrors,
		"endpoint": "https://edge.nats-kv.connected-cloud.io",
		"sample_url": "https://edge.nats-kv.connected-cloud.io/v1/kv/" + bucketName + "/<key>",
	})
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
