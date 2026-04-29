package control

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"github.com/bapley/project-nats-kv/internal/tenant"
	"github.com/nats-io/nats.go"
)

// Store is the persistence layer for tenants, API keys, and invite tokens.
//
// Tenants + APIKeys → NATS KV bucket `_kv-admin` so adapters everywhere can
// watch for live revocation.
// Invites → SQLite on the hub, since they are admin-only one-shot tokens.
type Store struct {
	tenants nats.KeyValue // bucket: kv-admin-tenants
	keys    nats.KeyValue // bucket: kv-admin-keys
	db      *sql.DB
}

func NewStore(js nats.JetStreamContext, db *sql.DB) (*Store, error) {
	tb, err := openOrCreate(js, &nats.KeyValueConfig{
		Bucket:      "kv-admin-tenants-v2",
		Description: "tenant records, watched by adapters for live state",
		History:     8,
		Storage:     nats.FileStorage,
		Replicas:    1,  // R1 — JetStream proxies cross-cluster reads via leader; R3 fails on init due to cluster placement constraints with hostPath leaves
	})
	if err != nil {
		return nil, fmt.Errorf("tenants bucket: %w", err)
	}
	kb, err := openOrCreate(js, &nats.KeyValueConfig{
		Bucket:      "kv-admin-keys-v2",
		Description: "API key hash records, watched by adapters",
		History:     8,
		Storage:     nats.FileStorage,
		Replicas:    1,  // R1 — JetStream proxies cross-cluster reads via leader; R3 fails on init due to cluster placement constraints with hostPath leaves
	})
	if err != nil {
		return nil, fmt.Errorf("keys bucket: %w", err)
	}

	if err := bootstrapSchema(db); err != nil {
		return nil, fmt.Errorf("sqlite schema: %w", err)
	}

	return &Store{tenants: tb, keys: kb, db: db}, nil
}

func openOrCreate(js nats.JetStreamContext, cfg *nats.KeyValueConfig) (nats.KeyValue, error) {
	kv, err := js.KeyValue(cfg.Bucket)
	if err == nil {
		return kv, nil
	}
	if !errors.Is(err, nats.ErrBucketNotFound) {
		return nil, err
	}
	return js.CreateKeyValue(cfg)
}

func bootstrapSchema(db *sql.DB) error {
	_, err := db.Exec(`
CREATE TABLE IF NOT EXISTS invites (
	token       TEXT PRIMARY KEY,
	tag         TEXT NOT NULL DEFAULT '',
	created_by  TEXT NOT NULL,
	created_at  INTEGER NOT NULL,
	expires_at  INTEGER NOT NULL,
	claimed_at  INTEGER,
	tenant_id   TEXT
);
CREATE INDEX IF NOT EXISTS idx_invites_unclaimed ON invites(claimed_at) WHERE claimed_at IS NULL;

CREATE TABLE IF NOT EXISTS audit (
	id          INTEGER PRIMARY KEY AUTOINCREMENT,
	at          INTEGER NOT NULL,
	actor       TEXT NOT NULL,
	action      TEXT NOT NULL,
	target_type TEXT,
	target_id   TEXT,
	detail      TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_actor ON audit(actor);
CREATE INDEX IF NOT EXISTS idx_audit_at    ON audit(at);
`)
	return err
}

// --- Tenants ---

func (s *Store) PutTenant(t *tenant.Tenant) error {
	b, err := json.Marshal(t)
	if err != nil {
		return err
	}
	_, err = s.tenants.Put(t.ID, b)
	return err
}

func (s *Store) GetTenant(id string) (*tenant.Tenant, error) {
	e, err := s.tenants.Get(id)
	if err != nil {
		return nil, err
	}
	var t tenant.Tenant
	if err := json.Unmarshal(e.Value(), &t); err != nil {
		return nil, err
	}
	return &t, nil
}

func (s *Store) ListTenants() ([]*tenant.Tenant, error) {
	keys, err := s.tenants.Keys()
	if errors.Is(err, nats.ErrNoKeysFound) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	out := make([]*tenant.Tenant, 0, len(keys))
	for _, k := range keys {
		t, err := s.GetTenant(k)
		if err != nil {
			continue
		}
		out = append(out, t)
	}
	return out, nil
}

func (s *Store) DeleteTenant(id string) error {
	return s.tenants.Delete(id)
}

// --- API Keys ---

func (s *Store) PutKey(k *tenant.APIKey) error {
	b, err := json.Marshal(k)
	if err != nil {
		return err
	}
	// Bucket key is the hash so adapter lookups by hash are O(1).
	_, err = s.keys.Put(k.Hash, b)
	return err
}

