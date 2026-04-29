// Package tenant provides the data model and persistence for tenants and API keys.
//
// Tenant + API-key state lives in a NATS KV bucket on the LZ adapter (`_kv-admin`,
// account "$SYS"-adjacent). Adapters everywhere watch this bucket for live
// revocation — sub-100ms global propagation. Invite tokens (one-shot, expiring)
// live in SQLite on the hub since they are admin-only and never propagate.
package tenant

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"strings"
	"time"
)

// Tenant is a self-service unit. Maps to a NATS Account on the substrate side.
type Tenant struct {
	ID          string    `json:"id"`           // t_<random>; stable
	Tag         string    `json:"tag"`          // human-readable label, set at claim time
	CreatedAt   time.Time `json:"created_at"`
	CreatedBy   string    `json:"created_by"`   // admin email who issued the invite
	Suspended   bool      `json:"suspended"`
	NatsAccount string    `json:"nats_account"` // NATS Account name = ID
	Quotas      Quotas    `json:"quotas"`
}

type Quotas struct {
	StorageBytes  int64 `json:"storage_bytes"`  // JetStream max bytes
	MaxBuckets    int   `json:"max_buckets"`
	MaxStreams    int   `json:"max_streams"`
	MaxConsumers  int   `json:"max_consumers"`
}

func DefaultQuotas() Quotas {
	return Quotas{
		StorageBytes: 5 * 1024 * 1024 * 1024, // 5 GiB
		MaxBuckets:   50,
		MaxStreams:   100,
		MaxConsumers: 100,
	}
}

// APIKey is a tenant credential. Plaintext shown only at creation; we store the hash.
type APIKey struct {
	ID          string    `json:"id"`           // ak_<random>; included in plaintext for display
	TenantID    string    `json:"tenant_id"`
	Hash        string    `json:"hash"`         // sha256(plaintext) hex
	Label       string    `json:"label"`        // optional, e.g. "default", "ci-runner"
	CreatedAt   time.Time `json:"created_at"`
	LastUsedAt  time.Time `json:"last_used_at"`
	RevokedAt   time.Time `json:"revoked_at,omitempty"`
}

// Active returns true if the key is not revoked.
func (k *APIKey) Active() bool { return k.RevokedAt.IsZero() }

// Invite is a one-time URL that lets a tester provision their own tenant.
type Invite struct {
	Token     string    `json:"token"`       // k_inv_<random>; appears in URL
	Tag       string    `json:"tag"`         // admin-supplied label, suggested as default tenant tag
	CreatedBy string    `json:"created_by"`  // admin email
	CreatedAt time.Time `json:"created_at"`
	ExpiresAt time.Time `json:"expires_at"`
	ClaimedAt time.Time `json:"claimed_at,omitempty"`
	TenantID  string    `json:"tenant_id,omitempty"`
}

// InviteRequest is a "please grant me access" submission from a visitor on the
// gated user app. Stored in NATS KV so the admin app can list pending requests
// and approve them — approval mints an Invite and binds it back to the request.
type InviteRequest struct {
	ID          string     `json:"id"`           // r_<base36-ts>-<rand>
	Name        string     `json:"name"`
	Email       string     `json:"email"`
	Reason      string     `json:"reason,omitempty"`
	UserAgent   string     `json:"user_agent,omitempty"`
	RemoteIP    string     `json:"remote_ip,omitempty"`
	CreatedAt   time.Time  `json:"created_at"`
	Status      string     `json:"status"`       // "pending" | "approved" | "declined"
	InviteToken string     `json:"invite_token,omitempty"` // set when approved
	DecidedAt   *time.Time `json:"decided_at,omitempty"`
	DecidedBy   string     `json:"decided_by,omitempty"`
	Note        string     `json:"note,omitempty"` // free-form admin note on decline/approve
}

func (i *Invite) Valid(now time.Time) error {
	if !i.ClaimedAt.IsZero() {
		return errors.New("invite already claimed")
	}
	if now.After(i.ExpiresAt) {
		return errors.New("invite expired")
	}
	return nil
}

// NewID returns a URL-safe random identifier with the given prefix.
// Format: <prefix><32 hex chars>. Sufficient for collision resistance and
// human-readable enough for API responses / log lines.
func NewID(prefix string) string {
	var buf [16]byte
	if _, err := rand.Read(buf[:]); err != nil {
		panic("rand failed: " + err.Error())
	}
	return prefix + hex.EncodeToString(buf[:])
}

// IssueAPIKey returns (key plaintext, hash). Plaintext shown once to caller.
func IssueAPIKey() (plaintext, hash string) {
	plaintext = NewID("akv_int_")
	sum := sha256.Sum256([]byte(plaintext))
	return plaintext, hex.EncodeToString(sum[:])
}

// HashKey returns the canonical sha256 hex of a key plaintext.
func HashKey(plaintext string) string {
	sum := sha256.Sum256([]byte(plaintext))
	return hex.EncodeToString(sum[:])
}

// ParseBearer extracts the token from an "Authorization: Bearer <token>" value.
func ParseBearer(authHeader string) (string, bool) {
	const prefix = "Bearer "
	if !strings.HasPrefix(authHeader, prefix) {
		return "", false
	}
	t := strings.TrimSpace(strings.TrimPrefix(authHeader, prefix))
	if t == "" {
		return "", false
	}
	return t, true
}

// AccountForTenant returns the NATS Account name for a tenant (1:1 mapping).
func AccountForTenant(t *Tenant) string {
	if t.NatsAccount != "" {
		return t.NatsAccount
	}
	return "T_" + strings.ToUpper(strings.TrimPrefix(t.ID, "t_"))
}

// MarshalSubject returns the subject prefix this tenant's KV ops are scoped to.
// Used as a defense-in-depth check at the adapter (in addition to NATS account auth).
func (t *Tenant) MarshalSubject() string {
	return fmt.Sprintf("$KV.%s.>", t.ID)
}
