# Recipe: Idempotency Key

Ensure a request with side effects executes at most once, even if the client retries.

## Pattern

```bash
TOKEN=akv_demo_open
URL=http://us-ord.nats-kv.connected-cloud.io:8080
IDEMPOTENCY_KEY=$(uuidgen)

RESP=$(curl -sw "\n%{http_code}" -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "If-None-Match: *" \
  -d "$(date -Iseconds): processed" \
  "$URL/v1/kv/idempotency/$IDEMPOTENCY_KEY")

CODE=$(echo "$RESP" | tail -1)
if [ "$CODE" = "200" ]; then
  # First time we've seen this key — do the work
  echo "first run, doing work"
  perform_side_effect
else
  # Already processed; safe to skip
  echo "already processed, skipping"
fi
```

## Why this works

Same primitive as the distributed lock: `If-None-Match: *` is atomic put-if-absent. The first request wins and proceeds; all retries see `412 Precondition Failed` and short-circuit.

## Tuning

- Store the *result* of the side effect as the value, so retries can read it back and return the same response.
- Bucket TTL (`max_age`) cleans up old idempotency records — set to your idempotency window (e.g., 24h).
- For high-cardinality keys (millions/day), use a dedicated bucket with `max_age=1d` and `history=1`.
