#!/usr/bin/env bash
# Bootstrap the harbor-creds imagePullSecret in the demo-nats-kv namespace.
# Pull secrets MUST NOT be committed to git — INTAKE.md:
#   "imagePullSecret: harbor-creds in default namespace"
# This script clones default/harbor-creds into demo-nats-kv. Run once per cluster
# before Argo CD's first sync. Idempotent.
set -euo pipefail

NS="${NS:-demo-nats-kv}"

KUBECONFIG="${KUBECONFIG:-$HOME/.kube/presales-landing-zone}"
export KUBECONFIG

kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

kubectl get secret harbor-creds -n default -o yaml \
  | python3 -c "
import sys, yaml
d = yaml.safe_load(sys.stdin)
d['metadata'] = {'name': 'harbor-creds', 'namespace': '$NS'}
print(yaml.dump(d))
" \
  | kubectl apply -f -

echo "OK — harbor-creds present in $NS"
