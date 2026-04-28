# DECISIONS.md — project-nats-kv

Architecture Decision Records. Newest entries appended.

---

## ADR-001 — HTTP adapter as the Spin integration path; defer native `key-value-nats` crate
**Status**: Accepted (2026-04-28)

**Context**: Spin functions need to call NATS KV, but Spin (open source) ships no NATS-backed KV provider — only `spin` (sqlite), `redis`, `azure_cosmos`, `aws_dynamo`. Akamai Functions further restricts apps to a single store labeled `default` with platform-owned `runtime-config.toml`. A native `key-value-nats` factor crate is technically possible but unusable on Akamai Functions today.

**Decision**: Build an HTTP adapter colocated with embedded NATS on each LKE node. Spin functions reach it via `wasi:http/outgoing-handler`; Spin's default `connection_pooling_enabled=true` amortizes TLS handshake at the worker level. The native crate is a future "if Akamai adopts NATS as a first-class backend" play, not a v1 deliverable.

**Consequences**: Function calls cost an extra metro hop (~5–15ms) vs. an in-process WIT call would. Acceptable because (a) the in-region NATS replica is still ~10x faster than Cosmos's typical 15–40ms reads, (b) HTTP unlocks NATS primitives the Spin KV WIT cannot express (history, atomic incr, subject wildcards, SSE watch), and (c) zero platform cooperation needed to ship.

---

## ADR-002 — NATS JetStream KV substrate, embedded NATS in adapter binary
**Status**: Accepted (2026-04-28)

**Decision**: Single Go binary per node containing the `nats-server` library with JetStream enabled and the HTTP adapter. In-process NATS connection — no localhost TCP hop. Storage on Block Storage Volume mounted at `/var/lib/nats/jetstream`.

**Consequences**: One systemd unit per node, no architectural change vs. project-latency's deployment pattern. Adapter restart = NATS restart for that node; mitigated by per-bucket replication and mirrors on all other nodes.

---

## ADR-003 — KV-flavored REST API with NATS-unique extensions
**Status**: Accepted (2026-04-28)

**Decision**: API surface intentionally exposes more than Spin's KV WIT can:
```
GET    /v1/kv/:bucket/:key              # read; query params: consistency, region, revision
PUT    /v1/kv/:bucket/:key              # write; If-Match / If-None-Match for CAS, ttl param
DELETE /v1/kv/:bucket/:key              # tombstone, optional If-Match
GET    /v1/kv/:bucket/:key/history      # versioned reads
GET    /v1/kv/:bucket/keys              # ?match=nats.subject.pattern
POST   /v1/kv/:bucket/batch/get         # bulk read, single round trip
POST   /v1/kv/:bucket/batch/set         # bulk write
POST   /v1/kv/:bucket/:key/incr         # atomic counter
GET    /v1/kv/:bucket/watch             # SSE — long-lived clients only (browser, server-to-server)
GET    /v1/admin/cluster                # topology + lag for the map UI
GET    /v1/admin/buckets                # bucket inventory
GET    /v1/health                       # GTM liveness
```

Every response includes observability headers: `X-Served-By`, `X-Replication-Lag-Ms`, `X-Revision`, `X-Cluster-Geo`.

**Consequences**: A future native crate would be a thin translation layer over this surface, not a redesign.

---

## ADR-004 — Watches via SSE for long-lived clients only; NATS triggers for Spin functions
**Status**: Accepted (2026-04-28)

**Context**: Akamai Functions has a wall-clock execution limit (specific value behind auth wall, but in seconds-to-minutes range). Even the most generous bound is wrong for long-lived watches: per-duration billing, no resumption across cold starts, mismatched against request-response model.

**Decision**: Server-side watches expose SSE at `GET /v1/kv/:bucket/watch`. Consumers are long-lived clients only — browsers (the demo map UI), backend-to-backend services. For Spin functions wanting change notifications, document the **NATS messaging trigger** pattern: KV write → JetStream publishes on `$KV.<bucket>.<key>` → Spin component invoked fresh by the trigger.

