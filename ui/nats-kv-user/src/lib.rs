use spin_sdk::http::{IntoResponse, Method, Request, Response};
use spin_sdk::http_component;
use spin_sdk::key_value::Store;

const ADAPTER_BASE: &str = "https://edge.nats-kv.connected-cloud.io";
const CONTROL_BASE: &str = "https://cp.nats-kv.connected-cloud.io";
const FALLBACK_TOKEN: &str = "akv_demo_open"; // used by playground if user not signed in

#[http_component]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    let path = req.path();

    // Static pages
    if path == "/" || path == "/index.html" {
        return html(INDEX_HTML);
    }
    if path == "/play" || path == "/play/" {
        return html(PLAY_HTML);
    }
    if path == "/dash" || path == "/dash/" {
        return html(DASH_HTML);
    }
    if path == "/topology" || path == "/topology/" {
        return html(TOPOLOGY_HTML);
    }
    if path.starts_with("/claim/") {
        return html(CLAIM_HTML);
    }
    if path == "/health" {
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true,"app":"nats-kv-user","backends":["nats","cosmos"]}"#)
            .build());
    }

    // /api/nats/<path...>  — proxy to NATS adapter using caller's bearer key (or fallback)
    if let Some(rest) = path.strip_prefix("/api/nats/") {
        let key = caller_bearer(&req).unwrap_or_else(|| FALLBACK_TOKEN.to_string());
        return Ok(call_nats(req.method().clone(), rest, req.body(), &key).await?);
    }

    // /api/cosmos/<bucket>/<key> — Spin's managed KV (Cosmos backend on FWF)
    if let Some(rest) = path.strip_prefix("/api/cosmos/") {
        return Ok(call_cosmos(req.method().clone(), rest, req.body()).await?);
    }

    // /api/control/<path...> — proxy to control plane (caller's bearer forwarded)
    if let Some(rest) = path.strip_prefix("/api/control/") {
        let bearer = req.header("authorization").and_then(|v| v.as_str()).unwrap_or("").to_string();
        return Ok(call_control(req.method().clone(), rest, req.body(), &bearer).await?);
    }

    Ok(Response::builder().status(404).body("not found").build())
}

fn html(body: &'static str) -> anyhow::Result<Response> {
    let rendered = body
        .replace("__SHARED_CSS__", SHARED_CSS)
        .replace("__NAV_JS__", NAV_JS);
    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/html; charset=utf-8")
        .body(rendered)
        .build())
}

fn caller_bearer(req: &Request) -> Option<String> {
    req.header("x-kv-key").and_then(|v| v.as_str()).map(|s| s.to_string())
}

