use spin_sdk::http::{IntoResponse, Method, Request, Response};
use spin_sdk::http_component;

const ADAPTER_BASE: &str = "http://us-ord.nats-kv.connected-cloud.io:8080";
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

    if let Some(rest) = path.strip_prefix("/api/") {
        let method = req.method().clone();
        return Ok(proxy(method, rest, req.body()).await?);
    }

    if path == "/health" {
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true,"app":"nats-kv-user"}"#)
            .build());
    }

    Ok(Response::builder().status(404).body("not found").build())
}

async fn proxy(method: Method, path: &str, body: &[u8]) -> anyhow::Result<Response> {
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
    let upstream: Response = spin_sdk::http::send(req).await?;

    let mut resp = Response::builder();
    resp.status(*upstream.status());
    for (k, v) in upstream.headers() {
        let kl = k.to_lowercase();
        if kl.starts_with("x-") || kl == "content-type" {
            if let Some(s) = v.as_str() {
                resp.header(k.to_string(), s.to_string());
            }
        }
    }
    resp.header("Access-Control-Allow-Origin", "*");
    Ok(resp.body(upstream.body().to_vec()).build())
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>NATS KV — playground</title>
<style>
  :root { color-scheme: light dark; --bg:#0d1117; --fg:#c9d1d9; --accent:#58a6ff; --muted:#8b949e; --ok:#3fb950; --warn:#d29922; --err:#f85149; }
  * { box-sizing: border-box; }
  body { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; background:var(--bg); color:var(--fg); margin:0; padding:24px; max-width:1100px; margin:0 auto; }
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
  .actions { display:flex; gap:8px; margin-top:10px; flex-wrap:wrap; }
  pre { background:#161b22; border:1px solid #30363d; border-radius:4px; padding:12px; max-height:300px; overflow:auto; font-size:12px; }
  .meta { color:var(--muted); font-size:11px; }
  .ok { color:var(--ok); } .err { color:var(--err); } .warn { color:var(--warn); }
  .grid { display:grid; grid-template-columns: 1fr 1fr; gap:16px; }
  @media (max-width:780px) { .grid { grid-template-columns: 1fr; } }
  .badge { display:inline-block; background:#30363d; color:var(--fg); padding:2px 6px; border-radius:3px; font-size:11px; margin-right:4px; }
</style>
</head>
<body>
  <h1>NATS KV playground</h1>
  <p class="sub">Backed by JetStream + HTTP adapter at <code>us-ord.nats-kv.connected-cloud.io:8080</code> via Akamai Functions / Fermyon Spin. R1 single-node POC.</p>

  <div class="grid">
    <fieldset>
      <legend>Get / Put / Delete</legend>
      <div class="row">
        <div><label>bucket</label><input id="bucket" value="demo"></div>
        <div><label>key</label><input id="key" value="greeting"></div>
        <div><label>&nbsp;</label><button class="secondary" onclick="doGet()">GET</button></div>
      </div>
      <label>value (PUT body)</label>
      <textarea id="value">hello from spin</textarea>
      <div class="actions">
        <button onclick="doPut()">PUT</button>
        <button class="secondary" onclick="doDelete()">DELETE</button>
        <button class="secondary" onclick="doIncr()">INCR</button>
        <button class="secondary" onclick="doHistory()">HISTORY</button>
      </div>
    </fieldset>

    <fieldset>
      <legend>List keys (subject pattern)</legend>
      <label>bucket</label><input id="lsBucket" value="demo">
      <label>match (NATS subject — e.g. <code>users.*.session</code> or <code>&gt;</code>)</label>
      <input id="lsMatch" value="&gt;">
      <div class="actions">
        <button onclick="doList()">LIST</button>
      </div>
    </fieldset>

    <fieldset>
      <legend>Bulk benchmark</legend>
      <label>operations</label>
      <select id="bench">
        <option value="get">GET 100x</option>
        <option value="put">PUT 100x</option>
        <option value="incr">INCR 100x</option>
      </select>
      <div class="actions">
        <button onclick="doBench()">RUN</button>
      </div>
      <p class="meta">Measures end-to-end latency from this Spin function to NATS adapter.</p>
    </fieldset>

    <fieldset>
      <legend>Cluster info</legend>
      <div class="actions">
        <button class="secondary" onclick="doCluster()">/v1/admin/cluster</button>
        <button class="secondary" onclick="doBuckets()">/v1/admin/buckets</button>
        <button class="secondary" onclick="doHealth()">/v1/health</button>
      </div>
    </fieldset>
  </div>

  <fieldset>
    <legend>Result</legend>
    <div id="status" class="meta">idle</div>
    <pre id="out">(awaiting request)</pre>
  </fieldset>

<script>
const $ = (id) => document.getElementById(id);
const out = (status, body, headers) => {
  const meta = headers ? Array.from(headers.entries()).filter(([k]) => k.startsWith("x-") || k === "content-type").map(([k,v]) => `${k}: ${v}`).join("\n") : "";
  $("status").innerHTML = `<span class="${status >= 200 && status < 300 ? 'ok' : 'err'}">HTTP ${status}</span>` + (meta ? `<br><span class="meta">${meta.replace(/</g,'&lt;')}</span>` : "");
  $("out").textContent = body;
};
async function call(method, path, body) {
  const t0 = performance.now();
  const opts = { method };
  if (body !== undefined) opts.body = body;
  const r = await fetch("/api/" + path, opts);
  const t = (performance.now() - t0).toFixed(1);
  const text = await r.text();
  $("status").innerHTML = `<span class="${r.ok?'ok':'err'}">HTTP ${r.status}</span> <span class="meta">${t}ms</span>`;
  let pretty = text;
  try { pretty = JSON.stringify(JSON.parse(text), null, 2); } catch {}
  $("out").textContent = pretty;
  return { status: r.status, headers: r.headers, body: text, ms: parseFloat(t) };
}
const doGet = () => call("GET", `v1/kv/${$("bucket").value}/${encodeURIComponent($("key").value)}`);
const doPut = () => call("PUT", `v1/kv/${$("bucket").value}/${encodeURIComponent($("key").value)}`, $("value").value);
const doDelete = () => call("DELETE", `v1/kv/${$("bucket").value}/${encodeURIComponent($("key").value)}`);
const doIncr = () => call("POST", `v1/kv/${$("bucket").value}/${encodeURIComponent($("key").value)}/incr`);
const doHistory = () => call("GET", `v1/kv/${$("bucket").value}/${encodeURIComponent($("key").value)}/history`);
const doList = () => call("GET", `v1/kv/${$("lsBucket").value}/keys?match=${encodeURIComponent($("lsMatch").value)}`);
const doCluster = () => call("GET", "v1/admin/cluster");
const doBuckets = () => call("GET", "v1/admin/buckets");
const doHealth = () => call("GET", "v1/health");
async function doBench() {
  const op = $("bench").value;
  const N = 100;
  const bucket = $("bucket").value;
  const key = $("key").value;
  const samples = [];
  for (let i=0; i<N; i++) {
    const t0 = performance.now();
    if (op === "get") await fetch(`/api/v1/kv/${bucket}/${encodeURIComponent(key)}`);
    else if (op === "put") await fetch(`/api/v1/kv/${bucket}/${encodeURIComponent(key)}`, { method: "PUT", body: "bench-" + i });
    else if (op === "incr") await fetch(`/api/v1/kv/${bucket}/bench-counter/incr`, { method: "POST" });
    samples.push(performance.now() - t0);
  }
  samples.sort((a,b)=>a-b);
  const p = (q) => samples[Math.floor((samples.length-1) * q)].toFixed(1);
  $("status").innerHTML = `<span class="ok">${N} ${op} ops complete</span>`;
  $("out").textContent = `op:    ${op}\nN:     ${N}\nmin:   ${samples[0].toFixed(1)} ms\np50:   ${p(0.5)} ms\np90:   ${p(0.9)} ms\np99:   ${p(0.99)} ms\nmax:   ${samples[samples.length-1].toFixed(1)} ms\nmean:  ${(samples.reduce((a,b)=>a+b,0)/N).toFixed(1)} ms`;
}
</script>
</body>
</html>
"##;