**Consequences**: We do not ship a watch endpoint intended for short-lived function consumption. The map UI is a first-class consumer of SSE — actually beneficial for the demo.

---

## ADR-005 — Lock / lease / leader-election as documented recipes, not endpoints
**Status**: Accepted (2026-04-28)

**Decision**: Ship CAS + TTL as raw primitives. Document patterns in `docs/recipes/`: distributed-lock, leader-election, idempotency-key, rate-limit-bucket, lease. No first-class `POST /locks/:name` endpoint.

**Rationale**: The pitch of this project is "NATS gives you raw primitives that Cosmos hides." Wrapping them in opinionated endpoints undercuts that. A user who learns CAS + TTL builds five things, not one.

**Consequences**: Slightly worse out-of-the-box ergonomics for the lock case; better long-term flexibility and a clearer mental model.

---

## ADR-006 — Latency-driven RAFT placement engine
**Status**: Accepted (2026-04-28)

**Decision**: At bucket creation, the placement engine pulls the real-time pairwise-RTT matrix from project-latency's hub API, enumerates candidate replica sets (`C(26, k)` for `k ∈ {1,3,5}`), scores by median quorum edge `e_ceil(k/2)` with tiebreakers on tighter and looser edges, and emits the chosen placement as JetStream `placement.tags`.

Three modes: `auto` (global optimum within constraints), `anchor` (one region pinned, system picks the rest — **default**, anchor = closest probe to request IP), `manual` (user supplies all replicas, validated for liveness).

Constraints: `geo_in`, `geo_not`, `min_pairwise_km` (anti-correlated failure), `max_quorum_latency_ms` (hard ceiling), `exclude_tier`.

Response includes selection rationale: chosen regions, edge latencies, score breakdown, top 3 alternatives considered.

**Consequences**: Project depends on project-latency being live — KV is non-functional without it. Documented as a hard dependency.

---

## ADR-007 — Drift detection surfaced; manual rebalance default; auto opt-in
**Status**: Accepted (2026-04-28)

**Decision**: Continuously recompute optimal placement against current placement per bucket every 60s. Surface `placement_drift_score` on `GET /v1/admin/buckets/:name`. Manual rebalance via `POST /v1/admin/buckets/:name/rebalance` (returns async job ID). Auto-rebalance is opt-in per bucket via `auto_rebalance: { drift_threshold_ms, cooldown }`, off by default.

**Rationale**: JetStream peer changes trigger snapshot transfer and can stall writes. Reactive rebalancing on minute-by-minute latency wobble would be operationally hostile.

---

## ADR-008 — R1 / R3 / R5 replication tiers, mirrors on every node by default
**Status**: Accepted (2026-04-28)

**Decision**:
| Tier | RAFT group | Mirrors | Default use |
|---|---|---|---|
| R1 | 1 node | 26 async | Cache, ephemeral state, max write throughput |
| R3 | 3 nodes (latency-optimal) | 24 async | **Default** for new tenants; durable + fast reads |
| R5 | 5 nodes (geo-dispersed) | 22 async | Continent-failure survivable |

`max_age` and `history` configurable per bucket. Strong-read flag `?consistency=strong` routes to RAFT leader; default is local mirror with `X-Replication-Lag-Ms` header surfaced.

Optional per-bucket `mirror_geo_in: ["eu"]` to constrain mirror set for residency reasons.

**Consequences**: Default storage footprint per node ≈ sum of all public bucket sizes. 50GB Block Storage per node sized accordingly; per-tenant quota 5GB initially.

---

## ADR-009 — Multi-tenancy via NATS Accounts, API key auth, NATS-bucket key store
**Status**: Accepted (2026-04-28)

**Decision**: One NATS Account per tenant — full namespace isolation, separate JetStream quotas, separate auth. Adapter authenticates `Authorization: Bearer akv_int_<random>` API keys → looks up tenant → operates as that tenant's NATS account internally.

