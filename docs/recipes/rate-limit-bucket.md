# Recipe: Rate Limit (Token Bucket)

Limit a caller to N operations per time window using atomic increment and bucket TTL.

## Pattern

```bash
TOKEN=akv_demo_open
URL=http://us-ord.nats-kv.connected-cloud.io:8080
CALLER=user-123
LIMIT=100              # 100 requests
WINDOW=60              # per 60 seconds

# Bucket key includes the time window so the counter resets automatically
WINDOW_START=$(( $(date +%s) / WINDOW * WINDOW ))
KEY="${CALLER}.${WINDOW_START}"

COUNT=$(curl -s -X POST \
  -H "Authorization: Bearer $TOKEN" \
  "$URL/v1/kv/ratelimit/$KEY/incr" | jq -r .value)

if [ "$COUNT" -le "$LIMIT" ]; then
  echo "allowed (used $COUNT / $LIMIT)"
  do_work
else
  echo "rate limit exceeded ($COUNT > $LIMIT)"
fi
```

## Why this works

- Each window has a unique key: `user-123.1714338000`. As soon as time crosses the window boundary, a new key starts at zero.
- `incr` is server-side atomic — no read-modify-write race between concurrent callers.
- Old window keys are cleaned up by the bucket's `max_age` setting; set it to `2 * WINDOW` to ensure cleanup without disrupting in-flight windows.

## Tuning

- For sliding windows, use multiple finer-grained buckets and sum.
- For burst handling, layer two limits: `100/min` and `5000/hour` with two keys.
- For per-tenant fairness, prefix the key with the tenant ID and let each tenant have its own counter.

## Compared to Cosmos

Cosmos's "atomic" patch operations cost RU per call and throttle. Our `incr` is a tight CAS loop server-side — sub-millisecond per call, no throttling, no per-op billing.
