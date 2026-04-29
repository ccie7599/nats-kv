# HANDOFF — NATS-KV for Akamai Functions

**Status**: demo-grade research POC. Live, working, and exercisable. Not production. See `SCOPE.md` for the formal scope contract and `DECISIONS.md` for 22 ADRs covering every architecture choice (and the misses).

**Owner**: Brian Apley (TSA, Akamai). Repo: github.com/ccie7599/nats-kv.

**What this gives you**: a richer KV substrate for Akamai Functions (atomic increment, revision history, CAS, subject-pattern wildcards, geographic placement control, mirrors-everywhere reads) exposed as an HTTP API any Spin function — or any HTTP client — can use today, without waiting for a native `key-value-nats` Spin factor crate.

---

## Try it in 5 minutes

| Surface | URL | Auth |
|---|---|---|
| **User app** (playground, dashboard, topology, docs, verify, load test, API explorer) | `https://nats-kv.connected-cloud.io/` (Akamai) or `https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app/` (FWF direct) | UI gate token (`?access=<token>`) |
| **Admin app** (mint invites, list tenants, approve invite requests, regen keys) | `https://nats-kv-admin.connected-cloud.io/` (Akamai) or `https://947e30b4-0481-4a14-bb05-fb83580e8594.fwf.app/` (FWF direct) | Admin gate token + admin bearer |
| Data plane (GTM-routed, no UI gate) | `https://edge.nats-kv.connected-cloud.io/v1/...` | KV bearer |
| Control plane (LZ only, no UI gate) | `https://cp.nats-kv.connected-cloud.io/v1/...` | tenant bearer for `/v1/me/*`; admin bearer for `/v1/admin/*` |
| Grafana — nats-kv property dashboard | `https://grafana.connected-cloud.io/d/nats-kv` | LZ Grafana login |
| Grafana — DS2 cross-demo analytics | `https://grafana.connected-cloud.io/d/ds2-cdn-analytics` (filter `demo=nats-kv`) | LZ Grafana login |

**Tokens** (rotate via `spin aka variables set <name>=<value>`):

| Token | Default | Purpose | Where stored |
|---|---|---|---|
| User-app UI gate | `demo-open-2026` | Unlocks the demo UI shell (`/play`, `/dash`, `/docs`, etc.) | Spin variable `ui_gate_token` |
| Admin-app UI gate | `admin-gate-2026-rotateme` | Unlocks the admin UI shell | Spin variable `admin_gate_token` |
| Admin bearer | (Vault) | Lets admin actually call control-plane admin endpoints | `api/data/nats-kv/control` → `admin_token` |
| Open demo KV bearer | `akv_demo_open` | Hardcoded; lets anyone read/write the shared `demo` bucket | source |

**Three gate layers, intentionally separate:**
1. **UI gate token** (this page) — unlocks the *page* for browsing.
2. **KV bearer** — lets you *do KV operations* once you're past the gate.
3. **Admin bearer** — lets you *manage tenants/invites*, gates `/v1/admin/*`.

**Steps for a vetted user (after admin shares a URL)**:
1. Click the URL the admin app generated — it carries `?access=<gate>` AND `/claim/<invite>`. The first hit sets the UI gate cookie and runs the claim flow, leaving the user with a tenant + KV bearer + UI cookie. They land on the dashboard.
2. From `/play` they can run side-by-side bench, `/verify` to run all 10 smoke tests, `/topology` to see the cluster live, `/loadtest` to drive throughput, `/docs` for API reference, `/api-explorer` for Swagger.

**Steps for a fresh visitor (no link)**:
1. Hit `https://nats-kv.connected-cloud.io/` — sees the gate page.
2. Submits the "Request invite" form (name + email + reason).
3. Request stored in NATS KV bucket `kv-admin-invite-requests-v1` (R5/NA).
4. Brian sees it in admin app → "Pending invite requests" panel → clicks Approve → admin app shows a complete share URL (Akamai + FWF-direct variants) → Brian sends to requester.
5. Requester clicks the URL → completes flow per "vetted user" steps above.

**Walking around as Brian**:
- Admin gate token + admin bearer both go in via the admin app's UI.
- The admin bearer can also be retrieved live from the LZ pod: `KUBECONFIG=/tmp/lz-kubeconfig.yaml kubectl exec -n demo-nats-kv deploy/kv-control -c control -- cat /vault/secrets/admin-token`

---

## What's deployed (current versions)

