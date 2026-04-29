# project-nats-kv

Globally distributed NATS JetStream KV for Akamai Functions / Fermyon Spin.

See `SCOPE.md` for the architecture, `DECISIONS.md` for ADRs.

## Status — v0.2.0 (LZ-native)

Single-region, but now properly deployed via the org's standard pattern: Harbor for the image, Vault for the token, Argo CD for sync, Prometheus annotations for OTel scrape.

| Component | URL | Backed by |
|---|---|---|
| Spin user app on Akamai Functions | https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app | FWF |
| Adapter (us-ord) | http://us-ord.nats-kv.connected-cloud.io | LKE NodeBalancer 172.237.141.164 |
| Image | `harbor.harbor.svc.cluster.local/presales/nats-kv-adapter:v0.1.0` | Harbor |
| Argo Application | `argocd/nats-kv` | LZ Argo CD |
| Vault secret | `api/nats-kv/config` (key `demo_token`) | LZ Vault |

## Quick test

```bash
TOKEN=akv_demo_open
URL=http://us-ord.nats-kv.connected-cloud.io

curl -s $URL/v1/health
curl -s -X PUT  -H "Authorization: Bearer $TOKEN" -d "hello" $URL/v1/kv/demo/greeting
curl -s         -H "Authorization: Bearer $TOKEN"            $URL/v1/kv/demo/greeting
curl -s -X POST -H "Authorization: Bearer $TOKEN"            $URL/v1/kv/demo/clicks/incr
curl -s         -H "Authorization: Bearer $TOKEN"            $URL/v1/kv/demo/clicks/history

for u in alice bob carol; do
  curl -s -X PUT -H "Authorization: Bearer $TOKEN" -d "session-of-$u" \
    $URL/v1/kv/demo/users.$u.session > /dev/null
done
curl -s -H "Authorization: Bearer $TOKEN" "$URL/v1/kv/demo/keys?match=users.*.session"
```

## Deployment topology (v0.2)

```
                          ┌────────────────────────────────────┐
                          │ Akamai Functions (Spin)            │
                          │   nats-kv-user                     │
                          │   https://3c5be533-…fwf.app        │
                          └────────────────┬───────────────────┘
                                           │  outbound HTTP (pooled)
                                           ▼
              ┌─────────────────────────────────────────────────┐
              │ us-ord.nats-kv.connected-cloud.io               │
              │   (Akamai Edge DNS A → NB 172.237.141.164)      │
              └────────────────┬────────────────────────────────┘
                                ▼
              ┌─────────────────────────────────────────────────┐
              │ LKE 575271 (presales-landing-zone, us-ord)      │
              │   ns: demo-nats-kv                              │
              │   Deployment kv-adapter (1 replica, PVC 50Gi)   │
              │   Image: harbor/presales/nats-kv-adapter:v0.1.0 │
              │   Vault Agent injects api/nats-kv/config        │
              │   OTel scraped via prometheus.io/scrape=true    │
              └─────────────────────────────────────────────────┘
```

## Build + deploy

Manifests in `k8s/` are managed by Argo CD. Push to `main`, Argo syncs within ~30s.

```bash
# 1. Build adapter binary + image
make build-linux
docker build -f deploy/docker/Dockerfile -t harbor.connected-cloud.io/presales/nats-kv-adapter:vX.Y.Z .
docker push harbor.connected-cloud.io/presales/nats-kv-adapter:vX.Y.Z

# 2. Update k8s/40-deployment.yaml image tag, commit, push — Argo applies it.

# 3. (One-time per cluster, before first Argo sync) clone harbor-creds into the namespace:
KUBECONFIG=~/.kube/presales-landing-zone scripts/bootstrap-harbor-creds.sh

# 4. (One-time) Apply the Argo Application:
kubectl apply -f argo/application.yaml

# Spin app:
cd ui/nats-kv-user && spin build && spin aka deploy --no-confirm
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

## Known gaps (deliberate v0.2 deferrals)

- **Single region** — multi-region cluster mesh + R3/R5 placement engine pending. Will fan out to the 26 latency LKE clusters next.
- **HTTP only** — TLS pending; cert-manager + Let's Encrypt with the Akamai DNS-01 solver per LZ standard.
- **NodeBalancer** instead of `hostNetwork`+`hostPort` — single replica scales to ~10k req/s on `g8-dedicated-4-2`, fine for POC. Hostnet pivot when we move to the per-region single-node leaf clusters.
- **NB firewall detached** — `demo-nb-fw` (3900446) does not include FWF Spin egress IPs in its allow-list, so FWF→adapter calls were rejected. NB is currently open. To re-attach: identify FWF egress CIDRs and add to the firewall rule, then `linode-cli firewalls device-create --id <NB_ID> --type nodebalancer 3900446`.
- **Shared demo token** — multi-tenancy via NATS Accounts + control plane is the next major piece.
- **No GTM** — single endpoint until multi-region.

## Repo layout

```
cmd/adapter/          Go HTTP+NATS adapter
internal/adapter/     Server/handlers
ui/nats-kv-user/      Spin app (Rust) for FWF
k8s/                  Manifests synced by Argo CD
argo/                 Argo Application CRDs (apply once per cluster)
deploy/docker/        Dockerfile
deploy/terraform/     Terraform module (legacy v0.1 bare-VM path; LKE manifests are the live path)
docs/recipes/         CAS+TTL pattern recipes (lock, lease, leader-election, idempotency, rate-limit)
scripts/              Bootstrap helpers (e.g. harbor-creds clone)
```
