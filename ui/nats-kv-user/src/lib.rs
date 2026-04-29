use spin_sdk::http::{IntoResponse, Method, Request, Response};
use spin_sdk::http_component;
use spin_sdk::key_value::Store;

const ADAPTER_BASE: &str = "http://us-ord.nats-kv.connected-cloud.io";
const DEMO_TOKEN: &str = "akv_demo_open";

#[http_component]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    let path = req.path();

    if path == "/" || path == "/index.html" {
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "text/html; charset=utf-8")
            .body(INDEX_HTML)
            .build());
    }

    if path == "/health" {
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true,"app":"nats-kv-user","backends":["nats","cosmos"]}"#)
            .build());
    }

    // /api/nats/<path...>  — proxy to NATS adapter, return JSON wrapper with timings
    if let Some(rest) = path.strip_prefix("/api/nats/") {
        return Ok(call_nats(req.method().clone(), rest, req.body()).await?);
    }

    // /api/cosmos/<bucket>/<key> — Spin's managed KV (Cosmos backend on FWF)
    if let Some(rest) = path.strip_prefix("/api/cosmos/") {
        return Ok(call_cosmos(req.method().clone(), rest, req.body()).await?);
    }

    Ok(Response::builder().status(404).body("not found").build())
}

async fn call_nats(method: Method, path: &str, body: &[u8]) -> anyhow::Result<Response> {
    let url = format!("{ADAPTER_BASE}/{path}");
    let mut builder = Request::builder();
    builder.method(method);
    builder.uri(url);
    builder.header("Authorization", format!("Bearer {DEMO_TOKEN}"));
    if !body.is_empty() {
        builder.header("Content-Type", "application/octet-stream");
        builder.body(body.to_vec());
    } else {
        builder.body(Vec::<u8>::new());
    }
    let req = builder.build();
    let t0 = std::time::Instant::now();
    let upstream: Response = spin_sdk::http::send(req).await?;
    let elapsed_us = t0.elapsed().as_micros();

    let status = *upstream.status();
    let mut adapter_ms = String::new();
    let mut served_by = String::new();
    let mut revision = String::new();
    for (k, v) in upstream.headers() {
        let kl = k.to_lowercase();
        if let Some(s) = v.as_str() {
            match kl.as_str() {
                "x-latency-ms" => adapter_ms = s.to_string(),
                "x-served-by" => served_by = s.to_string(),
                "x-revision" => revision = s.to_string(),
                _ => {}
            }
        }
    }

    let body_b64 = b64encode(upstream.body());
    let payload = format!(
        r#"{{"backend":"nats","status":{status},"upstream_us":{elapsed_us},"adapter_ms":"{adapter_ms}","served_by":"{served_by}","revision":"{revision}","body_b64":"{body_b64}"}}"#,
    );
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(payload)
        .build())
}

async fn call_cosmos(method: Method, path: &str, body: &[u8]) -> anyhow::Result<Response> {
    // Spin's KV WIT is sync; treat /api/cosmos/<bucket>/<key> as "key only" (Spin bucket label is forced 'default' on FWF)
    // Path is just the key — bucket prefix ignored to keep client URL parity with NATS API.
    let key = path.split('/').filter(|p| !p.is_empty()).last().unwrap_or("");
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => return Ok(json_err(&format!("cosmos open: {e}"))),
    };

    let t0 = std::time::Instant::now();
    let result = match method {
        Method::Get => {
            store.get(key).map(|opt| (200, opt.unwrap_or_default()))
                .map_err(|e| e.to_string())
        }
        Method::Put => {
            store.set(key, body).map(|_| (200, Vec::new()))
                .map_err(|e| e.to_string())
        }
        Method::Delete => {
            store.delete(key).map(|_| (200, Vec::new()))
                .map_err(|e| e.to_string())
        }
        _ => Err(format!("method {method:?} not supported")),
    };
    let elapsed_us = t0.elapsed().as_micros();

    let (status, body_bytes, error) = match result {
        Ok((s, b)) => (s, b, String::new()),
        Err(e) => (500, Vec::new(), e),
    };

    let body_b64 = b64encode(&body_bytes);
    let payload = format!(
        r#"{{"backend":"cosmos","status":{status},"upstream_us":{elapsed_us},"body_b64":"{body_b64}","error":"{error}"}}"#,
    );
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(payload)
        .build())
}