Tenant + key records stored in a special `_kv-admin` NATS account/bucket, replicated to all 27 nodes. Adapters watch this bucket for live revocation (sub-100ms global propagation). Invite tokens and audit log live in SQLite on the hub.

**Default tenant quotas (POC)**: 5 GB JetStream storage, 50 buckets, 100 streams, 100 consumers. Bumpable via admin UI without ticket.

**Consequences**: Compromising the adapter on one node does not compromise other tenants — NATS account boundary holds.

---

## ADR-010 — Two separate Spin apps (user + admin) with bearer-token admin auth
**Status**: Accepted (2026-04-28)

**Decision**: Two physically separate Spin apps deployed to Akamai Functions, both reached via `nats-kv-demo.connected-cloud.io`:
- `nats-kv-user` (default route) — landing, playground, self-service dashboard, claim flow. Holds user-scoped service token. Wasm binary contains zero admin code.
- `nats-kv-admin` (`/admin/*`) — invite issuance, regen-key, suspend, delete. Holds admin-scoped service token. Bearer admin token gate at app entry.

Defense in depth (3 layers):
1. Admin app checks `Authorization: Bearer <admin_token>` (token stored as Spin variable, allowlist `brian@apley.net`, `bapley@akamai.com`).
2. Control plane rejects unless admin-scoped service token is presented.
3. Wasm boundary — admin verbs absent from user binary (XSS / supply-chain compromise on user app yields zero admin capability).

**Rejected**: EAA / SSO integration for `/admin*` path — too much integration work for POC. Token-only.

---

## ADR-011 — GTM performance routing with full load imbalance permitted
**Status**: Accepted (2026-04-28)

**Decision**: GTM domain `nats-kv.akadns.net` (type `full`), `load_feedback=true` (capability enabled, no load objects wired today), `load_imbalance_percentage=100` (route purely on perf — no balancing constraint). One performance property `kv` with `score_aggregation_type=mean`, `stickiness_bonus_percentage=50`, `handout_limit=8`, HTTPS liveness on `/v1/health` per DC. 27 datacenters, one per cluster region. Resolves `nats-kv-edge.connected-cloud.io`.

**Rationale**: Pure perf routing matches the project pitch (latency-optimized everything). Load feedback capability enabled for futureproofing — adapter exposes `/v1/admin/load` we can wire later without re-architecting GTM.

---

## ADR-012 — Reuse project-latency LKE clusters via second node pool
**Status**: Accepted (2026-04-28)

**Decision**: Add a second node pool (`g8-dedicated-4-2`, count=1) with `tier=kv` taint and 50GB Block Storage Volume to each of the 27 existing LKE clusters (26 leafs in presales account + presales-landing-zone hub). KV pods scheduled exclusively on the new pool via nodeSelector. `hostNetwork: true` on standard NATS ports — no port collision because separate physical node from the latency probe pod.

**Consequences**: KV demo and latency demo share infrastructure but are logically independent — separate URLs, separate UIs, separate Spin apps, can `terraform destroy` the KV node pools without affecting latency probes. KV depends on latency for placement data (ADR-006); latency does not depend on KV.

**Rejected**: Standalone fleet of 27 new VMs. Doubles cost for negligible isolation benefit; would require duplicating latency measurement.

---

## ADR-013 — Hostname and Akamai property scheme
**Status**: Accepted (2026-04-28)

**Decision**:
| Hostname | Purpose | Origin |
|---|---|---|
| `nats-kv-edge.connected-cloud.io` | Data plane, GTM-routed | GTM property `kv` (27 DCs) |
| `nats-kv.connected-cloud.io` | Control plane API | Hub Go service |
| `nats-kv-demo.connected-cloud.io` | User UI + admin UI (path-routed) | Two Spin apps on Akamai Functions |
| `{short}.nats-kv.connected-cloud.io` | Per-region direct | Edge DNS A record per region |

Property names match primary hostnames per global CLAUDE.md convention. Three Akamai properties total + one GTM domain.

**Rejected**: Single combined hostname for data + control plane. Separation lets us scale them independently and route through different cache policies.
