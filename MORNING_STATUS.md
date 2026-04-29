# Morning status — 2026-04-29

Summary of overnight autonomous work after Brian went to bed at ~03:25 UTC.

## TL;DR

- **Multi-tenancy 401 issue is fixed.** Adapters now poll the control plane every 30s for active key hashes; any control-plane-issued user key works against any of 27 regional adapters.
- **NATS mesh is now actually full-mesh, not hub-and-spoke.** Root cause of earlier R3 failures: leaves were only routed to LZ. Fixed by populating `NATS_ROUTES` on every node with all 27 peer URLs.
- **R3 / R5 placement-by-geo works.** 5 demo buckets created showing R1, R3-na, R3-eu, R3-ap, R5-global topology.
- **Topology UI live** at `/topology` on the user Spin app — geographic SVG with replica triangles, leader rings, live replication lag per peer.
- **FW posture intact** — `nats-kv-fw` strict (DROP default, only 443 + 6222 public, admin-only on 22/4222/8222, LKE infra on internal CIDRs).
- **27 HTTPS endpoints all healthy** with the wildcard cert.

## What's deployed

| Surface | URL | Status |
|---|---|---|
| User app | https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app | live (`/`, `/play`, `/topology`, `/dash`, `/claim/<token>`) |
| Admin app | https://947e30b4-0481-4a14-bb05-fb83580e8594.fwf.app | live (paste admin token, manage tenants/invites) |
| Control plane | https://cp.nats-kv.connected-cloud.io | live (admin API + claim flow + `/v1/internal/keys`) |
| GTM data plane | https://edge.nats-kv.connected-cloud.io | live (warming up) |
| Per-region adapters | https://`<region>`.nats-kv.connected-cloud.io | 27/27 live, valid wildcard cert |

Admin token: `06ddd5c4a74ae2858b2d4c30c9d9dac601e9d1e5e6ffdc0d79cd662d9c1d3511` (also in Vault `api/nats-kv/control`).

## Demo buckets pre-seeded for you to look at

| Bucket | Replicas | Placement | Used by |
|---|---|---|---|
| `demo` | R1 | random (currently us-mia) | shared playground |
| `demo-r1-us-ord` | R1 | us-central | topology demo |
| `demo-r3-na` | R3 | ca-central + us-sea + us-west | topology demo |
| `demo-r3-eu` | R3 | fr-par-2 + de-fra-2 + eu-central | topology demo |
| `demo-r3-ap` | R3 | jp-osa + ap-west + in-maa | topology demo |
| `demo-r5-global` | R5 | br-gru + ap-northeast + id-cgk + it-mil + us-east | topology demo |

The `demo` bucket has seed data: `greeting`, `counter`, and `users.<alice|bob|carol|dave|eve>.<session|profile>` for the subject-wildcard demo.

## Critical structural finding

The 27-node "mesh" was actually **hub-and-spoke** — each leaf had 4 routes, all to LZ. Gossip wasn't dialing leaf-to-leaf despite cluster_advertise being set. R3 streams across regions therefore couldn't form quorum.

**Fix:** explicit `NATS_ROUTES` env on every node listing all 27 peer URLs. Each leaf now has ~104 routes (full N×N). JS meta cluster size 27, leader rotates normally. R3 placement with `geo:` tags works deterministically.

This deserves an ADR — adding now (ADR-016).

## What I did NOT touch

- `mortgage-inference` GPU node (`lke589020-869396-…` in latency-fra2) — left alone per your call
- The orphan R3 streams from earlier attempts (`KV_kv-admin-tenants`, `KV_kv-admin-keys`) — JetStream meta still has them in some intermediate state, but they don't block anything; the v2 buckets coexist
- Akamai property in front (raw fwf.app URLs)
- DS2 stream wiring
- Demo-puller robot creds (still need rotation per your acknowledgment earlier)

## Open items / known-not-quite-right

- **Mirror streams not yet created.** R3 buckets have replicas in 3 regions; reads from outside those 3 regions hit JS forwarding (cross-cluster), not a local mirror. To get truly local reads everywhere, would need to add `Mirror` streams in every other peer per bucket. This is a per-bucket policy decision and a chunk of work — leaving as v0.6.
- **Per-tenant bucket creation API** not yet exposed. Today only admins can create buckets via `nats kv add` CLI. Users on the dashboard see existing buckets but can't create their own. Easy follow-up.
- **Latency-driven placement engine (#4)** — placement currently uses static `geo:` tags. The proper version reads pairwise RTT from project-latency's hub and auto-picks the best triple. Deferred.
- **GTM cold-start latency** — first ~5 calls from fwf are 100-200ms; warm steady-state is 50-70ms server-side. GTM pop selection still warming, per your "give it bake time" note. Will re-baseline tomorrow.

## Code changes shipped overnight

```
adapter v0.1.6 → v0.1.7
  + NATS server tags: region:<id> + geo:<g>
  + /v1/admin/buckets returns full topology (replicas, leader, peer list, lag, placement_tags)

control v0.1.5 (no change tonight, the adapter polling change of v0.1.5 stays)

user spin app
  + /topology page (SVG world map + bucket polygons + leader rings + lag table)
  + nav now includes "topology"
  + proxy fix: query string preserved (was dropping ?match= for subject wildcards)

infra
  + NATS_ROUTES on every node (LZ deployment + 26 leaf DaemonSets) listing all 27 peer URLs
```

Git log:
```
7ae255a fix(ui): proxy preserves query string for /api/nats and /api/control
be228cd feat(ui): /topology page + 5 demo buckets (R1/R3/R5 across geos)
da66872 feat(adapter v0.1.7): geo tags on NATS server + per-bucket topology in /v1/admin/buckets
fcaa907 feat: HTTP-poll keys from control plane (drops dependency on cross-cluster KV watch); v0.1.6 adapter, v0.1.5 control
… etc
```

## Ready to look at when you wake up

1. **Open https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app/topology** — should render 5 demo buckets as triangles/polygons over a world map
2. **Open https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app/play** — playground works without sign-in (uses shared demo token)
3. **Open https://947e30b4-0481-4a14-bb05-fb83580e8594.fwf.app**, paste admin token — issue an invite, claim it via the URL, sign in to the user app with the resulting key, hit the playground again — everything should now work end-to-end with your tenant key (the original 401 bug is dead)

Anything broken or surprising — I'll investigate when next session starts.
