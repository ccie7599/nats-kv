# NATS-KV runbook

Operator playbook for the demo. Covers what breaks, how to spot it, how to fix it. Read with `HANDOFF.md` (single starting point) and `DECISIONS.md` (why things are the way they are).

---

## Quick health check (60 seconds)

```bash
# 1. Both Spin apps responding
curl -fsS https://nats-kv.connected-cloud.io/health | jq .         # or fwf-direct
curl -fsS https://nats-kv-admin.connected-cloud.io/health | jq .

# 2. Control plane responding
curl -fsS https://cp.nats-kv.connected-cloud.io/v1/health | jq .

# 3. Data plane responding (any of 27 leaves via GTM)
curl -fsS -H "Authorization: Bearer akv_demo_open" \
  https://edge.nats-kv.connected-cloud.io/v1/admin/cluster | jq .

# 4. Demo bucket reads from local mirror (means mirror layer is healthy)
curl -fsS -H "Authorization: Bearer akv_demo_open" -i \
  https://edge.nats-kv.connected-cloud.io/v1/kv/demo/health-probe | grep -i x-read-source

# 5. /verify on the user app — 10 functional tests, should be 10/10 pass
# (run from a browser at /verify with akv_demo_open in the bucket picker)
```

If any of those fail, the matching section below tells you what to do.

---

## Spin apps not responding

**Symptoms**: `/health` returns non-200, or browser hangs on `/`.

**Diagnose**:
- `spin aka app status` from inside `ui/nats-kv-user/` or `ui/nats-kv-admin/` — shows the app's invocation count, last deploy time. If invocations are 0 in 24h, suspect a deploy that broke the build.
- `spin aka app logs nats-kv-user --tail` — last 100 lines from FWF.

**Fix**:
- Check the source builds locally: `cd ui/nats-kv-user && cargo build --target wasm32-wasip1 --release`. If the build broke (panic, type error), fix it.
- Re-deploy: `spin aka deploy --no-confirm`.
- If the variable `ui_gate_token` was rotated and broke the gate page, restore: `spin aka variables set ui_gate_token=<good-value>`.

---

## Control plane not responding

**Symptoms**: `/v1/health` 5xx; admin app returns 401/500 even with valid bearer.

**Diagnose**:
```bash
KUBECONFIG=/tmp/lz-kubeconfig.yaml kubectl get pods -n demo-nats-kv -l app=kv-control
KUBECONFIG=/tmp/lz-kubeconfig.yaml kubectl logs -n demo-nats-kv deploy/kv-control -c control --tail=100
```

Common errors and what they mean:
- `nats: stream not found` (in poll loop) → admin KV buckets gone. **THIS IS THE WIPE — see "Recovery from total cluster wipe" below.**
- `placement matrix: ... context deadline exceeded` → can't reach the project-latency hub (`latency-demo.connected-cloud.io`). Probably a transient network issue; check `curl -sk https://latency-demo.connected-cloud.io/api/v1/matrix?auth=...` from outside the pod.
- `admin token required` 401 → you're sending the wrong bearer. The admin token rotates with the pod's Vault sidecar; refresh: `KUBECONFIG=/tmp/lz-kubeconfig.yaml kubectl exec -n demo-nats-kv deploy/kv-control -c control -- cat /vault/secrets/admin-token`.

**Fix routes**:
- Pod restart (preserves data): `kubectl rollout restart deploy/kv-control` — Argo will re-reconcile.
- Image roll-back: edit `k8s/70-control-plane.yaml`, push to git, Argo syncs. Don't do this without reading what changed since.

---

## Adapters (the 26 leaves + LZ) not serving

**Symptoms**: `/v1/admin/cluster` returns 5xx, or KV ops time out from playground.

**Diagnose**:
- `KUBECONFIG=/tmp/leaves/<cluster_id>.yaml kubectl get pods -n demo-nats-kv` for each leaf you suspect.
- Log tail: `kubectl logs -n demo-nats-kv ds/kv-adapter -c adapter --tail=100`. Look for `[ERR]` or panic.