async fn call_nats(method: Method, path: &str, body: &[u8], token: &str) -> anyhow::Result<Response> {
    let url = format!("{ADAPTER_BASE}/{path}");
    let mut builder = Request::builder();
    builder.method(method);
    builder.uri(url);
    builder.header("Authorization", format!("Bearer {token}"));
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
        if let Some(s) = v.as_str() {
            match k.to_lowercase().as_str() {
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
    let key = path.split('/').filter(|p| !p.is_empty()).last().unwrap_or("");
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => return Ok(json_err(&format!("cosmos open: {e}"))),
    };
    let t0 = std::time::Instant::now();
    let result = match method {
        Method::Get => store.get(key).map(|opt| (200, opt.unwrap_or_default())).map_err(|e| e.to_string()),
        Method::Put => store.set(key, body).map(|_| (200, Vec::new())).map_err(|e| e.to_string()),
        Method::Delete => store.delete(key).map(|_| (200, Vec::new())).map_err(|e| e.to_string()),
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
    Ok(Response::builder().status(200).header("content-type", "application/json").body(payload).build())
}

async fn call_control(method: Method, path: &str, body: &[u8], bearer: &str) -> anyhow::Result<Response> {
    let url = format!("{CONTROL_BASE}/{path}");
    let mut b = Request::builder();
    b.method(method);
    b.uri(url);
    if !bearer.is_empty() {
        b.header("Authorization", bearer.to_string());
    }
    if !body.is_empty() {
        b.header("Content-Type", "application/json");
        b.body(body.to_vec());
    } else {
        b.body(Vec::<u8>::new());
    }
    let upstream: Response = spin_sdk::http::send(b.build()).await?;
    let mut resp = Response::builder();
    resp.status(*upstream.status());
    for (k, v) in upstream.headers() {
        if k.to_lowercase() == "content-type" {
            if let Some(s) = v.as_str() {
                resp.header(k.to_string(), s.to_string());
            }
        }
    }
    Ok(resp.body(upstream.body().to_vec()).build())
}

fn json_err(msg: &str) -> Response {
    let payload = format!(r#"{{"backend":"cosmos","status":500,"upstream_us":0,"body_b64":"","error":"{}"}}"#, msg.replace('"', "'"));
    Response::builder().status(200).header("content-type", "application/json").body(payload).build()
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

const SHARED_CSS: &str = r##"
:root { color-scheme: dark; --bg:#0d1117; --fg:#c9d1d9; --accent:#58a6ff; --nats:#3fb950; --cosmos:#d29922; --muted:#8b949e; --err:#f85149; }
* { box-sizing: border-box; }
body { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; background:var(--bg); color:var(--fg); margin:0; padding:24px; max-width:1200px; margin:0 auto; }
h1 { color:var(--accent); margin:0 0 4px 0; }
h2 { color:var(--accent); margin:24px 0 8px 0; font-size:18px; }
.sub { color:var(--muted); margin:0 0 16px 0; }
nav { display:flex; gap:16px; padding:12px 0; border-bottom:1px solid #30363d; margin-bottom:16px; align-items:center; }
nav a { color:var(--accent); text-decoration:none; font-weight:600; }
nav a:hover { text-decoration:underline; }
nav .key-status { margin-left:auto; font-size:11px; color:var(--muted); }
nav .key-status.ok { color:var(--nats); }
fieldset { border:1px solid #30363d; border-radius:6px; padding:16px; margin:0 0 16px 0; }
legend { color:var(--accent); padding:0 8px; }
label { display:block; color:var(--muted); margin:6px 0 2px 0; font-size:12px; }
input, textarea, select { width:100%; background:#161b22; color:var(--fg); border:1px solid #30363d; border-radius:4px; padding:8px; font-family:inherit; font-size:13px; }
textarea { min-height:60px; resize:vertical; }
button { background:var(--accent); color:#0d1117; border:0; padding:8px 14px; border-radius:4px; font-family:inherit; font-size:13px; cursor:pointer; font-weight:600; }
button:hover { filter:brightness(1.15); }
button.secondary { background:#30363d; color:var(--fg); }
button.nats { background:var(--nats); color:#0d1117; }
button.cosmos { background:var(--cosmos); color:#0d1117; }
button.warn { background:var(--cosmos); color:#0d1117; }
button.danger { background:#a40e26; color:#fff; }
.row { display:grid; grid-template-columns: 1fr 1fr 100px; gap:8px; align-items:end; }
.actions { display:flex; gap:8px; margin-top:10px; flex-wrap:wrap; }
pre { background:#161b22; border:1px solid #30363d; border-radius:4px; padding:12px; max-height:340px; overflow:auto; font-size:12px; }
.copy { display:inline-block; background:#161b22; border:1px solid #30363d; padding:6px 10px; border-radius:4px; font-family:monospace; cursor:pointer; user-select:all; word-break:break-all; }
.bench { display:grid; grid-template-columns: 1fr 1fr; gap:12px; }
.bench .col { padding:12px; border-radius:6px; background:#161b22; border:1px solid #30363d; }
.bench h3 { margin:0 0 8px 0; font-size:14px; }
.bench .nats h3 { color:var(--nats); }
.bench .cosmos h3 { color:var(--cosmos); }
.stat { display:flex; justify-content:space-between; padding:2px 0; font-size:12px; }
.stat span:first-child { color:var(--muted); }
.meta { color:var(--muted); font-size:11px; }
.ok { color:var(--nats); } .err { color:var(--err); } .warn { color:var(--cosmos); }
.badge { display:inline-block; padding:2px 6px; border-radius:3px; font-size:11px; margin-right:4px; }
.badge.nats { background:var(--nats); color:#0d1117; }
.badge.cosmos { background:var(--cosmos); color:#0d1117; }
"##;

const NAV_JS: &str = r##"
const KEY = "nats-kv-user-key";
const TENANT = "nats-kv-user-tenant";
function userKey() { return localStorage.getItem(KEY) || ""; }
function userTenant() { return localStorage.getItem(TENANT) || ""; }
function renderNav(active) {
  const k = userKey();
  const t = userTenant();
  document.body.insertAdjacentHTML("afterbegin", `
    <nav>
      <a href="/" ${active==='home'?'style="text-decoration:underline"':''}>home</a>
      <a href="/play" ${active==='play'?'style="text-decoration:underline"':''}>playground</a>
      <a href="/topology" ${active==='topology'?'style="text-decoration:underline"':''}>topology</a>
      <a href="/dash" ${active==='dash'?'style="text-decoration:underline"':''}>dashboard</a>
      <span class="key-status ${k?'ok':''}">${k ? 'signed in: '+t+' • key '+k.slice(0,12)+'…' : 'no key (using shared demo)'}</span>
    </nav>
  `);
}
function authedFetch(path, opts={}) {
  opts.headers = opts.headers || {};
  if (userKey()) opts.headers["X-KV-Key"] = userKey();
  return fetch(path, opts);
}
"##;

const INDEX_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV demo</title><style>__SHARED_CSS__</style></head><body>
<h1>NATS-KV demo</h1>
<p class="sub">Globally distributed NATS JetStream KV for Akamai Functions. 27 regions, single &lt;10ms-from-anywhere endpoint, NATS-native primitives Cosmos can't expose.</p>

<fieldset><legend>Quick links</legend>
  <ul>
    <li><a href="/play">Playground</a> — hit the KV from this Spin function with live timings + side-by-side Cosmos comparison</li>
    <li><a href="/dash">Dashboard</a> — your tenant, buckets, API keys (sign in with the key from your invite)</li>
    <li><a href="https://github.com/ccie7599/nats-kv">GitHub</a> — source, SCOPE, ADRs, recipes</li>
  </ul>
</fieldset>

<fieldset><legend>Got an invite?</legend>
  <p class="meta">If your admin sent you a link like <code>/claim/k_inv_…</code>, click it to provision your tenant + API key. Then sign in here.</p>
  <label>Or paste your API key directly (kept in browser localStorage)</label>
  <input id="key" type="password" placeholder="akv_int_…">
  <label>Tenant ID (optional, for display)</label>
  <input id="tenant" placeholder="t_abc123…">
  <div class="actions">
    <button onclick="saveKey()">Save</button>
    <button class="secondary" onclick="clearKey()">Clear</button>
  </div>
</fieldset>

<script>__NAV_JS__
renderNav('home');
function saveKey() {
  const k = document.getElementById("key").value.trim();
  const t = document.getElementById("tenant").value.trim();
  if (k) localStorage.setItem(KEY, k);
  if (t) localStorage.setItem(TENANT, t);
  location.reload();
}
function clearKey() {
  localStorage.removeItem(KEY);
  localStorage.removeItem(TENANT);
  location.reload();
}
</script>
</body></html>
"##;

const CLAIM_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV — claim invite</title><style>__SHARED_CSS__</style></head><body>
<h1>Claim your NATS-KV tenant</h1>
<p class="sub">One-shot URL. Submit to provision a tenant + API key. The key is shown once.</p>

<fieldset id="welcome">
  <legend>Invite</legend>
  <div id="invite-status" class="meta">checking…</div>
  <label>Tag (your team / handle — for display only)</label>
  <input id="tag" placeholder="alice@gaming">
  <div class="actions">
    <button onclick="doClaim()" id="claim-btn">Claim</button>
  </div>
</fieldset>

<fieldset id="result" style="display:none">
  <legend>Provisioned</legend>
  <p>Tenant: <span class="copy" id="r-tenant"></span></p>
  <p>API key (copy now — won't be shown again):</p>
  <p class="copy" id="r-key" onclick="navigator.clipboard.writeText(this.textContent)"></p>
  <p class="meta">Endpoint: <code>https://edge.nats-kv.connected-cloud.io</code></p>
  <p class="meta">Saved to browser. <a href="/dash">Go to dashboard →</a></p>
</fieldset>

<script>__NAV_JS__
renderNav('home');
const token = location.pathname.split("/").pop();

async function check() {
  const r = await fetch("/api/control/v1/claim/" + token);
  const j = await r.json();
  const el = document.getElementById("invite-status");
  if (!r.ok) {
    el.innerHTML = '<span class="err">' + (j.error || "invalid") + '</span>';
    document.getElementById("claim-btn").disabled = true;
  } else {
    el.innerHTML = '<span class="ok">valid</span> — suggested tag: ' + (j.tag_suggested || "(none)") + ', expires ' + new Date(j.expires_at).toLocaleString();
    if (j.tag_suggested) document.getElementById("tag").value = j.tag_suggested;
  }
}
async function doClaim() {
  const tag = document.getElementById("tag").value.trim();
  const r = await fetch("/api/control/v1/claim/" + token, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tag })
  });
  const j = await r.json();
  if (!r.ok) { alert("Claim failed: " + (j.error || r.status)); return; }
  localStorage.setItem(KEY, j.key);
  localStorage.setItem(TENANT, j.tenant_id);
  document.getElementById("welcome").style.display = "none";
  document.getElementById("result").style.display = "block";
  document.getElementById("r-tenant").textContent = j.tenant_id;
  document.getElementById("r-key").textContent = j.key;
}
check();
</script>
</body></html>
"##;

const DASH_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV dashboard</title><style>__SHARED_CSS__</style></head><body>
<h1>Your tenant</h1>
<p class="sub">Sign in with the API key from your invite (home page) to use this dashboard.</p>

<fieldset id="signed-out" style="display:none">
  <legend>Not signed in</legend>
  <p>Go to <a href="/">home</a> and paste your API key, or claim an invite.</p>
</fieldset>

<fieldset id="info" style="display:none">
  <legend>Tenant</legend>
  <p>ID: <span class="copy" id="t-id"></span></p>
  <p>Tag: <span id="t-tag"></span></p>
  <p>Endpoint: <code>https://edge.nats-kv.connected-cloud.io</code></p>
</fieldset>

<fieldset id="buckets" style="display:none">
  <legend>Buckets in your tenant</legend>
  <div class="actions"><button class="secondary" onclick="loadBuckets()">Refresh</button></div>
  <pre id="buckets-out">(click Refresh)</pre>
  <p class="meta">In v0.5 each bucket name will be auto-prefixed with your tenant ID by the adapter so it stays isolated. Today the adapter accepts any token and bucket name (multi-tenancy enforcement is the next integration step).</p>
</fieldset>

<script>__NAV_JS__
renderNav('dash');
if (!userKey()) {
  document.getElementById("signed-out").style.display = "block";
} else {
  document.getElementById("info").style.display = "block";
  document.getElementById("buckets").style.display = "block";
  document.getElementById("t-id").textContent = userTenant() || "(unknown)";
  document.getElementById("t-tag").textContent = "(set on signup)";
  loadBuckets();
}
async function loadBuckets() {
  const r = await authedFetch("/api/nats/v1/admin/buckets");
  const j = await r.json();
  const text = atob(j.body_b64 || "");
  document.getElementById("buckets-out").textContent = text;
}
</script>
</body></html>
"##;

const TOPOLOGY_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV topology</title><style>__SHARED_CSS__
  svg { background:#0d1117; border:1px solid #30363d; border-radius:6px; display:block; margin:0 auto; }
  .land { fill:#161b22; stroke:#21262d; stroke-width:0.5; }
  .grid { stroke:#21262d; stroke-width:0.5; fill:none; }
  .region-dot { fill:#30363d; stroke:#8b949e; stroke-width:0.5; }
  .region-dot.has-bucket { fill:#58a6ff; }
  .region-label { fill:#8b949e; font-size:9px; text-anchor:middle; pointer-events:none; }
  .raft-edge { stroke-width:2; fill:none; opacity:0.8; }
  .raft-fill { fill-opacity:0.15; stroke:none; }
  .leader-ring { fill:none; stroke-width:2; }
  .bucket-row { padding:6px 8px; border-bottom:1px solid #30363d; cursor:pointer; }
  .bucket-row:hover { background:#161b22; }
  .bucket-row.active { background:#1f2933; }
  .pill { display:inline-block; padding:2px 6px; border-radius:3px; font-size:10px; margin-right:4px; background:#30363d; }
  .pill.r1 { background:#8b949e; color:#0d1117; }
  .pill.r3 { background:#3fb950; color:#0d1117; }
  .pill.r5 { background:#d29922; color:#0d1117; }
  .pill.mirror { background:#bc8cff; color:#0d1117; }
  .lag { font-family:ui-monospace, monospace; }
  .lag.ok { color:#3fb950; } .lag.warn { color:#d29922; } .lag.err { color:#f85149; }
  legend.geo { display:flex; gap:12px; font-size:11px; margin:0; padding:0 8px 6px 8px; flex-wrap:wrap; }
  .swatch { display:inline-block; width:10px; height:10px; border-radius:50%; vertical-align:middle; margin-right:4px; }
</style></head><body>
<h1>Consistency-domain topology</h1>
<p class="sub">Each bucket's RAFT replica set rendered geographically. Live replication lag per replica from the JetStream stream metadata.</p>

<fieldset>
  <legend>Buckets <span id="b-count" class="meta"></span></legend>
  <legend class="geo">
    <span><span class="swatch" style="background:#58a6ff"></span>has bucket</span>
    <span><span class="swatch" style="background:#3fb950"></span>R3 leader/replica</span>
    <span><span class="swatch" style="background:#d29922"></span>R5</span>
    <span><span class="swatch" style="background:#bc8cff"></span>mirror</span>
    <span><span class="swatch" style="background:#8b949e"></span>R1 (no replication)</span>
  </legend>
  <div id="bucket-list" style="max-height:200px; overflow:auto; border:1px solid #30363d; border-radius:4px;"></div>
  <div class="actions">
    <button class="secondary" onclick="loadAll()">Refresh</button>
    <button class="secondary" onclick="autoRefresh()" id="auto-btn">Auto-refresh (off)</button>
  </div>
</fieldset>

<svg id="map" viewBox="0 0 1000 500" width="100%" preserveAspectRatio="xMidYMid meet">
  <defs>
    <pattern id="grid" width="50" height="50" patternUnits="userSpaceOnUse">
      <path d="M 50 0 L 0 0 0 50" class="grid"/>
    </pattern>
    <radialGradient id="leader-glow"><stop offset="0%" stop-color="#3fb950" stop-opacity="0.6"/><stop offset="100%" stop-color="#3fb950" stop-opacity="0"/></radialGradient>
  </defs>
  <rect width="1000" height="500" fill="url(#grid)"/>
  <g id="dots"></g>
  <g id="overlay"></g>
  <g id="labels"></g>
</svg>

<fieldset id="bucket-detail" style="margin-top:16px; display:none">
  <legend>Bucket detail</legend>
  <div id="detail-out"></div>
</fieldset>

<script>__NAV_JS__
renderNav('topology');
const REGIONS = {
  "us-ord":       { lat: 41.8781,  lon: -87.6298,  geo: "na" },
  "us-east":      { lat: 40.7357,  lon: -74.1724,  geo: "na" },
  "us-central":   { lat: 32.7767,  lon: -96.7970,  geo: "na" },
  "us-west":      { lat: 37.5485,  lon: -121.9886, geo: "na" },
  "us-southeast": { lat: 33.7490,  lon: -84.3880,  geo: "na" },
  "us-lax":       { lat: 34.0522,  lon: -118.2437, geo: "na" },
  "us-mia":       { lat: 25.7617,  lon: -80.1918,  geo: "na" },
  "us-sea":       { lat: 47.6062,  lon: -122.3321, geo: "na" },
  "ca-central":   { lat: 43.6532,  lon: -79.3832,  geo: "na" },
  "br-gru":       { lat: -23.5505, lon: -46.6333,  geo: "sa" },
  "gb-lon":       { lat: 51.5074,  lon: -0.1278,   geo: "eu" },
  "eu-central":   { lat: 50.1109,  lon: 8.6821,    geo: "eu" },
  "de-fra-2":     { lat: 50.1109,  lon: 8.6821,    geo: "eu" },
  "fr-par-2":     { lat: 48.8566,  lon: 2.3522,    geo: "eu" },
  "nl-ams":       { lat: 52.3676,  lon: 4.9041,    geo: "eu" },
  "se-sto":       { lat: 59.3293,  lon: 18.0686,   geo: "eu" },
  "it-mil":       { lat: 45.4642,  lon: 9.1900,    geo: "eu" },
  "ap-south":     { lat: 1.3521,   lon: 103.8198,  geo: "ap" },
  "sg-sin-2":     { lat: 1.3521,   lon: 103.8198,  geo: "ap" },
  "ap-northeast": { lat: 35.6762,  lon: 139.6503,  geo: "ap" },
  "jp-tyo-3":     { lat: 35.6762,  lon: 139.6503,  geo: "ap" },
  "jp-osa":       { lat: 34.6937,  lon: 135.5023,  geo: "ap" },
  "ap-west":      { lat: 19.0760,  lon: 72.8777,   geo: "ap" },
  "in-bom-2":     { lat: 19.0760,  lon: 72.8777,   geo: "ap" },
  "in-maa":       { lat: 13.0827,  lon: 80.2707,   geo: "ap" },
  "id-cgk":       { lat: -6.2088,  lon: 106.8456,  geo: "ap" },
  "ap-southeast": { lat: -33.8688, lon: 151.2093,  geo: "oc" }
};
function proj(lat, lon) {
  const x = (lon + 180) / 360 * 1000;
  const y = (90 - lat) / 180 * 500;
  return [x, y];
}
function regionFromPeer(name) {
  return name.replace(/^kv-/, "");
}

function drawDots(activeRegions) {
  const g = document.getElementById("dots");
  const lab = document.getElementById("labels");
  g.innerHTML = ""; lab.innerHTML = "";
  for (const [r, m] of Object.entries(REGIONS)) {
    const [x,y] = proj(m.lat, m.lon);
    const cls = activeRegions.has(r) ? "region-dot has-bucket" : "region-dot";
    g.insertAdjacentHTML("beforeend", `<circle cx="${x.toFixed(1)}" cy="${y.toFixed(1)}" r="3" class="${cls}"><title>${r}</title></circle>`);
    lab.insertAdjacentHTML("beforeend", `<text x="${x.toFixed(1)}" y="${(y+12).toFixed(1)}" class="region-label">${r}</text>`);
  }
}

let currentBuckets = [];
let activeBucket = null;

async function loadAll() {
  drawDots(new Set());
  const r = await authedFetch("/api/nats/v1/admin/buckets");
  const j = await r.json();
  if (!r.ok || j.status >= 400) {
    document.getElementById("bucket-list").innerHTML = `<div class="err" style="padding:8px">${j.error || j.body_b64 || ('HTTP ' + r.status)}</div>`;
    return;
  }
  // The proxy wraps result; actual body is base64 JSON
  let payload;
  try { payload = JSON.parse(atob(j.body_b64 || "")); } catch (e) { payload = j; }
  currentBuckets = (payload.buckets || []).filter(b => !b.name.startsWith("kv-admin"));
  document.getElementById("b-count").textContent = `(${currentBuckets.length})`;
  const list = document.getElementById("bucket-list");
  list.innerHTML = currentBuckets.map((b, i) => {
    const repClass = b.replicas === 5 ? "r5" : b.replicas === 3 ? "r3" : "r1";
    const peers = (b.peers || []).map(p => p.name.replace(/^kv-/,'')).join(", ");
    return `<div class="bucket-row" onclick="selectBucket(${i})" id="bucket-row-${i}">
      <span class="pill ${repClass}">R${b.replicas||1}</span>
      <strong>${b.name}</strong>
      <span class="meta"> · ${b.values||0} values · ${(b.bytes||0)} bytes · peers: ${peers||'(none)'}</span>
    </div>`;
  }).join("") || '<div class="meta" style="padding:8px">no buckets yet</div>';
  // collect active regions across all buckets
  const active = new Set();
  for (const b of currentBuckets) {
    for (const p of (b.peers || [])) active.add(regionFromPeer(p.name));
  }
  drawDots(active);
  if (currentBuckets.length > 0) selectBucket(0);
}

function selectBucket(i) {
  activeBucket = i;
  document.querySelectorAll(".bucket-row").forEach((el, j) => el.classList.toggle("active", j === i));
  const b = currentBuckets[i];
  // overlay: triangle of peer points + leader ring
  const overlay = document.getElementById("overlay");
  overlay.innerHTML = "";
  const pts = (b.peers || []).map(p => {
    const r = regionFromPeer(p.name);
    const m = REGIONS[r];
    if (!m) return null;
    const [x,y] = proj(m.lat, m.lon);
    return { x, y, region: r, role: p.role, current: p.current, lag_ms: p.lag_ms };
  }).filter(Boolean);
  const repClass = b.replicas === 5 ? "r5" : b.replicas === 3 ? "r3" : "r1";
  const color = b.replicas === 5 ? "#d29922" : b.replicas === 3 ? "#3fb950" : "#8b949e";
  if (pts.length >= 2) {
    const poly = pts.map(p => `${p.x.toFixed(1)},${p.y.toFixed(1)}`).join(" ");
    overlay.insertAdjacentHTML("beforeend", `<polygon points="${poly}" class="raft-fill" style="fill:${color}"/>`);
    overlay.insertAdjacentHTML("beforeend", `<polyline points="${poly} ${pts[0].x.toFixed(1)},${pts[0].y.toFixed(1)}" class="raft-edge" style="stroke:${color}"/>`);
  }
  for (const p of pts) {
    if (p.role === "leader") {
      overlay.insertAdjacentHTML("beforeend", `<circle cx="${p.x}" cy="${p.y}" r="14" fill="url(#leader-glow)"/>`);
      overlay.insertAdjacentHTML("beforeend", `<circle cx="${p.x}" cy="${p.y}" r="6" class="leader-ring" style="stroke:${color}"/>`);
    }
    overlay.insertAdjacentHTML("beforeend", `<circle cx="${p.x}" cy="${p.y}" r="4" style="fill:${color}"><title>${p.region} (${p.role})</title></circle>`);
  }
  // detail
  const det = document.getElementById("detail-out");
  det.innerHTML = `
    <div><strong>${b.name}</strong> <span class="pill ${repClass}">R${b.replicas||1}</span></div>
    <div class="meta">cluster: ${b.cluster||'?'} · values: ${b.values||0} · bytes: ${b.bytes||0} · history: ${b.history||0}</div>
    <table style="width:100%; margin-top:8px; font-size:12px;">
      <thead><tr><th style="text-align:left">Peer</th><th>Role</th><th>Current</th><th>Lag (ms)</th></tr></thead>
      <tbody>${(b.peers||[]).map(p => `
        <tr>
          <td>${p.name}</td>
          <td>${p.role}</td>
          <td>${p.current ? '<span class="ok">✓</span>' : '<span class="err">✗</span>'}</td>
          <td class="lag ${p.lag_ms<10?'ok':p.lag_ms<100?'warn':'err'}">${p.lag_ms||0}</td>
        </tr>`).join("")}</tbody>
    </table>
  `;
  document.getElementById("bucket-detail").style.display = "block";
}

let timer = null;
function autoRefresh() {
  if (timer) { clearInterval(timer); timer = null; document.getElementById("auto-btn").textContent = "Auto-refresh (off)"; return; }
  timer = setInterval(loadAll, 5000);
  document.getElementById("auto-btn").textContent = "Auto-refresh (5s)";
}

loadAll();
</script>
</body></html>
"##;

const PLAY_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS KV vs Cosmos — playground</title><style>__SHARED_CSS__</style></head><body>
<h1>NATS-KV vs Cosmos playground</h1>
<p class="sub">Server-side timings from inside this Spin function on Akamai Functions. Endpoint: <code>edge.nats-kv.connected-cloud.io</code> (GTM-routed to nearest of 27 regions).</p>

<fieldset>
  <legend>Single op</legend>
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
    <div><label>operation</label><select id="bench-op"><option value="GET">GET</option><option value="PUT">PUT</option></select></div>
    <div><label>iterations</label><input type="number" id="bench-n" value="50" min="1" max="500"></div>
    <div><label>&nbsp;</label><button onclick="runBench()">Run both</button></div>
  </div>
  <div class="bench" id="bench-out" style="margin-top:12px;">
    <div class="col nats"><h3>NATS</h3><div id="nats-stats">(idle)</div></div>
    <div class="col cosmos"><h3>Cosmos</h3><div id="cosmos-stats">(idle)</div></div>
  </div>
  <p class="meta">Both timed server-side inside the Spin function. <code>upstream_us</code> excludes browser↔FWF time. NATS path: FWF→GTM→nearest of 27 regions.</p>
</fieldset>

<fieldset>
  <legend>NATS-unique demos</legend>
  <div class="actions">
    <button class="secondary" onclick="natsHistory()">History scrub (5 revisions)</button>
    <button class="secondary" onclick="natsSubject()">Subject wildcard (users.*.session)</button>
    <button class="secondary" onclick="natsCluster()">Cluster info</button>
  </div>
  <pre id="demo-out" style="margin-top:8px;">(awaiting demo)</pre>
</fieldset>

<script>__NAV_JS__
renderNav('play');
const $ = (id) => document.getElementById(id);

async function op(backend, verb) {
  const k = $("key").value;
  const v = $("value").value;
  if (verb === "INCR") {
    return fetchOp(`nats/v1/kv/demo/${encodeURIComponent(k)}/incr`, "POST", undefined, "single-out", "NATS INCR", "nats");
  }
  const path = backend === "nats" ? `nats/v1/kv/demo/${encodeURIComponent(k)}` : `cosmos/default/${encodeURIComponent(k)}`;
  let body = verb === "PUT" ? v : undefined;
  return fetchOp(path, verb, body, "single-out", `${backend.toUpperCase()} ${verb}`, backend);
}

async function fetchOp(path, method, body, outId, label, backend) {
  const t0 = performance.now();
  const opts = { method, headers: {} };
  if (userKey()) opts.headers["X-KV-Key"] = userKey();
  if (body !== undefined) opts.body = body;
  const r = await fetch("/api/" + path, opts);
  const browserMs = performance.now() - t0;
  const j = await r.json();
  const upstream_ms = (j.upstream_us / 1000).toFixed(2);
  let bodyText = "";
  try { bodyText = atob(j.body_b64 || ""); } catch {}
  $(outId).innerHTML = `
    <div class="stat"><span>${label}</span><span>${backend === "nats" ? "🟢 NATS" : "🟡 COSMOS"} status=${j.status}</span></div>
    <div class="stat"><span>upstream (server-side)</span><span><strong>${upstream_ms} ms</strong></span></div>
    <div class="stat"><span>browser overhead</span><span>${(browserMs - parseFloat(upstream_ms)).toFixed(1)} ms</span></div>
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
    const upstream = []; const browser = [];
    const path = backend === "nats" ? `nats/v1/kv/demo/${key}` : `cosmos/default/${key}`;
    for (let i = 0; i < N; i++) {
      const t0 = performance.now();
      const opts = { method: verb, headers: {} };
      if (userKey()) opts.headers["X-KV-Key"] = userKey();
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
    <div class="stat" style="margin-top:6px;border-top:1px solid #30363d;padding-top:6px;"><span>browser p50</span><span>${browserStats.p50.toFixed(1)} ms</span></div>
  `;
  const ns = stats(nats.upstream); const cs = stats(cosmos.upstream);
  $("nats-stats").innerHTML = renderCol(ns, stats(nats.browser));
  $("cosmos-stats").innerHTML = renderCol(cs, stats(cosmos.browser));
  const winner = ns.p50 < cs.p50 ? "nats-stats" : "cosmos-stats";
  const ratio = ns.p50 < cs.p50 ? (cs.p50/ns.p50) : (ns.p50/cs.p50);
  $(winner).innerHTML = `<div style="color:#3fb950;font-weight:bold;margin-bottom:6px;">🏆 ${ratio.toFixed(1)}× faster on p50</div>` + $(winner).innerHTML;
}

async function natsHistory() {
  const k = "demo-key-" + Date.now();
  for (let i = 1; i <= 5; i++) {
    await authedFetch(`/api/nats/v1/kv/demo/${k}`, { method: "PUT", body: "version-" + i });
  }
  const r = await authedFetch(`/api/nats/v1/kv/demo/${k}/history`);
  const j = await r.json();
  $("demo-out").textContent = `Wrote 5 revisions of "${k}". History (server-side ${(j.upstream_us/1000).toFixed(1)} ms):\n` + atob(j.body_b64);
}
async function natsSubject() {
  const id = Date.now();
  for (const u of ["alice","bob","carol","dave"]) {
    await authedFetch(`/api/nats/v1/kv/demo/users.${u}.${id}.session`, { method: "PUT", body: u });
  }
  const r = await authedFetch(`/api/nats/v1/kv/demo/keys?match=users.*.${id}.session`);
  const j = await r.json();
  $("demo-out").textContent = `Wrote 4 keys with subject pattern users.<name>.${id}.session\nWildcard query users.*.${id}.session (server-side ${(j.upstream_us/1000).toFixed(1)} ms):\n` + atob(j.body_b64) + `\n\nCosmos has no equivalent — keys must be exact-match or full table scan.`;
}
async function natsCluster() {
  const r = await authedFetch(`/api/nats/v1/admin/cluster`);
  const j = await r.json();
  $("demo-out").textContent = atob(j.body_b64);
}
</script>
</body></html>
"##;
