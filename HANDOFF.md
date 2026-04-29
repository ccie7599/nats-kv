# HANDOFF — NATS-KV for Akamai Functions

**Status**: demo-grade research POC. Live, working, and exercisable. Not production. See `SCOPE.md` for the formal scope contract and `DECISIONS.md` for 22 ADRs covering every architecture choice (and the misses).

**Owner**: Brian Apley (TSA, Akamai). Repo: github.com/ccie7599/nats-kv.

**What this gives you**: a richer KV substrate for Akamai Functions (atomic increment, revision history, CAS, subject-pattern wildcards, geographic placement control, mirrors-everywhere reads) exposed as an HTTP API any Spin function — or any HTTP client — can use today, without waiting for a native `key-value-nats` Spin factor crate.

---

## Try it in 5 minutes

| Surface | URL |
|---|---|
| **User app** (playground, dashboard, topology, docs, verify, load test, API explorer) | `https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app/` |
| **Admin app** (mint invites, list tenants, regen keys) | `https://947e30b4-0481-4a14-bb05-fb83580e8594.fwf.app/` |
| Data plane (GTM-routed) | `https://edge.nats-kv.connected-cloud.io/v1/...` |
| Control plane (LZ only) | `https://cp.nats-kv.connected-cloud.io/v1/...` |

**Open demo bearer**: `akv_demo_open` — works against the shared `demo` bucket. No claim flow needed.

**Steps**:
1. Hit the user app `/play` → run a side-by-side bench against Cosmos to see the perf shape.
2. `/verify` → run all 10 functional tests against the `demo` bucket. Should be 10/10 pass.
3. `/topology` → see the 27-region mesh with bucket placement overlays.
4. `/docs` → full API reference, sample Spin function code, throughput projections, caveats.
5. `/api-explorer` → Swagger UI; Authorize once with `akv_demo_open`, run any endpoint live.
6. `/loadtest` → drive concurrent traffic and measure ops/sec, p50/p95/p99.

For your own tenant + buckets, the admin app issues invite URLs that one-shot claim into a tenant + API key. Admin token is in Vault at `api/data/nats-kv/control` (key `admin_token`); also retrievable from the live LZ pod with `kubectl exec -n demo-nats-kv deploy/kv-control -c control -- cat /vault/secrets/admin-token`.

---

## What's deployed (current versions)

| Component | Version | Where |
|---|---|---|
| `kv-adapter` | `v0.2.5` | All 27 LKE clusters (1 LZ + 26 leaves), presales account |
| `kv-control` | `v0.1.20` | LZ only (`presales-landing-zone`, `demo-nats-kv` namespace) |
| `nats-kv-user` Spin app | latest | Akamai Functions, `3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app` |
| `nats-kv-admin` Spin app | latest | Akamai Functions, `947e30b4-0481-4a14-bb05-fb83580e8594.fwf.app` |
| GTM property | `nats-kv` | `connectedcloud5.akadns.net` (shared domain) |
| Latency hub dependency | external | `latency-demo.connected-cloud.io` (project-latency) |

LZ adapter and control plane are reconciled by ArgoCD from this repo's `k8s/` dir. Leaf adapters are managed imperatively (DaemonSet `kubectl set image` across 26 clusters).

---

## Functional verification

`/verify` runs 10 tests covering every NATS-KV surface against a chosen bucket:

1. PUT then GET round-trip
2. DELETE then GET → 404
3. Revision history (3 PUTs → ≥3 revisions in history)
4. Read specific historical revision (`?revision=N`)
5. CAS with correct `If-Match` succeeds
6. CAS with stale `If-Match` returns 412
7. Atomic increment (5× incr by 1 → value=5)
8. Subject-pattern wildcard (`keys?match=users.*.session`)
9. Local-mirror reads (verifies `adapter_ms ≤ 5`)
10. Bucket appears in `/v1/admin/buckets`

Each test uses random key suffixes and cleans up after itself. Run it after any version bump or new bucket creation.

---