| Component | Version | Where |
|---|---|---|
| `kv-adapter` | `v0.2.5` | All 27 LKE clusters (1 LZ + 26 leaves), presales account |
| `kv-control` | `v0.1.21` | LZ only (`presales-landing-zone`, `demo-nats-kv` namespace) |
| `nats-kv-user` Spin app | latest (gate-enabled) | Akamai Functions, `3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app` |
| `nats-kv-admin` Spin app | latest (gate + invite-requests panel) | Akamai Functions, `947e30b4-0481-4a14-bb05-fb83580e8594.fwf.app` |
| GTM property (data plane) | `nats-kv` | `connectedcloud5.akadns.net` (shared domain) |
| Akamai property (UI front-door) | `nats-kv` | single property, two hostnames (demo + admin), per-host rules — TF in `~/project-landing-zone/.../demos/nats-kv/` |
| DataStream 2 stream | `nats-kv-ds2` | shared ClickHouse ingest at `ds2-im-demo.connected-cloud.io` |
| Grafana dashboard | uid `nats-kv` | `https://grafana.connected-cloud.io/d/nats-kv` (LZ Grafana ConfigMap-managed) |
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

## Bringing up the Akamai property (one-time, post-cert)

The single nats-kv Akamai property is defined in TF at `~/project-landing-zone/presales-landing-zone/infra/terraform/demos/nats-kv/`. Hostnames + cert SANs (293468 enrollment) need to be in place before activation. Procedure:

```bash
# 1. Add nats-kv.connected-cloud.io and nats-kv-admin.connected-cloud.io as SANs
#    on enrollment 293468 (sse.connected-cloud.io). Use the helper at
#    /tmp/akamai_cps.py from your local edgegrid venv:
/tmp/akamai-venv/bin/python /tmp/akamai_cps.py add-sans 293468 \
  nats-kv.connected-cloud.io,nats-kv-admin.connected-cloud.io

# 2. Ack the pre-verification warnings (one-time prompt about overlapping cert):
/tmp/akamai-venv/bin/python /tmp/akamai_cps.py ack-warnings 293468

# 3. CPS asks for DNS challenge records — fetch values, add as TXT in the
#    Akamai connected-cloud.io zone, then ack lets-encrypt-challenges-completed.
#    (See the inline DNS-PUT loop in the deploy-day notes; uses Edge DNS API.)

# 4. After cert deploys (~15-45 min), terraform apply.
cd ~/project-landing-zone/presales-landing-zone/infra/terraform/demos/nats-kv
terraform init && terraform apply

# 5. Two-phase DS2: re-apply with the issued stream id
STREAM_ID=$(terraform output -raw datastream_id)
sed -i "s/ds2_stream_id        = 0/ds2_stream_id        = ${STREAM_ID}/" terraform.tfvars
terraform apply

# 6. Patch the demo-usage Grafana dashboard's `demo` variable so nats-kv
#    appears in the dropdown:
CP=$(terraform output -raw cp_code_id)
cd ~/project-nats-kv && ./observability/grafana/apply.sh --cp "${CP}"
```

After step 6, both UI hostnames are live behind Akamai + DS2 logging is flowing into ClickHouse + the nats-kv Grafana dashboard begins populating.

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

- **Multi-language SDK / code samples**. Only Rust example currently in `/docs §6`. TS and Python would each be ~50 lines.
- **OTel traces from inside the adapter** (only metrics today). Pipeline exists at the latency hub.
- **Live load-test results in `/docs §9`** — placeholder projections still in place. Run `/loadtest` for ~30 min and replace.
- **Native `key-value-nats` Spin factor crate** — the structural fix for the FWF↔adapter hop. Out of scope for this POC; data here makes the case for someone to take it on.
- **Auto-rebalance on latency drift** — SCOPE non-goal but flagged in `DECISIONS.md`.
- **GTM ECS** is currently disabled at the shared `connectedcloud5.akadns.net` domain — backend request open. Until then, FWF traffic routes to a per-resolver fixed mapping rather than per-client subnet.
- **Email/Slack notification on new invite request** — today the request just sits in the admin app's "Pending requests" panel until you check it. Adding a Slack webhook would be ~20 lines of Go in `internalInviteRequestHandler`.

---

## Reading order if you've never seen this before

1. `SCOPE.md` — what we set out to do, scope lock, non-goals, exit criteria, cost
2. `DECISIONS.md` — 22 ADRs in order; ADR-001/2 frame the architecture, ADR-008 / 020 / 021 cover placement and read-locality (the demo's headline story), ADR-022 is the post-mortem worth knowing about
3. The `/docs` page in the live app — same content as the API reference, with the comparison table that matters for sales conversations
4. This file — for handoff specifics

For sales-grade narrative: the side-by-side numbers in `/play` against the Cosmos comparison row in `/docs §2` are the elevator pitch. The throughput delta vs Spin's WIT KV (~1k reads/sec, ~100 writes/sec gated) is the second story.
