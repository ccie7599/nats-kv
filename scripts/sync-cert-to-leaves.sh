#!/usr/bin/env bash
# Sync the wildcard TLS Secret from the LZ cluster (cert-manager-issued) to every
# leaf cluster. Run manually after first issuance, then on a 60-day cadence
# (or as a CronJob in v0.5).
set -euo pipefail

LZ_KUBE="${LZ_KUBE:-$HOME/.kube/presales-landing-zone}"
SECRET_NAME="${SECRET_NAME:-nats-kv-tls}"
NS="${NS:-demo-nats-kv}"

echo "==> fetch wildcard secret from LZ"
TMP=$(mktemp)
KUBECONFIG="$LZ_KUBE" kubectl get secret "$SECRET_NAME" -n "$NS" -o yaml \
  | python3 -c "
import sys, yaml
d = yaml.safe_load(sys.stdin)
d['metadata'] = {'name': '$SECRET_NAME', 'namespace': '$NS'}
print(yaml.dump(d))" > "$TMP"

ok=0; fail=0
for KUBE in $(ls "$HOME"/.kube/latency-* | grep -v ".yaml$"); do
  NAME=$(basename "$KUBE")
  if KUBECONFIG="$KUBE" kubectl create namespace "$NS" --dry-run=client -o yaml | KUBECONFIG="$KUBE" kubectl apply -f - >/dev/null 2>&1 \
     && KUBECONFIG="$KUBE" kubectl apply -f "$TMP" >/dev/null 2>&1; then
    echo "  $NAME: OK"
    ok=$((ok+1))
  else
    echo "  $NAME: FAIL"
    fail=$((fail+1))
  fi
done
rm -f "$TMP"
echo
echo "ok=$ok fail=$fail"
