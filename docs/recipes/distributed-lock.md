# Recipe: Distributed Lock

Mutually exclusive access to a named resource across multiple workers, with automatic release if the holder dies.

Built from two primitives: **CAS on create** (`If-None-Match: *`) and **TTL** (auto-purge after a deadline).

## Pattern

```bash
TOKEN=akv_demo_open
URL=http://us-ord.nats-kv.connected-cloud.io:8080
LOCK_NAME=foo
WORKER_ID=$(hostname)-$$
TTL_SECONDS=10

# Acquire
RESP=$(curl -sw "\n%{http_code}" -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "If-None-Match: *" \
  -d "$WORKER_ID" \
  "$URL/v1/kv/locks/$LOCK_NAME?ttl=${TTL_SECONDS}s")

CODE=$(echo "$RESP" | tail -1)
if [ "$CODE" = "200" ]; then
  echo "lock acquired"
  # ... do exclusive work ...
  curl -s -X DELETE -H "Authorization: Bearer $TOKEN" "$URL/v1/kv/locks/$LOCK_NAME"
else
  echo "lock held by someone else (HTTP $CODE)"
fi
```

## Why this works

- `If-None-Match: *` translates to NATS KV `Create()` — atomic put-if-not-exists. Two workers racing both call `PUT`; exactly one gets `200`, the other gets `412 Precondition Failed`.
- TTL ensures that if the holder crashes before deleting, the lock auto-releases after the deadline. No coordinator, no zombie locks.
- Releasing with no `If-Match` is fire-and-forget. If you want strict "only the holder can release," use `If-Match: <revision-from-acquire>`.

## Gotchas

- **Renewal**: long-running work needs to refresh the TTL by re-PUTting before expiry. Pattern: a heartbeat goroutine that PUTs every `TTL/3`.
- **Clock skew**: TTL is enforced server-side; no client-clock dependency.
- **Fairness**: this is unfair (whoever calls first wins; no queue). For fair locks, layer a per-name request log and consume in order — but you probably don't need that for an internal POC.
- **TTL not yet wired in v0.1**: per-key TTL is a NATS 2.11 feature. Today, set bucket `max_age` instead, or run a sweeper. v0.2 will wire `?ttl=` properly.

## See also

- [leader-election.md](leader-election.md) — same pattern with a longer TTL and renewal
- [idempotency-key.md](idempotency-key.md) — write-once with no release
