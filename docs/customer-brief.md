# NATS-KV for Akamai Functions — customer brief

**Status**: research POC, demo-grade. Not production. Built to inform a real product decision: should Akamai Functions expose a richer KV WIT to function authors, or is `spin:key-value`'s exact-key get/set/delete enough?

---

## What it is, in one sentence

A globally distributed NATS JetStream KV store fronted by an HTTP adapter on every Akamai region, exposing primitives that the existing Spin KV WIT can't — atomic increment, revision history, CAS, subject-pattern wildcards, geographic placement control, and per-region read mirrors — for any function (or any HTTP client) to use today.

---

## Why customers care

The current Spin KV WIT (Cosmos backend on Akamai Functions) covers the simple case well: get/set/delete by exact key, in-process call shape. The moment a function author wants something more — a counter that's safe under concurrent writes, a "list all sessions for this user" query, a CAS-based distributed lock, or a write that should land in EU and be readable from APAC sub-100ms — they have to leave Akamai and call something else. NATS-KV closes that gap.

The headline numbers (measured against shared-tenant Cosmos via Spin's WIT, FWF egress in Chicago):

| Capability | Cosmos WIT KV | NATS-KV (this) |
|---|---|---|
| atomic increment | ✗ (read-modify-write race) | ✓ — atomic, CAS-loop server-side |
| revision history per key | ✗ | ✓ — last 8 versions queryable |
| compare-and-swap | ✗ | ✓ — `If-Match` header |
| subject-pattern wildcard query | ✗ — exact key only | ✓ — `users.*.session` works natively |
| live watch / SSE | ✗ | ✓ — browser EventSource, server clients |
| geographic placement control | ✗ — managed | ✓ — R1/R3/R5 with geo:na/eu/ap tags |
| per-region read locality | ~ — managed POPs | ✓ — local mirror at every node |
| **read throughput limit** | **~1,000 reads/sec (FWF gate)** | **~15-30k reads/sec/region** (projected) |
| **write throughput limit** | **~100 writes/sec (FWF gate)** | **~5-15k writes/sec/R3 bucket** (projected) |
| read latency (FWF→Cosmos vs FWF→NATS local mirror) | 18-22ms | 3-9ms when GTM lands locally |
| call shape | in-process WIT (no network) | HTTP via wasi-http (one network hop today) |
| production support / SLA | ✓ Akamai-managed | ✗ demo-grade |

Two distinct stories worth pulling out:

1. **Throughput.** The Spin WIT KV is rate-limited at the platform level (~1k reads/sec, ~100 writes/sec). NATS-KV projections are 10-30× higher per region for reads and 50× higher per bucket for writes. For any function building a counter, a session store, a feature-flag store at scale — the WIT path runs out of room fast.
2. **Primitives.** Atomic counters, CAS, history, subject wildcards aren't just nice-to-haves. They're the primitives you build distributed locks, leader election, idempotency keys, and rate limiters on top of. Without them, function authors implement the same patterns badly with retry loops + eventual consistency assumptions.

---

## What it costs to look at it

- **Demo URL** (gate-protected, browser): brian shares an access link.
- **5 min to first impression**: the playground side-by-side benchmark runs Cosmos and NATS PUT/GET head-to-head live.
- **20 min to depth**: `/docs` has the full API + Cosmos comparison + Rust/TS/Python sample function code; `/loadtest` produces measured ops/sec; `/topology` shows the 27-region mesh; `/api-explorer` is interactive Swagger.
- **No Akamai sales engagement required to evaluate** — the demo is self-contained and runs on the presales account. If you want to put real workload on it, that's a deeper conversation.

---

## What we want to learn from showing it

Three questions that drove the build, that the demo data should help answer:

1. **Is the gap real?** Not "would atomic increment be nice" in the abstract — but: of the workloads function authors are bringing to Akamai today, what fraction would benefit materially from the richer primitives? Which primitives matter most? (Answer often: counters and CAS, sometimes wildcards, rarely history.)
2. **Is the latency cost of HTTP-out vs in-process WIT acceptable?** The demo shows a one-network-hop floor (~5-10ms intra-region, more if GTM mis-routes). If a function author is OK with that, the gap is closeable today via this HTTP path. If they're not, the path forward is a native `key-value-nats` Spin factor crate (deferred — see ADR-001).
3. **Is the operational cost worth it?** This demo is 27 nodes × 1 control plane + 2 Spin apps + 1 Akamai property + 1 GTM property + 1 Grafana dashboard. Real production would need on-call, backup/restore (no DR today; see ADR-022), and something better than a single-CPS-cert SAN. Customer feedback should inform whether that's worth building.

---

## What the demo isn't

- Not a managed offering.
- Not benchmarked at scale (projections in `/docs §9`, real measurements via `/loadtest` you run yourself).
- Not on EAA or any SSO — bearer tokens only.
- Not multi-tenant in the cost-attribution sense — every tenant pays the same cluster.
- Not in any way committed to ship as a product. This is a build to inform whether one should be built.

---

## Engagement path

If a customer wants to get hands-on:

1. **Brian sends an invite link** (gate token + tenant claim baked in).
2. They land on the dashboard with a fresh tenant, R3 bucket auto-created, ready to PUT/GET.
3. Recipe docs + sample Rust/TS/Python in `/docs` get them productive in an hour.
4. `/verify` proves correctness across the whole API surface (10 tests; should be 10/10 pass).
5. `/loadtest` lets them produce numbers from their own client perspective.

For deeper evaluation (custom buckets, multi-region testing, CAS-based locking pattern in their own code) — schedule a session with Brian.

---

## Source + design context

- **Source**: [github.com/ccie7599/nats-kv](https://github.com/ccie7599/nats-kv)
- **Architecture**: `SCOPE.md` (scope contract), `DECISIONS.md` (24 ADRs covering every architecture choice including the misses), `HANDOFF.md` (operator on-ramp).
- **Live demo**: ask Brian for the URL (gated).

Built April 2026 by Brian Apley (TSA, Akamai). Free to repurpose any of this for customer conversations or internal product discussions.
