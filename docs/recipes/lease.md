# Recipe: Lease (Time-Bounded Resource Reservation)

A lease is "I get this resource for the next N seconds, after which it's free again." Same primitives as a lock, but the holder isn't expected to renew — the work fits inside the lease window.

## Pattern

```javascript
const TOKEN = "akv_demo_open";
const URL = "http://us-ord.nats-kv.connected-cloud.io:8080";

async function acquireLease(name, holderId, ttlSeconds) {
  const r = await fetch(`${URL}/v1/kv/leases/${name}?ttl=${ttlSeconds}s`, {
    method: "PUT",
    headers: {
      "Authorization": `Bearer ${TOKEN}`,
      "If-None-Match": "*",
    },
    body: holderId,
  });
  if (r.status === 200) {
    return { acquired: true, expiresAt: Date.now() + ttlSeconds * 1000 };
  }
  return { acquired: false };
}

async function renewLeaseEarly(name, holderId, revision, ttlSeconds) {
  const r = await fetch(`${URL}/v1/kv/leases/${name}?ttl=${ttlSeconds}s`, {
    method: "PUT",
    headers: {
      "Authorization": `Bearer ${TOKEN}`,
      "If-Match": revision,
    },
    body: holderId,
  });
  return r.ok;
}
```

## Use cases

- **Job scheduling**: worker leases a job for the time it expects to run; if it crashes, another worker picks up after expiry.
- **Connection draining**: lease "I'm draining region X for 5 minutes" — load balancer respects the lease.
- **Maintenance window**: lease a resource for the duration of a deploy, blocking other operations.
- **Deduplication of triggers**: webhook receiver leases the trigger ID for an hour to suppress duplicate fires.
