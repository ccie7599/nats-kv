# SCOPE.md — project-nats-kv

## Overview
Globally distributed NATS JetStream KV service that Akamai Functions (Fermyon Spin) can use as a backend KV store. HTTP adapter co-located with embedded NATS on each LKE leaf cluster, fronted by GTM for nearest-region routing. Two-app Spin UI on Akamai Functions for self-service signup, playground, and admin operations. Latency-driven placement engine selects the optimal RAFT triple (or quintuple) for each bucket using real-time data from `project-latency`.

Pitched as an internal Akamai POC: a faster, NATS-native alternative to the Cosmos-backed KV that ships with Akamai Functions today, with primitives Cosmos can't expose (atomic increment, history, subject-pattern wildcards, distributed locks).

## Tier Classification
**Tier 2** — Reusable demo / reference asset

## Domains
- `nats-kv-edge.connected-cloud.io` — data plane, GTM-routed to nearest of 27 datacenters
- `nats-kv.connected-cloud.io` — control plane API on hub (tenants, keys, invites)
- `nats-kv-demo.connected-cloud.io` — Akamai Functions Spin apps (user UI + admin UI, path-routed)
- `{short}.nats-kv.connected-cloud.io` — per-region direct adapter (testing / explicit pinning)

## Goals
1. Backend KV that any Spin function on Akamai Functions can call via HTTP, returning sub-10ms reads from the nearest of 27 regions.
2. Beat Cosmos (current Spin KV default) on read p50/p99, write p50, throughput per dollar, and tail latency under load — measured side-by-side in the demo UI.
3. Expose NATS-unique primitives the Spin KV WIT can't express: atomic increment, revision history, subject-pattern key wildcards, CAS, watch via SSE, distributed-lock recipe.
4. Latency-driven RAFT placement: auto / anchor+auto-pick / manual modes, R1/R3/R5 replication tiers, mirrors on every node.
5. Multi-tenant POC for internal Akamai engineers — self-service signup via one-use invite URLs issued by admin.
6. Visualize bucket placement, replication lag, write propagation, and consistency drift on a D3 retro flat map (similar style to project-latency).

## Architecture Summary
- **27 KV nodes**: 26 leaf LKE clusters (presales account) + presales-landing-zone hub. Second node pool per cluster (`g8-dedicated-4-2`, 50GB Block Storage), `nodeSelector tier=kv` + taint to isolate from latency probes. `hostNetwork` on standard NATS ports — no port collision because separate node from latency probe.
- **Adapter binary**: single Go binary per node containing embedded NATS server (JetStream enabled), HTTP API, OTel instrumentation, and watch on `_kv-admin` bucket for live API key validation.
- **JetStream KV**: bucket-per-tenant-namespace, R1/R3/R5 tiers, placement tags (`region:<short>`, `geo:<region>`) for explicit RAFT membership. Mirror streams on all 26 other nodes by default.
- **Placement engine**: pulls real-time RTT matrix from project-latency hub API, enumerates candidate replica sets, scores by median quorum edge (`e_ceil(k/2)`), picks winner. Modes: `auto`, `anchor` (default — closest probe to caller IP), `manual`.
- **Control plane**: Go service on hub (`nats-kv.connected-cloud.io`). Handles tenant signup, NATS account provisioning, API key issuance, invite tokens (one-use, expiring), audit log. Two service-token scopes: `user-self-service` and `admin`. Tenant/key state lives in NATS `_kv-admin` bucket so adapters watch for live revocation; SQLite on hub holds invite tokens + audit log.
- **GTM**: domain `nats-kv.akadns.net`, type `full`, `load_feedback=true` (capability enabled, no load objects today), `load_imbalance_percentage=100` for pure performance routing. 27 datacenters, one performance property `kv` with HTTPS liveness on `/v1/health`.
- **Two Spin apps on Akamai Functions**, both at `nats-kv-demo.connected-cloud.io`:
  - `nats-kv-user` (default route) — landing, playground, self-service dashboard, claim flow
  - `nats-kv-admin` (`/admin/*`) — invite issuance, regen-key button, suspend, delete. Bearer admin token only (no EAA).
- **Observability**: prometheus-nats-exporter + adapter metrics + OTel traces, scraped by latency hub's Prometheus, Grafana dashboards with side-by-side Cosmos vs NATS panels.

## Non-Goals
1. **Production-grade compliance posture** — internal POC only. We expose raw `geo_in`/`geo_not` placement constraints; we do not advertise GDPR/SOC2/HIPAA compliance.
2. **A native `key-value-nats` Spin factor crate** — deferred. Function-side calls go via HTTP adapter using `wasi:http/outgoing-handler` with default connection pooling. Crate is a possible future win if Akamai Functions accepts custom KV providers.
3. **Function-held watches** — Spin's request-response model and Akamai Functions wall-time limits make long-lived watches inside a function the wrong shape. Watches are server-side; consumers are long-lived clients (browser SSE) or Spin components invoked by NATS triggers.
4. **First-class lock / lease / leader-election endpoints** — shipped as documented recipes over CAS + TTL primitives, not opinionated APIs.
5. **Live RAFT auto-rebalancing on latency drift** — drift score surfaced and manual `POST /rebalance` op available; auto-rebalance opt-in only.
6. **EAA / SSO integration for admin UI** — bearer token sufficient for POC.
7. **Standalone infra** — reuse the 27 LKE clusters from project-latency / presales account; do not provision a parallel fleet.

## Exit Criteria
- [ ] All 27 KV nodes deployed with adapter + embedded NATS + 50GB Block Storage
- [ ] Control plane live at `nats-kv.connected-cloud.io` with admin SSO + token auth
- [ ] GTM routing live at `nats-kv-edge.connected-cloud.io` with all 27 DCs healthy
- [ ] User Spin app deployed on Akamai Functions: signup, playground, self-service dashboard
- [ ] Admin Spin app deployed: invite issuance, regen-key, suspend, delete
- [ ] Latency-driven placement engine: auto + anchor + manual modes working with `e_ceil(k/2)` scoring
- [ ] R1/R3/R5 buckets demonstrably created with correct placement and mirrors-everywhere
- [ ] Side-by-side benchmark UI: live histograms for read/write p50/p99 against Cosmos and NATS
- [ ] KV map visualization: bucket triangles, replication lag, drift score, optimal-placement overlay
- [ ] Recipe docs: distributed-lock, leader-election, idempotency-key, rate-limit-bucket, lease
- [ ] Grafana dashboards live with Cosmos vs NATS panels
- [ ] Terraform deploys and destroys full stack cleanly (KV node pools + Block Storage + 3 properties + GTM)
- [ ] First external Akamai engineer onboarded via invite URL and successfully runs a benchmark

## Cost
- 27 KV nodes (`g8-dedicated-4-2` @ $45/mo) = $1,215/mo
- 27 Block Storage volumes (50GB @ $5/mo) = $135/mo
- Akamai property + GTM domain (presales account, no marginal cost)
- Akamai Functions usage (negligible at POC volume)
- **Total: ~$1,350/mo incremental** on top of project-latency's $667/mo
- Can `terraform destroy` the KV node pools when not demoing — leaves latency probe nodes intact

## Dependencies
- `project-latency` — pairwise RTT matrix consumed by placement engine. KV is non-functional without it.
- `presales` Linode account — host of the 26 leaf clusters and hub cluster.
- Akamai EdgeGrid credentials — `~/.edgerc` `default` section.
- Linode CLI profile `presales` — for KV node provisioning and Block Storage.