## Operations cheatsheet

**Restart the LZ control plane** (e.g., to flush state):
```
kubectl --context=lke575271-ctx -n demo-nats-kv rollout restart deploy/kv-control
```
Argo will reconcile back to the manifest version.

**Roll a new adapter image to all 26 leaves** (LZ goes via Argo):
```
for cfg in /tmp/leaves/*.yaml; do
  KUBECONFIG=$cfg kubectl set image -n demo-nats-kv \
    ds/kv-adapter adapter=harbor.connected-cloud.io/presales/nats-kv-adapter:vX.Y.Z &
done; wait
```
(See `/tmp/leaves/` cluster-id ↔ kubeconfig mapping established in session prep — re-fetch via `linode-cli lke clusters-list` against the presales token.)

**Force a stream's RAFT leader to a specific region** (used by control plane after bucket creation per ADR-021):
```
nats stream cluster step-down KV_<bucket> --force
# or with placement constraint (preferred):
nats req '$JS.API.STREAM.LEADER.STEPDOWN.KV_<bucket>' \
  '{"placement":{"tags":["region:us-ord"]}}'
```

**Rebuild the demo bucket** (e.g., after a cluster wipe — see ADR-022):
The adapter's `ensureBucket` flow auto-creates `demo` on first hit via the control plane's `/v1/internal/buckets/ensure` endpoint, anchored to whatever leaf takes the call. To force a specific anchor, delete first then PUT from the desired region.

---

## Known caveats (also in `/docs §10`)

- **No SLA, no on-call.** Demo cluster on the presales account. ADR-022 documents a previous total cluster wipe (R1 admin storage caused meta-RAFT to propagate an empty snapshot). Admin buckets are now R5/NA so this can't repeat the same way; other failure modes are surely possible.
- **Auth is bearer-only.** No EAA, no per-key ACL granularity beyond active/revoked.
- **GTM ECS not enabled** on the shared `connectedcloud5.akadns.net` domain. Backend request open. Until then, FWF traffic routing to a specific NA leaf is non-deterministic per-resolver — affects which leaf a function lands on (see `/docs §3` and ADR comments).
- **Throughput numbers in `/docs §9` are projections, not measured.** `/loadtest` lets anyone produce defensible numbers; please replace the projections in the doc with measured values once you've done a representative run.
- **Spin's wasi-http is the latency floor**. Until a native `key-value-nats` Spin factor crate exists, every function call to NATS-KV pays one HTTP hop (~5-10ms steady state to the local NB).

---

## What's NOT done that you might want

- **Multi-language SDK / code samples**. Only Rust example currently in `/docs`. TS and Python would be small additions.
- **OTel traces from inside the adapter** (only metrics today). Pipeline exists at the latency hub.
- **Grafana dashboards**. SCOPE.md lists this in exit criteria; deferred. Prometheus already scrapes `/varz` from each adapter via the OTel pipeline.
- **Live load-test results in `/docs §9`** — placeholder projections still in place.
- **Native `key-value-nats` Spin factor crate** — the structural fix for the FWF↔adapter hop. Out of scope for this POC; data here makes the case for someone to take it on.
- **Auto-rebalance on latency drift** — SCOPE non-goal but flagged in `DECISIONS.md`.

---

## Reading order if you've never seen this before

1. `SCOPE.md` — what we set out to do, scope lock, non-goals, exit criteria, cost
2. `DECISIONS.md` — 22 ADRs in order; ADR-001/2 frame the architecture, ADR-008 / 020 / 021 cover placement and read-locality (the demo's headline story), ADR-022 is the post-mortem worth knowing about
3. The `/docs` page in the live app — same content as the API reference, with the comparison table that matters for sales conversations
4. This file — for handoff specifics

For sales-grade narrative: the side-by-side numbers in `/play` against the Cosmos comparison row in `/docs §2` are the elevator pitch. The throughput delta vs Spin's WIT KV (~1k reads/sec, ~100 writes/sec gated) is the second story.
