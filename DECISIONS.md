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

---

## ADR-014 — Adapter NB firewall stays open; rely on token auth at the adapter
**Status**: Accepted (2026-04-29)

**Context**: FWF Spin functions egress from Akamai infrastructure but their source CIDRs are not published and don't appear in the standard Akamai OIPACL. Attaching `demo-nb-fw` (3900446) to the adapter NodeBalancer caused FWF→adapter calls to fail with `ConnectionRefused`. Other LZ demos solve this by fronting the origin with an Akamai property — that lets the FW restrict to OIPACL because requests then arrive from Akamai edge, not FWF egress.

**Decision**: For this POC we leave the adapter NodeBalancer firewall **open** and rely on bearer-token authentication at the adapter (`Authorization: Bearer <token>`) for all `/v1/kv/*` and `/v1/admin/*` endpoints. Only `/v1/health` is unauthenticated (needed for GTM liveness probes).

**Consequences**: The adapter is internet-reachable on port 80; any unauthorized request gets `401`. Tokens are issued and rotated through the control plane (when v0.3 lands). For the demo, a single shared `akv_demo_open` token in Vault is acceptable.

**Future hardening (not v0.2)**: Akamaize the endpoint via an Akamai property in front of the NB. Then re-attach `demo-nb-fw` (or a new FW) restricting NB ingress to OIPACL only, and TLS terminates at the edge. This is the standard pattern for LZ demos — pending until we add the Akamai property in v0.3+.

**Rejected**: Discovering and pinning FWF egress CIDRs in firewall rules. Akamai does not publish them, and they likely change.

---

## ADR-015 — Reuse `connectedcloud5.akadns.net` GTM domain instead of creating a new one
**Status**: Accepted (2026-04-29) — supersedes the "own domain" intent in ADR-011

**Context**: Tried to create `nats-kv.akadns.net` per ADR-011. The presales account's only contract (`M-1YX7F61`, `AKAMAI_INTERNAL`) returns `performancePlusDomainsCanNotBeCreated` from the config-gtm v1 API for any new domain. The 106 existing GTM domains in this account were all provisioned out-of-band (Control Center / Akamai support). Provisioning a new domain there blocks on a manual ticket.

**Decision**: Add a `nats-kv` property + 27 dedicated `kv-<region>` datacenters into the existing `connectedcloud5.akadns.net` GTM domain (already used by mortgage-inference). Datacenter nicknames are kv-prefixed so we never collide with sibling demos. Property-scoped resources (traffic_targets, liveness_test) are isolated; domain-level resources (load_feedback toggle, default_timeout_penalty) are shared with siblings.

**Consequences**:
- DS2 stream for GTM events is **per-domain**, so we cannot independently enable/scope a DS2 feed just for nats-kv. If/when DS2 is enabled on `connectedcloud5.akadns.net`, the feed will include all sibling demos' events too. Filter at the consumer (ClickHouse) by property name.
- TF state for GTM lives in `nats-kv/gtm/terraform.tfstate` in the LZ ObjStore bucket, separate from sibling demos' state. Their state owns the domain resource; ours owns only the property + 27 DCs + 1 DNS CNAME.
- A future migration to a dedicated `nats-kv.akadns.net` domain remains possible (`terraform import` after Akamai support provisions it).

**Edge hostname**: `nats-kv-edge.connected-cloud.io` CNAMEs to `nats-kv.connectedcloud5.akadns.net.`

---

## ADR-017 — The 50ms KV-call gap to Cosmos is FWF-specific, not Spin/wasi-http
**Status**: Documented (2026-04-29)

**Context**: First-look benchmarks showed FWF Spin → our NATS adapter taking ~50ms per call vs. Cosmos's ~22ms via the same Spin app. Network ping FWF↔us-ord-NB is 0.3ms (intra-Chicago). Adapter internal time is sub-ms. Where's the 50ms going?

**Investigation**: Ran identical Spin app locally on a Linode VM in the same Chicago metro (claudebot, 0.3ms ping to us-ord NB) via `spin up`. Numbers:
- **Local Spin → wasi:http → us-ord NB (intra-DC)**: 1.7-2.4ms steady state (55ms only on first call for TLS handshake).
- **Local Spin → spin:key-value WIT → in-process sqlite**: 0.06-0.15ms GET, ~2.5ms PUT (sqlite fsync).
- **FWF Spin → wasi:http → us-ord NB**: 50-58ms steady state.
- **FWF Spin → spin:key-value WIT → managed Cosmos**: 22ms steady state.

**Decision/finding**: The ~50ms gap on FWF is a property of Akamai's managed Spin runtime, not Spin or wasi-http inherently. Self-hosted Spin in the same metro as the adapter delivers ~2ms — equivalent to in-process WIT for read paths.

**Implications**:
1. The "NATS-KV is slow vs. Cosmos" argument is FWF-platform-specific. Server-to-server clients (curl, sdks, native services) hit the adapter at ~5ms, not 50ms.
2. A native `key-value-nats` factor crate (deferred, see ADR-001) remains the right long-term fix on FWF — moves the call from wasi-http to in-process WIT.
3. We can demo true sub-5ms NATS-KV by deploying Spin ourselves (on LKE or a VM) alongside the adapter. The "self-hosted Spin" demo path is now a viable axis to compare against FWF.
4. Worth raising with Fermyon/Akamai: the 50ms FWF-outbound-HTTP overhead applies to every Spin app calling external services, not just NATS. Could be platform-wide latency win to investigate.

**Not changing yet**:
- Adapter / GTM / cluster topology — all fine; perf is constrained by FWF's outbound layer.
- Akamai property in front of `edge.nats-kv` (still deferred).

---

## ADR-016 — Explicit full-mesh `NATS_ROUTES` on every node
**Status**: Accepted (2026-04-29)

**Context**: The 27-node cluster was nominally a flat mesh (each node had `cluster.routes` pointing only to `us-ord.nats-kv.connected-cloud.io:6222`). NATS gossip was supposed to discover the other 25 peers and dial them. In practice, leaves only had 4 routes — all to LZ. R3 stream placement across regions failed because a quorum couldn't form between peers that didn't actually have routes to each other. JS meta-RAFT showed `cluster_size: 27` (peers were known about) but no peer-to-peer dial-out happened.

Suspected root cause: gossip-advertised peer addresses for the LZ pod were the pod's internal IP (10.2.0.x), which leaves either couldn't reach (different pod-network) or which conflicted with the latency-mesh NATS server's IP space in some clusters. Once leaves rejected the gossiped LZ address, the only known route stayed at the bootstrap one.

**Decision**: Set `NATS_ROUTES` on every node (LZ deployment + 26 leaf DaemonSets) to the full comma-separated list of all 27 peer URLs (`nats-route://<region>.nats-kv.connected-cloud.io:6222` × 27). NATS dedups self-routes; each node ends up with ~104 routes (4 per peer). Result: R3 stream placement with `geo:` tags works deterministically.

**Trade-off**: explicit-route lists need to be regenerated when nodes are added/removed. For 27 stable regions this is fine. If we scale dynamically later, switch to NATS gateway-based super-cluster topology (out of scope for now).

**Companion**: each adapter now also publishes `Tags: [region:<id>, geo:<g>]` so JetStream `placement.tags` selectors work (`geo:na`, `geo:eu`, etc.) — see `cmd/adapter/main.go` `geoOfRegion()`.

---

