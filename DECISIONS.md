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

## ADR-022 — Admin buckets are R5 in NA (post-mortem: R1 admin storage caused total data loss)
**Status**: Accepted (2026-04-29)

**Incident**: At 2026-04-29 14:19:27, the JetStream meta-RAFT propagated a 0-byte snapshot ("no streams exist") cluster-wide. All 27 peers truncated their local stream catalogs in response. PVs stayed attached but on-disk stream blocks were wiped — `du -sk /var/lib/nats/jetstream` went from "had data" to 80K (just `$SYS/_js_/` skeleton) on every peer. Lost: every user bucket (`demo-r3-na`, `demo-r3-eu`, `demo-r3-ap`, `demo-r5-global`, `demo-r1-us-ord`, all auto-created mirrors), the tenants/keys catalogs (`kv-admin-tenants-v2`, `kv-admin-keys-v2`), and the entire demo dataset.

**Root cause**: the v2 admin buckets were created `Replicas: 1` with a comment saying "the cluster mesh proxies reads/watches from any other peer." That comment was wrong about what R1 means in JetStream — R1 is a single canonical replica, not a logically-shared singleton with cluster-wide proxy. JetStream pinned both buckets to the LZ adapter pod (the control plane's local NATS connection lands there). Earlier in the day Argo rolled the LZ adapter pod from v0.1.9 → v0.2.1; the new pod's PV came up with the JetStream skeleton (`$SYS/_js_/`) but no `_meta_/msgs/` log. With the LZ peer reporting "I don't have these streams" while leaves still had mirror data, the meta-RAFT eventually elected a leader that only had the empty view, snapshotted that view, and propagated it. Because the source streams were R1 with no replicas elsewhere, there was nothing to vote against the empty snapshot.

**Why all 27 peers wiped, not just the LZ**: meta-RAFT snapshots are authoritative. Once "0 streams" became the consensus view, mirror streams (which depend on a source stream definition) were also reaped. Leaves had data on disk but NATS deleted those directories on snapshot apply.

**Fix**:
- `kv-admin-tenants-v3` and `kv-admin-keys-v3`: R5 with `placement.tags = [geo:na]`. 5 NA replicas (LZ + 4 leaves), quorum=3, survives 2 simultaneous peer failures. NA-only because the control plane's NATS connection lands in us-ord; cross-region quorum writes would slow tenant claim flows.
- New version suffix (v3) instead of in-place R1→R5 update because in-place stream config changes for replica count require a careful migration path (peer-add followed by peer-remove) that isn't worth implementing for a POC. The wipe means there's no data to migrate anyway.
- The `R1 keeps the bucket on one peer; cluster mesh proxies reads/watches` comment is removed because it described a behavior NATS doesn't have.

**Bigger lesson**: R1 is appropriate only for buckets where the data is regenerable from elsewhere, OR where the cost of loss is acceptable. Admin / control-plane data is neither. Going forward, anything that gates user functionality (tenants, keys, audit, billing-relevant counters) is R3 minimum, R5 if it lives in one geo. The placement engine already rejects R1 as a non-default option for user buckets — same posture for internal storage.

**Not addressed by this ADR (follow-ups)**:
- The LZ adapter's PV came up with an inconsistent JetStream skeleton after rollout. Need to investigate whether the adapter's startup logic should refuse to start with a half-populated `_js_` dir, OR whether the deployment should clear the PV on rollout (since R5 means we don't need single-peer durability anymore).
- Cluster-wide backup/restore: currently nothing snapshots PVs. For a tier-2 demo POC, accepting "rebuild on disaster" is fine, but should be called out.

**Versions**: control `v0.1.12`. Rolled via Argo 2026-04-29.

---

## ADR-021 — Placement engine recommends a geo, not arbitrary regions (NATS placement tags are AND-only)
**Status**: Accepted (2026-04-29)

**Context**: SCOPE called for a "latency-driven placement engine" with three modes (auto/anchor/manual) that picks an optimal RAFT triple/quintuple from the 27 regions. Implementation hit a wall: NATS `placement.tags` is an AND filter — passing `[region:us-ord, region:eu-central, region:jp-osa]` matches zero servers because no single server has all three tags. There is no "place one replica in each of these specific regions" primitive in JetStream's stream config.

**Two possible implementations:**

1. **Geo-grouped placement.** Engine outputs the best *geo* (`na`/`eu`/`ap`/`sa`) for an anchor and emits `tags: [geo:<g>]`. NATS picks N servers carrying that tag. Coarser-grained but uses tags every server already advertises — zero ops overhead.
2. **Synthetic per-bucket tags.** Engine picks specific regions, then injects a unique tag onto each chosen server via a NATS server-config reload. NATS then matches those servers and only those. Finer-grained, but requires per-bucket reload coordination across 27 nodes.

**Decision**: Ship (1) for v1. The UI gets the *informationally rich* output you wanted — anchor region, RTT from anchor to each candidate geo, median quorum edge per candidate, runner-up score deltas — but the placement tag we hand JetStream is just `geo:<g>`. NATS still picks the actual servers within the geo; in practice it picks the closest few because the cluster meta layer optimizes for connectivity. Verified against R3 buckets: engine recommended `[fr-par-2, de-fra-2, gb-lon]` for an `fr-par-2` anchor, NATS placed `[eu-central, de-fra-2, fr-par-2]` (all EU geo, slightly different specific regions). Both sets satisfy the customer-visible promise of "this bucket lives in EU."

**Algorithm** (`internal/placement`):
- Pull the live RTT matrix from `latency-demo.connected-cloud.io/api/v1/matrix` (60s in-process cache; serves stale on hub failure).
- For each geo with ≥`replicas` regions, sort by RTT-from-anchor, take top-`replicas`, pick the leader within that set that minimizes `anchor→leader + leader's quorum edge`. Quorum edge = (ceil(k/2)-1)-th smallest leader→peer RTT.
- Rank geos by minimum write-latency-from-anchor. Decision includes every evaluated geo with eligibility flag and reason — feeds the UI's "why this geo" panel.

**Anchor sourcing** (v1):
- `geo: "auto"` + optional `anchor: "<region>"` field on the create request → engine runs with that anchor.
- `geo: "anchor:<region>"` shorthand also accepted.
- No anchor supplied → defaults to `us-ord` (where the control plane lives) and labels the source as `default-control-plane` so the UI knows to nudge "tell us your nearest region for better placement."
- GeoIP lookup from caller IP is deferred — Spin user-app already exposes `/api/whereami` (FWF egress geo), which it can pass as the anchor hint when calling create.

**Network gotcha**: `latency.connected-cloud.io` (origin) is firewalled to specific source IPs and is unreachable from the LZ pod's NAT egress. Switched to the Akamai-fronted `latency-demo.connected-cloud.io` which works through the CDN. Adds 5-10ms to the matrix fetch but doesn't affect placement decisions because we cache for 60s.

**New endpoint**: `GET /v1/placement/preview?anchor=&replicas=&mode=` returns the Decision without creating a bucket — for the dashboard's "preview" panel before a user clicks Create.

**Future (not v1)**:
- Synthetic-tag implementation (option 2) once the UI demands per-region precision.
- Pull p50/p95 from project-latency once it exposes them — currently only the latest reading is in the matrix.
- Auto-rebalance on drift (already in SCOPE non-goals as opt-in only).

**Versions**: control `v0.1.10`. Rolled to LZ via Argo 2026-04-29.

---

## ADR-020 — Adapter reads address the local mirror by stream name (NATS direct-get load-balances; we need explicit local pinning)
**Status**: Accepted (2026-04-29)

**Context**: After per-region mirrors landed (one `KV_<bucket>_mirror_<region>` per non-RAFT region, 24+ mirrors per source bucket), reads from leaves still posted X-Latency-Ms of 130-380ms — same as before mirrors existed. The mirrors had the data (`stream subjects` showed `$KV.<bucket>.<key>` with count=1) and the local NATS server had a subscription on the source's direct-get subject (`$JS.API.DIRECT.GET.KV_<bucket>`), yet `msgs` on that sub stayed at 0 across multiple reads.

**Two bugs found, both required for local reads:**

1. **`MirrorDirect=false` on source streams.** `nats.go`'s `js.CreateKeyValue()` only sets `MirrorDirect=true` when creating a *mirror* bucket, not a *source* bucket (see `nats.go@v1.49.0/kv.go:469`). Without `MirrorDirect=true` on the source, mirrors never advertise themselves as direct-get servers for that source. Fix: control plane now does `UpdateStream` after `CreateKeyValue` to flip `MirrorDirect=true`. Existing demo buckets fixed manually with `nats stream edit --allow-mirror-direct`.

2. **Direct gets to a source name don't prefer local mirrors.** Even after fixing (1) — and confirming local subs existed — direct-get requests sent to `$JS.API.DIRECT.GET.KV_<source>` get **load-balanced across all mirror servers cluster-wide**. We watched a request from a us-ord-attached client get answered by `KV_demo-r3-na_mirror_in-bom-2` (Mumbai) when both us-ord and ca-central had local mirrors. There is no "prefer local" affinity in NATS for direct gets across mirrors.

**Decision**: Adapter now maintains a `bucket → KV_<bucket>_mirror_<region>` map (refreshed every 30s by scanning local stream names matching the suffix) and rewrites reads:

- If a local mirror exists for `bucket`, call `js.GetLastMsg(localMirrorName, "$KV.<bucket>.<key>", nats.DirectGet())` — the request goes to `$JS.API.DIRECT.GET.<mirror-name>` which only the local mirror's server serves. **Sub-millisecond.**
- Else fall back to `kv.Get(key)` against the source. **One cross-region hop** (50-80ms when reading from a non-local replica of the source RAFT).
- Versioned reads (`?revision=N`) always go to the source.

`X-Read-Source` (and `X-Read-Stream` when local) headers expose which path served — useful for the topology UI and for debugging future regressions.

**Numbers (verified across all 26 leaves, intra-pod localhost test):**

| Region group | X-Latency-Ms | X-Read-Source |
|---|---|---|
| 23 mirror regions (eu, ap, br, in, etc.) | **0-1 ms** | `local-mirror` |
| 3 R3 RAFT regions for `KV_demo-r3-na` (us-west, us-sea, ca-central) | 53-81 ms | `source` |

The 3 RAFT regions are by design — we skip mirroring on a region that already hosts a RAFT replica of the source (placement conflict). Reads there go to the 3-replica source RAFT; NATS picks one of the three replicas (sometimes local, often remote). Acceptable: those regions get strongly-consistent reads from the same RAFT cluster, and a future improvement could pin those reads to the local replica via `$JS.API.STREAM.MSG.GET.KV_<bucket>` with a per-server hint.

**Why not just bind the KV API to the local mirror?** `nats.go` doesn't expose "bind KeyValue to mirror by name" — the `KeyValue(name)` lookup always resolves to the source stream. Re-exposing the mirror as a separate KV bucket would break the user's mental model (one bucket name, many physical streams). Reading via the low-level `GetLastMsg` keeps the public API ("PUT/GET on `bucket`") unchanged while routing reads locally.

**Consequences**:
- Mirrors-everywhere now actually delivers the local-read promise we made in ADR-008. Before this, mirrors only added cluster fan-out without serving requests.
- Eventual consistency window stays as it was — replication lag is whatever NATS mirror replication takes (sub-second under normal load); the topology UI will surface it.
- Adapter has 30s blind spot for new buckets (refresh cadence). Acceptable for create→read flows since the source still serves until the mirror is built; UI shows the transition.

**Versions**: adapter `v0.2.1`, control `v0.1.9`. Rolled to all 27 nodes 2026-04-29.

---

## ADR-019 — FWF→adapter calls use plain HTTP (TLS off the data plane); NATS-KV now beats Cosmos
**Status**: Accepted (2026-04-29)

**Context**: ADR-018 isolated the 45ms gap to FWF's wasi-http TLS handshake (every HTTPS call to a same-metro endpoint pays full handshake; FWF's TLS path is ~10x slower than a regular Linux TLS handshake on the same network). Switching the FWF→adapter hop to plain HTTP eliminates the tax.

**Decision**: User Spin app's `ADAPTER_BASE` now points at `http://edge.nats-kv.connected-cloud.io:8080`. LZ NB Service exposes port 8080 in addition to 80 (so the URL is consistent across LZ + leaves; leaves already had hostPort 8080 from the DaemonSet). External clients (browsers / server-to-server SDK calls) still hit HTTPS on `:443` — only the in-Akamai FWF→adapter hop drops to HTTP.

**Numbers (intra-Chicago, FWF Spin → us-ord adapter, steady state):**
| Path | Latency | vs Cosmos |
|---|---|---|
| FWF → http://edge.nats-kv:8080 (GTM hostname) | **6-9ms** | 2-3× faster |
| FWF → http://172.237.141.164 (NB IP, no DNS) | **3.5-4ms** | 5-6× faster |
| KV PUT (HTTP, GTM hostname) | **5.5-8ms** | 3× faster |
| KV GET (HTTP, GTM hostname) | **7-8ms** | 3× faster |
| Cosmos via spin:key-value WIT | 19-23ms | baseline |

DNS lookup costs 3-5ms per call on FWF's wasi-http (hostname=6-9ms vs IP=3.5-4ms).

**Rationale for accepting plain HTTP on this hop**:
1. Both ends are on the same Akamai/Linode network (Akamai Connected Cloud ASN 63949 to Linode Chicago metro). Bearer-token auth at the adapter still gates access. The TLS proxy/inspection layer that's slowing FWF's handshake adds no security on this trust boundary.
2. External access (anything outside FWF) continues to use HTTPS on 443. The wildcard `*.nats-kv.connected-cloud.io` cert is unchanged.
3. Reverting is one config change (`const ADAPTER_BASE`) if security posture changes.

**Consequences**:
- Headline pitch flips: "NATS-KV is faster than Cosmos on FWF" — concrete 3-6× wins.
- The TLS-handshake bug for Fermyon to file (ADR-018) is still worth filing — fixing it would let everyone on FWF use HTTPS at HTTP latency.
- Future native `key-value-nats` factor crate would shave another 3-5ms (no DNS, no HTTP framing) for a sub-1ms KV op.

**Cleanup**: probe Linode (96843977 / 172.236.104.189) and `probe.nats-kv.connected-cloud.io` DNS torn down post-test.

---

## ADR-018 — FWF wasi-http TLS handshake is the entire 50ms gap (intra-Chicago test)
**Status**: Documented (2026-04-29)

**Context**: ADR-017 said the 50ms gap was "FWF-specific, not Spin/wasi-http inherently" based on local-Spin benchmarks. Today we narrowed it further by deploying identical Spin app to FWF and probing three endpoint shapes from FWF, all in the same Chicago metro as FWF's egress IP (`172.239.46.208`):

| Path from FWF Spin | Steady state | Notes |
|---|---|---|
| `http://<linode>:8888/` plain HTTP | **3.5ms** | Network/Spin floor on FWF; no TLS |
| `https://<linode>:8443/` direct (no NB, valid LE cert) | **49ms** | TLS adds ~45ms over plain HTTP |
| `https://us-ord-NB/` (existing adapter path) | **52-60ms** | NB adds 3-7ms over direct HTTPS |

For comparison, a regular Chicago Linode VM (claudebot, sub-1ms ping to the probe) doing 5 sequential `curl https://...` calls (each a full TLS-1.3 handshake from scratch) completes in **5ms total**. FWF Spin doing the equivalent takes ~49ms — **10x slower than a standard TLS handshake on the same intra-DC network**.

**Decision/finding**: The 50ms gap is concentrated in FWF's wasi-http TLS handshake path, not the network, NodeBalancer, adapter, NATS, or Spin/wasi-http inherently (local-`spin up` of the same app on a regular Chicago Linode hits the HTTPS probe in 5-10ms). Likely root causes (in priority order, for Fermyon to investigate):

1. TLS session-ticket / 1-RTT resumption not actually working — every request pays handshake, despite TCP pool being warm.
2. WASM-level crypto operations are slow in the FWF Spin runtime.
3. An Akamai-side TLS-inspection proxy in the outbound path (would explain HTTP=fast, HTTPS=slow uniformly).

**Implications**:
1. NATS-KV's network/adapter is fully OK. The KV-vs-Cosmos perf gap is due to (a) Cosmos using `spin:key-value` WIT (in-process) vs. our `wasi-http` outbound, AND (b) FWF's HTTPS-specific overhead.
2. Self-hosted Spin on LZ would deliver intra-Chicago HTTPS at 5-10ms, on par with Cosmos's 22ms.
3. A native `key-value-nats` factor crate (ADR-001) bypasses wasi-http entirely — would close the gap fully on FWF.
4. **Concrete bug report for Fermyon**: ship a Hello-world Spin app that does 100 GETs to the same HTTPS endpoint; observe each takes ~50ms. Compare to local `spin up`: each takes 2-5ms. That's the bug.

**Probe still up at `172.236.104.189:8888` (HTTP) and `:8443` (HTTPS, valid wildcard cert).** Used by `/api/probe-claudebot` and `/api/probe-https` in the user app for re-tests.

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

