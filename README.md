# project-nats-kv

Globally distributed NATS JetStream KV for Akamai Functions / Fermyon Spin.

See `SCOPE.md` for the full architecture, `DECISIONS.md` for ADRs.

## Status — v0.1.0 (initial MVP)

Single-region deployment. Validates the architecture end-to-end before fanning out.

| Component | URL | Status |
|---|---|---|
| Adapter (us-ord) | http://us-ord.nats-kv.connected-cloud.io:8080 | live |
| Adapter (us-ord, by IP) | http://172.234.25.197:8080 | live |
| Spin user app on FWF | https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app | live |

**Deviations from SCOPE.md** (deliberate v0.1 shortcuts):
- Single Linode VM in us-ord (`g8-dedicated-4-2`), not LKE node pool. Reason: needed a registry-free deploy path for first MVP. Re-architect as LKE node pool once container-registry access is sorted.
- HTTP only, no TLS. Will layer Caddy+Let's Encrypt or LKE Ingress in v0.2.
- R1 single-node. Multi-region cluster mesh + R3/R5 placement engine pending.
- Demo bearer token `akv_demo_open` is shared, no multi-tenancy yet.
- No GTM property yet (single endpoint).
- No Akamai Ion property — using raw fwf.app URL.

## Quick test

```bash
TOKEN=akv_demo_open
URL=http://us-ord.nats-kv.connected-cloud.io:8080

# Health
curl -s $URL/v1/health

# Put / Get
curl -s -X PUT -H "Authorization: Bearer $TOKEN" -d "hello" $URL/v1/kv/demo/greeting
curl -s    -H "Authorization: Bearer $TOKEN"            $URL/v1/kv/demo/greeting

# Atomic increment (CAS-loop server-side)
curl -s -X POST -H "Authorization: Bearer $TOKEN" $URL/v1/kv/demo/clicks/incr

# Versioned history
curl -s -H "Authorization: Bearer $TOKEN" $URL/v1/kv/demo/clicks/history

# Subject-pattern key listing (NATS-unique)
for u in alice bob carol; do
  curl -s -X PUT -H "Authorization: Bearer $TOKEN" -d "session-of-$u" \
    $URL/v1/kv/demo/users.$u.session > /dev/null
done
curl -s -H "Authorization: Bearer $TOKEN" "$URL/v1/kv/demo/keys?match=users.*.session"
```

## API surface (v0.1)

```
GET    /v1/health
GET    /v1/admin/cluster
GET    /v1/admin/buckets
GET    /v1/kv/:bucket/:key                 # ?revision=N
PUT    /v1/kv/:bucket/:key                 # If-Match: <rev> | If-None-Match: *
DELETE /v1/kv/:bucket/:key
GET    /v1/kv/:bucket/:key/history
POST   /v1/kv/:bucket/:key/incr            # ?delta=N
GET    /v1/kv/:bucket/keys                 # ?match=nats.subject.pattern
```

All KV endpoints require `Authorization: Bearer <token>`. Responses include observability headers: `X-Served-By`, `X-Cluster-Geo`, `X-Revision`, `X-Latency-Ms`.

## Build

```bash
# Adapter
go build -o bin/kv-adapter ./cmd/adapter

# Spin app
cd ui/nats-kv-user && spin build && spin aka deploy --no-confirm
```

## Local dev

```bash
REGION=local LISTEN_ADDR=:8080 NATS_CLIENT_PORT=14222 NATS_CLUSTER_PORT=16222 \
  NATS_MONITOR_PORT=18222 JETSTREAM_DIR=/tmp/jstest \
  ./bin/kv-adapter
```

## Roadmap

See `SCOPE.md` exit criteria. Next priorities:
1. Multi-region cluster mesh (NATS_ROUTES wiring, R3 default).
2. LKE node pool migration once container registry is sorted.
3. Placement engine with project-latency RTT data.
4. Multi-tenancy (NATS Accounts + API key issuance).
5. Admin Spin app with invite tokens.
6. GTM property fronting all 27 regions.
