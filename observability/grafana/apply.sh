#!/usr/bin/env bash
# observability/grafana/apply.sh — POSTs the nats-kv dashboard to the LZ Grafana
# and extends the ds2-cdn-analytics `demo` variable mapping to recognise the
# nats-kv property's CP code.
#
# Run from the repo root after `terraform apply` in
# ~/project-landing-zone/presales-landing-zone/infra/terraform/demos/nats-kv/
# (need the CP code from `terraform output cp_code_id`).
#
# Requires:
#   - kubectl context for the LZ cluster
#   - LZ kubeconfig at $KUBECONFIG (we port-forward from there)
#   - python3, jq
#
# Usage:
#   ./observability/grafana/apply.sh                # just upload nats-kv dashboard
#   ./observability/grafana/apply.sh --cp 1234567   # also patch demo variable
set -euo pipefail

CP_CODE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cp) CP_CODE="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done

ADMIN_USER="${GRAFANA_ADMIN_USER:-admin}"
ADMIN_PASS="${GRAFANA_ADMIN_PASS:-changeme-please}"
PORT="${GRAFANA_LOCAL_PORT:-13000}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "==> port-forwarding LZ grafana to localhost:$PORT"
kubectl port-forward -n central-services svc/grafana "$PORT:3000" >/tmp/gf-pf.log 2>&1 &
PF=$!
trap "kill $PF 2>/dev/null || true" EXIT
sleep 3

base="http://localhost:$PORT"
auth=(-u "$ADMIN_USER:$ADMIN_PASS")

echo "==> POST nats-kv dashboard"
python3 -c "
import json, sys
with open('$SCRIPT_DIR/nats-kv-dashboard.json') as f: d = json.load(f)
print(json.dumps({'dashboard': d, 'overwrite': True, 'message': 'apply.sh upload'}))
" > /tmp/_dash.json
curl -s "${auth[@]}" -X POST -H "Content-Type: application/json" --data @/tmp/_dash.json \
  "$base/api/dashboards/db" | python3 -m json.tool | head -10

if [[ -n "$CP_CODE" ]]; then
  echo "==> patching demo variable in ds2-cdn-analytics with cp=$CP_CODE → 'nats-kv'"
  python3 <<PY
import json, urllib.request, base64
auth = base64.b64encode(b"$ADMIN_USER:$ADMIN_PASS").decode()
req = urllib.request.Request("$base/api/dashboards/uid/ds2-cdn-analytics", headers={"Authorization":"Basic "+auth})
with urllib.request.urlopen(req) as r:
    d = json.load(r)
dash = d["dashboard"]
for v in dash.get("templating",{}).get("list",[]):
    if v.get("name") == "demo":
        q = v.get("query","")
        new_pair = "cp=$CP_CODE,'nats-kv'"
        if new_pair in q:
            print("  already present; nothing to do")
        else:
            # insert right before the trailing toString(cp))
            q2 = q.replace(", toString(cp))", f", {new_pair}, toString(cp))")
            v["query"] = q2
            v["definition"] = q2
            print("  updated query")
        break
payload = {"dashboard": dash, "overwrite": True, "message": "apply.sh: add nats-kv CP $CP_CODE"}
data = json.dumps(payload).encode()
req2 = urllib.request.Request("$base/api/dashboards/db", data=data,
    headers={"Authorization":"Basic "+auth, "Content-Type":"application/json"}, method="POST")
with urllib.request.urlopen(req2) as r:
    print(r.read().decode())
PY
else
  echo "==> skipping demo-variable patch (no --cp passed; pass after \`terraform output cp_code_id\`)"
fi

echo "==> done."
