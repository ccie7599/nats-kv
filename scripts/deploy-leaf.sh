#!/usr/bin/env bash
# Deploy the kv-adapter DaemonSet to a leaf LKE cluster.
#
# Usage: deploy-leaf.sh <region-short> <linode-cluster-id>
# Example: deploy-leaf.sh fra 588993
#
# Requires: LINODE_TOKEN env var, kubectl, akamai dns CLI.
set -euo pipefail

REGION_SHORT="${1:?usage: deploy-leaf.sh <short> <cluster-id>}"
CLUSTER_ID="${2:?cluster-id required}"
LINODE_TOKEN="${LINODE_TOKEN:?LINODE_TOKEN env var required}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KUBE="$HOME/.kube/latency-${REGION_SHORT}"
DEMO_TOKEN="${DEMO_TOKEN:-akv_demo_open}"

echo "==> [$REGION_SHORT] fetch kubeconfig (cluster $CLUSTER_ID)"
curl -sf -H "Authorization: Bearer $LINODE_TOKEN" \
  "https://api.linode.com/v4/lke/clusters/$CLUSTER_ID/kubeconfig" \
  | python3 -c "import sys,json,base64; print(base64.b64decode(json.load(sys.stdin)['kubeconfig']).decode())" \
  > "$KUBE"
chmod 600 "$KUBE"

export KUBECONFIG="$KUBE"

echo "==> [$REGION_SHORT] node info"
NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="ExternalIP")].address}')
LINODE_REGION=$(kubectl get nodes -o jsonpath='{.items[0].metadata.labels.failure-domain\.beta\.kubernetes\.io/region}')
echo "    node IP: $NODE_IP   region: $LINODE_REGION"

echo "==> [$REGION_SHORT] apply namespace + harbor pull secret"
kubectl apply -f "$REPO_ROOT/k8s-leaf/00-namespace.yaml" >/dev/null
# Inline harbor-creds — use the demo-puller robot creds (read-only on Harbor).
kubectl create secret docker-registry harbor-creds \
  --namespace=demo-nats-kv \
  --docker-server=harbor.connected-cloud.io \
  --docker-username='robot$presales+demo-puller' \
  --docker-password="${HARBOR_DEMO_PULLER_PASSWORD:?HARBOR_DEMO_PULLER_PASSWORD env var required}" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null

echo "==> [$REGION_SHORT] apply config secret"
sed -e "s/REGION/$LINODE_REGION/g" -e "s/TOKEN/$DEMO_TOKEN/g" \
  "$REPO_ROOT/k8s-leaf/10-secret.yaml.tmpl" | kubectl apply -f - >/dev/null

echo "==> [$REGION_SHORT] apply daemonset"
kubectl apply -f "$REPO_ROOT/k8s-leaf/20-daemonset.yaml" >/dev/null

echo "==> [$REGION_SHORT] wait for pod ready"
for i in $(seq 1 30); do
  READY=$(kubectl get ds kv-adapter -n demo-nats-kv -o jsonpath='{.status.numberReady}' 2>/dev/null || echo 0)
  DESIRED=$(kubectl get ds kv-adapter -n demo-nats-kv -o jsonpath='{.status.desiredNumberScheduled}' 2>/dev/null || echo 0)
  echo "    [$i] ready=$READY/$DESIRED"
  [ "$READY" = "$DESIRED" ] && [ "$READY" -gt 0 ] && break
  sleep 5
done

echo "==> [$REGION_SHORT] DNS A record ${REGION_SHORT}.nats-kv.connected-cloud.io -> $NODE_IP"
akamai dns rm-record A connected-cloud.io --name "${LINODE_REGION}.nats-kv.connected-cloud.io" 2>/dev/null || true
akamai dns add-record A connected-cloud.io \
  --name "${LINODE_REGION}.nats-kv.connected-cloud.io" \
  --rdata "$NODE_IP" --ttl 60 --suppress 2>&1 | tail -1

echo "==> [$REGION_SHORT] smoke test http://$NODE_IP:8080/v1/health"
sleep 3
curl -sf "http://$NODE_IP:8080/v1/health" || { echo "FAIL"; exit 1; }
echo

echo "==> [$REGION_SHORT] DONE"
