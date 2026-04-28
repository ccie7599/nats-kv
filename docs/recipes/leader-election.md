# Recipe: Leader Election

Exactly one worker in a fleet acts as the leader; the others stand by and take over if it dies.

This is just a distributed lock with a long TTL and active renewal.

## Pattern

```python
import requests, time, threading, os, socket

TOKEN = "akv_demo_open"
URL = "http://us-ord.nats-kv.connected-cloud.io:8080"
WORKER_ID = f"{socket.gethostname()}-{os.getpid()}"
TTL = 30  # seconds

def try_acquire():
    r = requests.put(
        f"{URL}/v1/kv/leaders/cron-runner?ttl={TTL}s",
        headers={"Authorization": f"Bearer {TOKEN}", "If-None-Match": "*"},
        data=WORKER_ID,
    )
    return r.status_code == 200, r.headers.get("X-Revision")

def renew(revision):
    r = requests.put(
        f"{URL}/v1/kv/leaders/cron-runner?ttl={TTL}s",
        headers={"Authorization": f"Bearer {TOKEN}", "If-Match": revision},
        data=WORKER_ID,
    )
    return r.status_code == 200, r.headers.get("X-Revision")

def be_leader():
    revision = None
    while True:
        if revision is None:
            ok, revision = try_acquire()
            if not ok:
                time.sleep(TTL // 3)
                continue
            print(f"{WORKER_ID} is leader, revision={revision}")
        # Renew before TTL expires
        time.sleep(TTL // 3)
        ok, revision = renew(revision)
        if not ok:
            print("lost leadership")
            revision = None

threading.Thread(target=be_leader, daemon=True).start()
```

## Why this works

- `If-None-Match: *` acquires the leadership key atomically — exactly one worker wins.
- The leader renews via `If-Match: <revision>` — only the current holder can renew. If a stale holder tries to renew with an old revision, it gets `412` and steps down.
- If the leader crashes, the TTL expires and the next `try_acquire()` from any waiting worker succeeds.
- Set TTL to 3-5x your renewal interval to tolerate transient network failures.

## Gotchas

- **Split brain**: during a network partition, a follower might acquire the lock while the old leader still thinks it's in charge. The old leader's renewal will fail (revision mismatch) within one renewal cycle, but during that cycle two workers both think they're leader. Make leader work *idempotent*, or add a fencing token (the revision number) to all downstream writes.
- **TTL gap**: between key expiry and the next acquire attempt, no leader exists. Tune TTL and poll cadence accordingly.