func (s *Store) GetKeyByHash(hash string) (*tenant.APIKey, error) {
	e, err := s.keys.Get(hash)
	if err != nil {
		return nil, err
	}
	var k tenant.APIKey
	if err := json.Unmarshal(e.Value(), &k); err != nil {
		return nil, err
	}
	return &k, nil
}

func (s *Store) ListKeysByTenant(tenantID string) ([]*tenant.APIKey, error) {
	keys, err := s.keys.Keys()
	if errors.Is(err, nats.ErrNoKeysFound) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	out := make([]*tenant.APIKey, 0)
	for _, hash := range keys {
		k, err := s.GetKeyByHash(hash)
		if err != nil {
			continue
		}
		if k.TenantID == tenantID {
			out = append(out, k)
		}
	}
	return out, nil
}

func (s *Store) RevokeKey(hash string) error {
	k, err := s.GetKeyByHash(hash)
	if err != nil {
		return err
	}
	k.RevokedAt = time.Now().UTC()
	return s.PutKey(k)
}

// --- Invites (SQLite) ---

func (s *Store) CreateInvite(ctx context.Context, inv *tenant.Invite) error {
	_, err := s.db.ExecContext(ctx,
		`INSERT INTO invites(token, tag, created_by, created_at, expires_at) VALUES(?,?,?,?,?)`,
		inv.Token, inv.Tag, inv.CreatedBy, inv.CreatedAt.Unix(), inv.ExpiresAt.Unix())
	return err
}

func (s *Store) GetInvite(ctx context.Context, token string) (*tenant.Invite, error) {
	row := s.db.QueryRowContext(ctx,
		`SELECT token, tag, created_by, created_at, expires_at, claimed_at, tenant_id FROM invites WHERE token = ?`, token)
	var inv tenant.Invite
	var createdAt, expiresAt int64
	var claimedAt sql.NullInt64
	var tenantID sql.NullString
	if err := row.Scan(&inv.Token, &inv.Tag, &inv.CreatedBy, &createdAt, &expiresAt, &claimedAt, &tenantID); err != nil {
		return nil, err
	}
	inv.CreatedAt = time.Unix(createdAt, 0).UTC()
	inv.ExpiresAt = time.Unix(expiresAt, 0).UTC()
	if claimedAt.Valid {
		inv.ClaimedAt = time.Unix(claimedAt.Int64, 0).UTC()
	}
	if tenantID.Valid {
		inv.TenantID = tenantID.String
	}
	return &inv, nil
}

func (s *Store) ClaimInvite(ctx context.Context, token, tenantID string) error {
	res, err := s.db.ExecContext(ctx,
		`UPDATE invites SET claimed_at = ?, tenant_id = ? WHERE token = ? AND claimed_at IS NULL`,
		time.Now().Unix(), tenantID, token)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return errors.New("invite not found or already claimed")
	}
	return nil
}

func (s *Store) ListInvites(ctx context.Context, includeClaimed bool) ([]*tenant.Invite, error) {
	q := `SELECT token, tag, created_by, created_at, expires_at, claimed_at, tenant_id FROM invites`
	if !includeClaimed {
		q += ` WHERE claimed_at IS NULL`
	}
	q += ` ORDER BY created_at DESC`
	rows, err := s.db.QueryContext(ctx, q)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []*tenant.Invite{}
	for rows.Next() {
		var inv tenant.Invite
		var createdAt, expiresAt int64
		var claimedAt sql.NullInt64
		var tenantID sql.NullString
		if err := rows.Scan(&inv.Token, &inv.Tag, &inv.CreatedBy, &createdAt, &expiresAt, &claimedAt, &tenantID); err != nil {
			return nil, err
		}
		inv.CreatedAt = time.Unix(createdAt, 0).UTC()
		inv.ExpiresAt = time.Unix(expiresAt, 0).UTC()
		if claimedAt.Valid {
			inv.ClaimedAt = time.Unix(claimedAt.Int64, 0).UTC()
		}
		if tenantID.Valid {
			inv.TenantID = tenantID.String
		}
		out = append(out, &inv)
	}
	return out, rows.Err()
}

func (s *Store) RevokeInvite(ctx context.Context, token string) error {
	_, err := s.db.ExecContext(ctx, `DELETE FROM invites WHERE token = ? AND claimed_at IS NULL`, token)
	return err
}

// --- Audit ---

func (s *Store) Audit(ctx context.Context, actor, action, targetType, targetID, detail string) {
	_, _ = s.db.ExecContext(ctx,
		`INSERT INTO audit(at, actor, action, target_type, target_id, detail) VALUES(?,?,?,?,?,?)`,
		time.Now().Unix(), actor, action, targetType, targetID, detail)
}