fn json_err(msg: &str) -> Response {
    let payload = format!(r#"{{"backend":"cosmos","status":500,"upstream_us":0,"body_b64":"","error":"{}"}}"#, msg.replace('"', "'"));
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(payload)
        .build()
}

fn b64encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(((data.len() + 2) / 3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(TBL[((n >> 18) & 0x3f) as usize] as char);
        out.push(TBL[((n >> 12) & 0x3f) as usize] as char);
        out.push(TBL[((n >> 6) & 0x3f) as usize] as char);
        out.push(TBL[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(TBL[((n >> 18) & 0x3f) as usize] as char);
        out.push(TBL[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(TBL[((n >> 18) & 0x3f) as usize] as char);
        out.push(TBL[((n >> 12) & 0x3f) as usize] as char);
        out.push(TBL[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>NATS KV vs Cosmos — playground</title>
<style>
  :root { color-scheme: dark; --bg:#0d1117; --fg:#c9d1d9; --accent:#58a6ff; --nats:#3fb950; --cosmos:#d29922; --muted:#8b949e; --err:#f85149; }
  * { box-sizing: border-box; }
  body { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; background:var(--bg); color:var(--fg); margin:0; padding:24px; max-width:1200px; margin:0 auto; }
  h1 { color:var(--accent); margin:0 0 4px 0; }
  .sub { color:var(--muted); margin:0 0 24px 0; }
  fieldset { border:1px solid #30363d; border-radius:6px; padding:16px; margin:0 0 16px 0; }
  legend { color:var(--accent); padding:0 8px; }
  label { display:block; color:var(--muted); margin:6px 0 2px 0; font-size:12px; }
  input, textarea, select { width:100%; background:#161b22; color:var(--fg); border:1px solid #30363d; border-radius:4px; padding:8px; font-family:inherit; font-size:13px; }
  textarea { min-height:60px; resize:vertical; }
  .row { display:grid; grid-template-columns: 1fr 1fr 100px; gap:8px; align-items:end; }
  button { background:var(--accent); color:#0d1117; border:0; padding:8px 14px; border-radius:4px; font-family:inherit; font-size:13px; cursor:pointer; font-weight:600; }
  button:hover { filter:brightness(1.15); }
  button.secondary { background:#30363d; color:var(--fg); }
  button.nats { background:var(--nats); color:#0d1117; }
  button.cosmos { background:var(--cosmos); color:#0d1117; }
  .actions { display:flex; gap:8px; margin-top:10px; flex-wrap:wrap; }
  pre { background:#161b22; border:1px solid #30363d; border-radius:4px; padding:12px; max-height:340px; overflow:auto; font-size:12px; }
  .grid { display:grid; grid-template-columns: 1fr 1fr; gap:16px; }
  @media (max-width:780px) { .grid { grid-template-columns: 1fr; } }
  .bench { display:grid; grid-template-columns: 1fr 1fr; gap:12px; }
  .bench .col { padding:12px; border-radius:6px; background:#161b22; border:1px solid #30363d; }
  .bench h3 { margin:0 0 8px 0; font-size:14px; }
  .bench .nats h3 { color:var(--nats); }
  .bench .cosmos h3 { color:var(--cosmos); }
  .stat { display:flex; justify-content:space-between; padding:2px 0; font-size:12px; }
  .stat span:first-child { color:var(--muted); }
  .winner { color:var(--nats); font-weight:bold; }
  .loser { color:var(--cosmos); }
  .badge { display:inline-block; padding:2px 6px; border-radius:3px; font-size:11px; margin-right:4px; }
  .badge.nats { background:var(--nats); color:#0d1117; }
  .badge.cosmos { background:var(--cosmos); color:#0d1117; }
</style>
</head>
<body>
  <h1>NATS KV vs Cosmos KV — playground</h1>
  <p class="sub">Side-by-side benchmarks from inside this Spin function on Akamai Functions. Adapter: <code>us-ord.nats-kv.connected-cloud.io:8080</code></p>

  <fieldset>
    <legend>Single op (with server-side timing)</legend>
    <div class="row">
      <div><label>key</label><input id="key" value="bench-key"></div>
      <div><label>value</label><input id="value" value="hello"></div>
      <div><label>&nbsp;</label></div>
    </div>
    <div class="actions">
      <span class="badge nats">NATS</span>
      <button class="nats" onclick="op('nats','GET')">GET</button>
      <button class="nats" onclick="op('nats','PUT')">PUT</button>
      <button class="nats" onclick="op('nats','DELETE')">DELETE</button>
      <button class="nats" onclick="op('nats','INCR')">INCR</button>
      <span style="margin-left:18px"></span>
      <span class="badge cosmos">COSMOS</span>
      <button class="cosmos" onclick="op('cosmos','GET')">GET</button>
      <button class="cosmos" onclick="op('cosmos','PUT')">PUT</button>
      <button class="cosmos" onclick="op('cosmos','DELETE')">DELETE</button>
    </div>
    <div id="single-out" style="margin-top:12px;"></div>
  </fieldset>

  <fieldset>
    <legend>Side-by-side benchmark</legend>
    <div class="row">
      <div>
        <label>operation</label>
        <select id="bench-op">
          <option value="GET">GET</option>
          <option value="PUT">PUT</option>
        </select>
      </div>
      <div><label>iterations</label><input type="number" id="bench-n" value="50" min="1" max="500"></div>
      <div><label>&nbsp;</label><button onclick="runBench()">Run both</button></div>
    </div>
    <div class="bench" id="bench-out" style="margin-top:12px;">
      <div class="col nats"><h3>NATS</h3><div id="nats-stats">(idle)</div></div>
      <div class="col cosmos"><h3>Cosmos</h3><div id="cosmos-stats">(idle)</div></div>
    </div>
    <p class="muted" style="font-size:11px; color:var(--muted); margin-top:8px;">
      Both timed server-side inside the Spin function (microseconds → ms). Single-region NATS adapter (us-ord) — multi-region pending. <code>upstream_us</code> excludes browser↔FWF time.
    </p>
  </fieldset>

  <fieldset>
    <legend>NATS-unique demos (no Cosmos equivalent)</legend>
    <div class="actions">
      <button class="secondary" onclick="natsHistory()">History scrub (write 5 revisions, list)</button>
      <button class="secondary" onclick="natsSubject()">Subject wildcard (users.*.session)</button>
      <button class="secondary" onclick="natsCluster()">Cluster info</button>
    </div>
    <pre id="demo-out" style="margin-top:8px;">(awaiting demo)</pre>
  </fieldset>

<script>
const $ = (id) => document.getElementById(id);

async function op(backend, verb) {
  const key = $("key").value;
  const value = $("value").value;
  const path = backend === "nats" ? `nats/v1/kv/demo/${encodeURIComponent(key)}` : `cosmos/default/${encodeURIComponent(key)}`;
  let method = verb, body;
  if (verb === "INCR") {
    if (backend !== "nats") return alert("INCR is NATS-only");
    method = "POST";
    return fetchOp(`nats/v1/kv/demo/${encodeURIComponent(key)}/incr`, "POST", undefined, "single-out", "NATS INCR");
  }
  if (verb === "PUT") body = value;
  return fetchOp(path, method, body, "single-out", `${backend.toUpperCase()} ${verb}`);
}

async function fetchOp(path, method, body, outId, label) {
  const t0 = performance.now();
  const opts = { method };
  if (body !== undefined) opts.body = body;
  const r = await fetch("/api/" + path, opts);
  const browserMs = performance.now() - t0;
  const j = await r.json();
  const upstream_ms = (j.upstream_us / 1000).toFixed(2);
  let bodyText = "";
  try {
    bodyText = atob(j.body_b64 || "");
  } catch {}
  $(outId).innerHTML = `
    <div class="stat"><span>${label}</span><span>${j.backend === "nats" ? "🟢 NATS" : "🟡 COSMOS"} status=${j.status}</span></div>
    <div class="stat"><span>upstream (server-side, FWF→KV→FWF)</span><span><strong>${upstream_ms} ms</strong></span></div>
    <div class="stat"><span>browser→FWF→FWF→browser overhead</span><span>${(browserMs - parseFloat(upstream_ms)).toFixed(1)} ms</span></div>
    <div class="stat"><span>browser end-to-end</span><span>${browserMs.toFixed(1)} ms</span></div>
    ${j.served_by ? `<div class="stat"><span>served by</span><span>${j.served_by}</span></div>` : ""}
    ${j.revision ? `<div class="stat"><span>revision</span><span>${j.revision}</span></div>` : ""}
    ${j.adapter_ms ? `<div class="stat"><span>adapter internal</span><span>${j.adapter_ms} ms</span></div>` : ""}
    ${j.error ? `<div class="stat"><span>error</span><span style="color:var(--err)">${j.error}</span></div>` : ""}
    ${bodyText ? `<pre>${bodyText.replace(/</g,'&lt;')}</pre>` : ""}
  `;
}

async function runBench() {
  const verb = $("bench-op").value;
  const N = parseInt($("bench-n").value, 10);
  const key = "bench-key";
  const body = "bench-payload";

  $("nats-stats").textContent = "running...";
  $("cosmos-stats").textContent = "running...";

  const collect = async (backend) => {
    const upstream = [];
    const browser = [];
    const path = backend === "nats" ? `nats/v1/kv/demo/${key}` : `cosmos/default/${key}`;
    for (let i = 0; i < N; i++) {
      const t0 = performance.now();
      const opts = { method: verb };
      if (verb === "PUT") opts.body = body + "-" + i;
      const r = await fetch("/api/" + path, opts);
      const bms = performance.now() - t0;
      const j = await r.json();
      upstream.push(j.upstream_us / 1000);
      browser.push(bms);
    }
    return { upstream, browser };
  };

  const [nats, cosmos] = await Promise.all([collect("nats"), collect("cosmos")]);

  const stats = (arr) => {
    const sorted = [...arr].sort((a,b)=>a-b);
    const p = (q) => sorted[Math.floor((sorted.length-1)*q)];
    const mean = arr.reduce((a,b)=>a+b,0)/arr.length;
    return { min: sorted[0], p50: p(0.5), p90: p(0.9), p99: p(0.99), max: sorted[sorted.length-1], mean };
  };

  const renderCol = (s, browserStats) => `
    <div class="stat"><span>min</span><span>${s.min.toFixed(2)} ms</span></div>
    <div class="stat"><span>p50</span><span><strong>${s.p50.toFixed(2)} ms</strong></span></div>
    <div class="stat"><span>p90</span><span>${s.p90.toFixed(2)} ms</span></div>
    <div class="stat"><span>p99</span><span>${s.p99.toFixed(2)} ms</span></div>
    <div class="stat"><span>max</span><span>${s.max.toFixed(2)} ms</span></div>
    <div class="stat"><span>mean</span><span>${s.mean.toFixed(2)} ms</span></div>
    <div class="stat" style="margin-top:6px; border-top:1px solid #30363d; padding-top:6px;"><span>browser p50 (incl. browser↔FWF)</span><span>${browserStats.p50.toFixed(1)} ms</span></div>
  `;
  const ns = stats(nats.upstream);
  const cs = stats(cosmos.upstream);
  const nbrowser = stats(nats.browser);
  const cbrowser = stats(cosmos.browser);
  $("nats-stats").innerHTML = renderCol(ns, nbrowser);
  $("cosmos-stats").innerHTML = renderCol(cs, cbrowser);

  // Add winner badge to p50
  const winner = ns.p50 < cs.p50 ? "nats-stats" : "cosmos-stats";
  const ratio = ns.p50 < cs.p50 ? (cs.p50/ns.p50) : (ns.p50/cs.p50);
  $(winner).innerHTML = `<div style="color:#3fb950; font-weight:bold; margin-bottom:6px;">🏆 ${ratio.toFixed(1)}× faster on p50</div>` + $(winner).innerHTML;
}

async function natsHistory() {
  const key = "demo-key-" + Date.now();
  for (let i = 1; i <= 5; i++) {
    await fetch(`/api/nats/v1/kv/demo/${key}`, { method: "PUT", body: "version-" + i });
  }
  const r = await fetch(`/api/nats/v1/kv/demo/${key}/history`);
  const j = await r.json();
  $("demo-out").textContent = `Wrote 5 revisions of "${key}". History (server-side ${(j.upstream_us/1000).toFixed(1)} ms):\n` + atob(j.body_b64);
}

async function natsSubject() {
  const id = Date.now();
  const users = ["alice","bob","carol","dave"];
  for (const u of users) {
    await fetch(`/api/nats/v1/kv/demo/users.${u}.${id}.session`, { method: "PUT", body: u });
  }
  const r = await fetch(`/api/nats/v1/kv/demo/keys?match=users.*.${id}.session`);
  const j = await r.json();
  $("demo-out").textContent = `Wrote 4 keys with subject pattern users.<name>.${id}.session\nWildcard query users.*.${id}.session (server-side ${(j.upstream_us/1000).toFixed(1)} ms):\n` + atob(j.body_b64) + `\n\nCosmos has no equivalent — keys must be exact-match or full table scan.`;
}

async function natsCluster() {
  const r = await fetch(`/api/nats/v1/admin/cluster`);
  const j = await r.json();
  $("demo-out").textContent = atob(j.body_b64);
}
</script>
</body>
</html>
"##;
