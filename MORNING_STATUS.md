# Morning status — 2026-04-29

Summary of overnight autonomous work after Brian went to bed at ~03:25 UTC.

## TL;DR

- **Multi-tenancy 401 issue is fixed.** Adapters now poll the control plane every 30s for active key hashes; any control-plane-issued user key works against any of 27 regional adapters.
- **NATS mesh is now actually full-mesh, not hub-and-spoke** (root cause of earlier R3 failures). Fixed by populating `NATS_ROUTES` on every node with all 27 peer URLs.
- **R3 / R5 placement-by-geo works.** 5 demo buckets created with R1, R3-na, R3-eu, R3-ap, R5-global topology.
- **Mirror streams added** to demonstrate per-geo read replicas. Each R3 bucket now has mirror streams in the other 3 geos (each shown on the topology UI).
- **Topology UI live** at `/topology` on the user Spin app — geographic SVG with replica polygons, leader rings, mirror dots with dashed lines, live replication lag per peer/mirror.
- **FW posture intact** — `nats-kv-fw` strict (DROP default, only 443 + 6222 public, admin-only on 22/4222/8222).
- **27/27 HTTPS endpoints healthy.** JS meta cluster_size 27, leader rotates normally.

## What's deployed

| Surface | URL | Status |
|---|---|---|
| User app | https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app | live (`/`, `/play`, `/topology`, `/dash`, `/claim/<token>`) |
| Admin app | https://947e30b4-0481-4a14-bb05-fb83580e8594.fwf.app | live (paste admin token, manage tenants/invites) |
| Control plane | https://cp.nats-kv.connected-cloud.io | live (admin API + claim flow + `/v1/internal/keys`) |
| GTM data plane | https://edge.nats-kv.connected-cloud.io | live (~50-70ms steady-state from FWF, 100-200ms cold) |
| Per-region adapters | https://`<region>`.nats-kv.connected-cloud.io | 27/27 live, valid wildcard cert |

Admin token: `06ddd5c4a74ae2858b2d4c30c9d9dac601e9d1e5e6ffdc0d79cd662d9c1d3511` (also in Vault `api/nats-kv/control`).

## Demo buckets pre-seeded — for the topology UI

| Bucket | Replicas | RAFT placement | Mirrors |
|---|---|---|---|
| `demo` | R1 | random (currently us-mia) | none — for shared playground |
| `demo-r1-us-ord` | R1 | us-central | 4 (na, eu, ap, sa) |
| `demo-r3-na` | R3 | us-west + ca-central + us-sea | 3 (eu, ap, sa) |
| `demo-r3-eu` | R3 | eu-central + de-fra-2 + fr-par-2 | 4 (na, eu, ap, sa) |
| `demo-r3-ap` | R3 | ap-west + in-maa + jp-osa | 4 (na, eu, ap, sa) |
| `demo-r5-global` | R5 | us-east + ap-northeast + br-gru + id-cgk + it-mil | 0 (R5 already global) |

The `demo` bucket has playground seed data: `greeting`, `counter`, and `users.<alice|bob|carol|dave|eve>.<session|profile>` for the subject-wildcard demo (5 keys returned for `?match=users.*.session`).

## Critical structural finding (worth knowing)

The 27-node "mesh" was actually **hub-and-spoke** — each leaf had 4 routes, all to LZ. Gossip wasn't dialing leaf-to-leaf despite cluster_advertise being set. R3 streams across regions therefore couldn't form quorum.

**Fix:** explicit `NATS_ROUTES` env on every node listing all 27 peer URLs (~104 routes per node now). JS R3 placement with `geo:` tags works deterministically. Captured as **ADR-016** in `DECISIONS.md`.

## What I did NOT touch

- `mortgage-inference` GPU node (`lke589020-869396-…` in latency-fra2) — left alone per your call
- The orphan R3 streams from earlier attempts — JetStream meta still has them in some intermediate state, but they don't block anything
- Akamai property in front (raw fwf.app URLs)
- DS2 stream wiring
- Demo-puller robot creds (still need rotation per your acknowledgment)
- Admin token (unchanged from when you generated it)

## Open / known-not-quite-right

- **Latency-driven placement engine (#4 proper)** — placement currently uses static `geo:` tags. Next: read pairwise RTT from project-latency's hub and auto-pick the best triple within a geo (or anchor + auto-pick).
- **Adapter prefix-rewrite for tenant isolation** — buckets created via `/v1/me/buckets` are named `<tenant_id>__<name>` and you call them via that full name. Cleaner UX would have the adapter automatically rewrite `/v1/kv/<short>/<key>` → the prefixed name based on the bearer's tenant. Deferred.
- **Latency-driven placement engine (#4)** — placement currently uses static `geo:` tags. Next: read pairwise RTT from project-latency's hub and auto-pick the best triple within a geo (or anchor + auto-pick).
- **GTM cold-start latency** — first ~5 calls from fwf are 100-200ms; warm steady-state is 50-70ms server-side. Per your note, it'll improve with bake time. Will re-baseline tomorrow.
- **`ap-southeast` (Sydney) classified as `geo:ap`** in the adapter's `geoOfRegion` (matches "ap-" prefix) — strictly should be `geo:oc`. Cosmetic; revisit if we add OC-specific placements.

## Code changes shipped overnight

```
adapter v0.1.6 → v0.1.8
  + NATS server tags: region:<id> + geo:<g>
  + /v1/admin/buckets returns full topology (replicas, leader, peers, lag, placement_tags)
  + /v1/admin/buckets discovers mirror streams (KV_<bucket>_mirror_*) and reports per-mirror lag

control v0.1.5 → v0.1.7
  + /v1/me           — user-key auth resolves to tenant info
  + /v1/me/buckets   — list & POST create bucket; placement (geo) + auto-mirrors in other geos
  + bucket naming: <tenant_id>__<name> (NATS forbids dots in bucket names)

user spin app
  + /topology page (SVG world map, bucket polygons, leader rings, mirror dashed lines + lag table)
  + /dash now has self-service bucket creation form
  + /play subject-wildcard fix (proxy was dropping query string)
  + nav now includes "topology"

infra
  + NATS_ROUTES on every node (LZ + 26 leaves) listing all 27 peer URLs
  + 5 demo buckets + 15 mirror streams created (manually via nats CLI)
  + ADR-016 documenting the full-mesh routing fix
```

Git log:
```
d4f26bc feat(ui): mirror visualization in /topology — dashed lines + lag
ce40e92 feat(adapter v0.1.8): bucket summary now reports mirror streams + lag
a09ce62 docs: ADR-016 (explicit full-mesh routes); MORNING_STATUS.md
be228cd feat(ui): /topology page + 5 demo buckets (R1/R3/R5 across geos)
da66872 feat(adapter v0.1.7): geo tags on NATS server + per-bucket topology in /v1/admin/buckets
fcaa907 feat: HTTP-poll keys from control plane (drops dependency on cross-cluster KV watch)
… etc
```

## Verify when you wake up

1. **Open https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app/topology** — should render 5 demo buckets as RAFT polygons + mirror dots over a world map. Click each bucket on the left to see its consistency domain visualized.
2. **Open /play** — playground works without sign-in (uses shared demo token). Try the subject-wildcard demo (now correctly returns 5 `users.*.session` keys).
3. **Issue an invite from /admin app, claim it, sign in, retry the playground** — your tenant key is now accepted by all 27 adapters (the original 401 bug is dead).

If anything looks off, MORNING_STATUS notes give the context, and the git log tells the story.