**Common modes**:
- One leaf down → GTM stops routing to it within ~60s (failover_delay=0). Cluster keeps serving from other 26.
- Multiple leaves restarting → check Linode infra status and the cluster's NodeBalancer (`linode-cli nodebalancers list`).
- `[ERR] ... cluster name "nats-kv-mesh" does not match "latency-mesh"` → harmless noise; another NATS cluster (project-latency) on the same nodes is rejecting routing attempts. Ignore.

**Fan-out roll** (after a code change):
```bash
# All 26 leaves in parallel; LZ goes via Argo from k8s/40-deployment.yaml
for cfg in /tmp/leaves/*.yaml; do
  KUBECONFIG=$cfg kubectl set image -n demo-nats-kv \
    ds/kv-adapter adapter=harbor.connected-cloud.io/presales/nats-kv-adapter:vX.Y.Z &
done; wait
```

(Refresh `/tmp/leaves/*.yaml` from `linode-cli lke clusters-list` against the presales token if it's stale.)

---

## Recovery from total cluster wipe (the ADR-022 scenario)

**Symptoms**: `nats stream ls -a` returns "No Streams defined" or close to it. Control plane logs spam `nats: stream not found`. All tenants/buckets/admin data appear to have vanished. PVs show only `$SYS/_js_/_meta_` directories at ~80K.

**This is the ADR-022 scenario** — a 0-byte meta-RAFT snapshot propagated cluster-wide. Data is genuinely gone; PVs survived but on-disk stream blocks were wiped.

**Recovery**:
1. **Confirm the meta-RAFT recovered** (each peer has a `_meta_` log): `KUBECONFIG=/tmp/leaves/<id>.yaml kubectl exec -n demo-nats-kv ds/kv-adapter -- find /var/lib/nats/jetstream -maxdepth 5 -type d` — should show `$SYS/_js_/_meta_/msgs/1.blk`.
2. **Restart the control plane** so it re-creates the admin buckets:
   ```bash
   KUBECONFIG=/tmp/lz-kubeconfig.yaml kubectl rollout restart deploy/kv-control -n demo-nats-kv
   ```
   On boot, `internal/control/store.go::NewStore` does an `openOrCreate` on `kv-admin-tenants-v3` and `kv-admin-keys-v3` (R5/NA). Within 10s of pod-Ready you should see `nats stream info KV_kv-admin-tenants-v3` succeed.
3. **Re-issue any tenants you need** via the admin app's invite flow. There is no automatic restore — those buckets are gone. The wipe is a known consequence of demo-grade ops.
4. **Recreate the demo bucket** by hitting the playground from any region — `ensureBucket` flow (adapter v0.2.2+) calls control to create `topology_probe`-style auto-place.

**Prevent recurrence**: keep admin buckets at R5 (per ADR-022) and don't pin LZ-only data on R1. The wipe happened because R1 admin storage anchored canonical state to a single peer whose PV came up empty after a rollout. R5/NA now spreads canonical state across 5 peers; losing any 2 doesn't take the cluster down.

---

## Cert / SAN issues

**Symptoms**: HTTPS to `nats-kv.connected-cloud.io` or `nats-kv-admin.connected-cloud.io` returns SSL handshake error; `curl -v` shows cert with wrong CN or missing SAN.

**Diagnose**:
```bash
# What cert are we serving?
echo | openssl s_client -servername nats-kv.connected-cloud.io \
  -connect nats-kv.connected-cloud.io:443 2>/dev/null \
  | openssl x509 -noout -subject -ext subjectAltName | head -10

# What's the state of CPS enrollment 293468?
/tmp/akamai-venv/bin/python /tmp/akamai_cps.py show 293468
/tmp/akamai-venv/bin/python /tmp/akamai_cps.py changes 293468
```

**If a SAN is missing** (e.g., new hostname not in cert):
```bash
/tmp/akamai-venv/bin/python /tmp/akamai_cps.py add-sans 293468 newhost.connected-cloud.io
/tmp/akamai-venv/bin/python /tmp/akamai_cps.py ack-warnings 293468
# Wait for "awaiting-input" with "lets-encrypt-challenges" type, then:
# 1. GET the challenge values
# 2. Add TXT records to Akamai DNS via Edge DNS API
# 3. POST input/update/lets-encrypt-challenges-completed with {"acknowledgement":"acknowledge"}
```

The `/tmp/akamai_cps.py` helper has all these flows. Full procedure also in `HANDOFF.md` "Bringing up the Akamai property" section.

**If cert is in long deploy phase** (`status: deploy-cert-to-production`): wait. Akamai's edge propagation is 20-60 min. Don't re-trigger.

---

## Argo not syncing

**Symptoms**: pushed a change to `k8s/40-deployment.yaml` or `k8s/70-control-plane.yaml`, but the LZ pods don't roll.

**Force a sync**:
```bash
KUBECONFIG=/tmp/lz-kubeconfig.yaml kubectl annotate -n argocd application nats-kv \
  argocd.argoproj.io/refresh=hard --overwrite
```

Argo polls every 3 min on its own; the annotation triggers immediately.

**If Argo claims Synced but the pod still has the old image**: Argo is reconciling against `origin/main`, so make sure your push landed:
```bash
git log origin/main..HEAD --oneline   # should be empty if everything's pushed
```

ArgoCD only manages the LZ. Leaf adapters are imperatively rolled (`kubectl set image ds/kv-adapter ...`). Argo will NOT touch leaves; if you're updating the adapter image, do both: push to git AND fan out to leaves.

---

## GTM routing oddities

**Symptoms**: requests landing on a far leaf (e.g., FWF in CHI hits `kv-jp-tyo-3`), latency surge.

**Diagnose**:
```bash
# Where is GTM pointing me right now?
dig +short edge.nats-kv.connected-cloud.io
# Compare to actual served_by:
curl -sk -H "X-KV-Key: akv_demo_open" \
  https://3c5be533-...fwf.app/api/nats/v1/admin/cluster | python3 -c "
import sys, json, base64
d = json.load(sys.stdin)
inner = json.loads(base64.b64decode(d['body_b64']).decode())
print(f'served_by={inner[\"server\"]}')
"
```

**Why this happens**: GTM domain `connectedcloud5.akadns.net` doesn't have ECS enabled (request open), so GTM picks based on the recursive resolver's IP, not the actual client. Different resolvers map to different "best" datacenters; the FWF runtime's resolver sometimes maps to a non-CHI leaf.

**Workarounds until ECS is enabled**:
- Bump `weight` or `precedence` on us-ord traffic_target in `~/project-landing-zone/.../infra/terraform/gtm/main.tf` to bias toward us-ord (less elegant).
- Add an ASN map for AS63949 → us-ord in GTM (more invasive, but deterministic).
- Document the variability in user-facing perf tables (we already do — `/docs §8`).

**Long-term**: enable ECS on the GTM domain (ADR-024 follow-up). Backend request open.

---

## Akamai property — first activation

If you blew away state and need to re-init the Akamai property for nats-kv:

1. **Cert SANs first** — see "Cert / SAN issues" above. Both `nats-kv.connected-cloud.io` and `nats-kv-admin.connected-cloud.io` must be in enrollment 293468 before activation, or HTTPS will hard-fail.
2. **Two-phase TF apply** for DS2 (per `HANDOFF.md` "Bringing up the Akamai property").
3. **Patch Grafana** demo variable: `./observability/grafana/apply.sh --cp <cp_code_id>`.

If activation hangs in Akamai's pipeline, check `https://control.akamai.com/apps/luna-properties/` for the property's activation status — sometimes the CLI says "queued" while the UI shows "warning" requiring acknowledgement.

---

## Invite flow not working

**Symptoms**: visitor submits the gated form, but it doesn't appear in admin's "Pending requests".

**Diagnose**:
```bash
# Is /api/request-invite reaching the control plane?
curl -sk -X POST -H "Content-Type: application/json" \
  -d '{"name":"runbook-test","email":"r@e.com"}' \
  https://3c5be533-...fwf.app/api/request-invite

# Is the request in NATS KV?
KUBECONFIG=/tmp/lz-kubeconfig.yaml kubectl port-forward -n demo-nats-kv \
  svc/kv-adapter 14222:4222 > /dev/null 2>&1 &
~/bin/nats --server=nats://localhost:14222 kv ls kv-admin-invite-requests-v1
~/bin/nats --server=nats://localhost:14222 kv get kv-admin-invite-requests-v1 <id>
```

**Common modes**:
- 500 on submit → control plane down or admin-bucket write failing. See "Control plane not responding".
- `bucket not found` → control plane bootstrap didn't recreate `kv-admin-invite-requests-v1`. Restart control: `kubectl rollout restart deploy/kv-control -n demo-nats-kv`.
- Request stored but admin app shows nothing → admin gate token mismatch (`spin aka variables get admin_gate_token` vs what the admin pasted), or admin bearer expired.

---

## Token rotation

| Token | Rotate via |
|---|---|
| User-app UI gate (`ui_gate_token`) | `cd ui/nats-kv-user && spin aka variables set ui_gate_token=<new>` — takes effect immediately, old cookies invalid |
| Admin-app UI gate (`admin_gate_token`) | `cd ui/nats-kv-admin && spin aka variables set admin_gate_token=<new>` |
| Admin bearer (`admin_token`) | Update Vault secret `api/data/nats-kv/control` then restart control pod (Vault Agent re-injects on pod start) |
| Tenant KV keys | Admin app → Tenants → "Regen key" |
| Demo open token (`akv_demo_open`) | Hardcoded in source. Don't rotate without coordinating with the demo bucket — it's the only credential for the shared `demo` bucket. |

After any rotation, hard-refresh in the browser to drop stale cookies.

---

## Useful one-liners

```bash
# Stream count cluster-wide
KUBECONFIG=/tmp/lz-kubeconfig.yaml kubectl exec -n demo-nats-kv deploy/kv-adapter -- /bin/sh -c \
  'curl -s http://localhost:8222/jsz | python3 -c "import sys,json; print(json.load(sys.stdin)[\"streams\"])"'

# Force a stream's RAFT leader to a specific region
~/bin/nats --server=nats://localhost:14222 req '$JS.API.STREAM.LEADER.STEPDOWN.KV_demo' \
  '{"placement":{"tags":["region:us-ord"]}}'

# Drop all mirrors of a bucket (then a write triggers re-creation via control plane)
for sn in $(~/bin/nats --server=nats://localhost:14222 stream ls -a | \
           awk -F'│' '{print $2}' | tr -d ' ' | grep '^KV_demo_mirror_'); do
  ~/bin/nats --server=nats://localhost:14222 stream rm $sn --force
done

# Check which Akamai POP is serving FWF egress
curl -s https://3c5be533-...fwf.app/api/whereami | python3 -c "
import sys, json, base64
d = json.load(sys.stdin)
print(json.loads(base64.b64decode(d['body_b64']).decode()))"
```

---

## When to call for help

- Cluster wipe (multiple buckets gone, tenants gone) → see ADR-022; recovery is documented above but takes 30+ min.
- Cert validation stuck > 60 min in any non-deploy phase → file Akamai support ticket.
- Multiple leaf adapters down concurrently across regions (not isolated to one geo) → check Linode account status and presales-account quota; could be a billing or account-wide issue.
- ArgoCD claims Synced but pods don't reconcile after a fresh git push → check `kubectl get application -n argocd nats-kv -o yaml | grep -i error` for permission/auth errors.

For anything that needs Brian: `brian@apley.net`, or just open an issue on the repo.
