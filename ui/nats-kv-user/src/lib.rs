use spin_sdk::http::{IntoResponse, Method, Request, Response};
use spin_sdk::http_component;
use spin_sdk::key_value::Store;
use spin_sdk::variables;

const ADAPTER_BASE: &str = "http://edge.nats-kv.connected-cloud.io:8080";
const CONTROL_BASE: &str = "https://cp.nats-kv.connected-cloud.io";
const FALLBACK_TOKEN: &str = "akv_demo_open"; // used by playground if user not signed in
const UI_COOKIE: &str = "nats-kv-ui-access";

fn ui_gate_token() -> String {
    variables::get("ui_gate_token").unwrap_or_else(|_| "demo-open-2026".to_string())
}

// True if the request carries a valid UI gate cookie OR a valid `?access=` query.
// Whitelisted paths bypass this entirely (see is_public_path).
fn is_ui_authed(req: &Request) -> bool {
    let want = ui_gate_token();
    // Cookie path
    if let Some(cookie_hdr) = req.header("cookie").and_then(|v| v.as_str()) {
        for kv in cookie_hdr.split(';') {
            let p = kv.trim();
            if let Some(rest) = p.strip_prefix(&format!("{UI_COOKIE}=")) {
                if rest == want { return true; }
            }
        }
    }
    // Query string path
    let q = req.query();
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("access=") {
            if v == want { return true; }
        }
    }
    false
}

// Paths that don't require the UI gate. Everything else does.
fn is_public_path(path: &str) -> bool {
    matches!(path,
        "/health" | "/api/request-invite"
    )
    || path.starts_with("/claim/")           // existing claim flow stays open
    || path.starts_with("/static/")          // future static assets
}

// HTML page shown when an unauthed visitor lands. They can either request
// an invite (queued for the admin to grant) or paste an access token.
fn render_gate_page() -> anyhow::Result<Response> {
    html(GATE_HTML)
}

// Build a 302 to the same path minus `?access=...`, with a Set-Cookie that
// remembers the gate so subsequent navigation doesn't need the query string.
fn redirect_with_cookie(path_no_query: &str) -> anyhow::Result<Response> {
    let token = ui_gate_token();
    // 14 days; HttpOnly so JS can't read it; SameSite=Lax to allow nav.
    let cookie = format!("{UI_COOKIE}={token}; Path=/; Max-Age=1209600; HttpOnly; SameSite=Lax; Secure");
    Ok(Response::builder()
        .status(302)
        .header("location", path_no_query)
        .header("set-cookie", cookie)
        .body(Vec::<u8>::new())
        .build())
}

#[http_component]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    let path = req.path();

    // ---- UI gate ----
    // If the request is unauthed AND not on a public path, either capture the
    // ?access= token (set cookie + 302 to clean URL) or show the gate page.
    if !is_public_path(path) && !is_ui_authed(&req) {
        return render_gate_page();
    }
    // If they came in with ?access=... and are now authed, drop the query param
    // from the visible URL by setting the cookie + redirecting to the bare path.
    if !is_public_path(path) && req.query().contains("access=") {
        return redirect_with_cookie(path);
    }

    // POST /api/request-invite — visitor submits name + email, queued for admin.
    if path == "/api/request-invite" && req.method() == &Method::Post {
        return forward_invite_request(&req).await;
    }

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
    if path == "/docs" || path == "/docs/" {
        return html(DOCS_HTML);
    }
    if path == "/loadtest" || path == "/loadtest/" {
        return html(LOADTEST_HTML);
    }
    if path == "/verify" || path == "/verify/" {
        return html(VERIFY_HTML);
    }
    if path == "/api-explorer" || path == "/api-explorer/" {
        return html(API_EXPLORER_HTML);
    }
    if path == "/openapi.yaml" {
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/yaml")
            .header("access-control-allow-origin", "*")
            .body(OPENAPI_YAML.to_string())
            .build());
    }
    if path.starts_with("/claim/") {
        // Serving a claim URL implies a vetted recipient — set the UI gate
        // cookie so the recipient can navigate past /play / /dash / /docs
        // immediately after the claim flow completes, without also needing
        // the ?access= query param. The admin app's invite share URLs append
        // ?access=<gate> too as belt-and-suspenders, but this makes
        // /claim/<token>-only links work just as well.
        let token = ui_gate_token();
        let cookie = format!("{UI_COOKIE}={token}; Path=/; Max-Age=1209600; HttpOnly; SameSite=Lax; Secure");
        let body = CLAIM_HTML
            .replace("__SHARED_CSS__", SHARED_CSS)
            .replace("__NAV_JS__", NAV_JS);
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "text/html; charset=utf-8")
            .header("set-cookie", cookie)
            .body(body)
            .build());
    }
    if path == "/health" {
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true,"app":"nats-kv-user","backends":["nats","cosmos"]}"#)
            .build());
    }

    // /api/probe-ip — hit the us-ord NB by IP (no DNS lookup), plain HTTP. Diff vs
    // /api/nats by hostname approximates DNS resolution cost on the FWF Spin path.
    if path == "/api/probe-ip" {
        let mut b = Request::builder();
        b.method(Method::Get);
        b.uri("http://172.237.141.164/v1/health");
        b.body(Vec::<u8>::new());
        let t0 = std::time::Instant::now();
        let upstream: Response = spin_sdk::http::send(b.build()).await?;
        let elapsed_us = t0.elapsed().as_micros();
        let body_b64 = b64encode(upstream.body());
        let payload = format!(
            r#"{{"upstream_us":{elapsed_us},"status":{},"body_b64":"{body_b64}"}}"#,
            *upstream.status() as u16,
        );
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(payload)
            .build());
    }

    // /api/probe-claudebot — legacy probe to a third-party Chicago Linode (kept for now)
    if path == "/api/probe-claudebot" {
        let mut b = Request::builder();
        b.method(Method::Get);
        b.uri("http://172.236.104.189:8888/");
        b.body(Vec::<u8>::new());
        let t0 = std::time::Instant::now();
        let upstream: Response = spin_sdk::http::send(b.build()).await?;
        let elapsed_us = t0.elapsed().as_micros();
        let body_b64 = b64encode(upstream.body());
        let payload = format!(
            r#"{{"upstream_us":{elapsed_us},"status":{},"body_b64":"{body_b64}"}}"#,
            *upstream.status() as u16,
        );
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(payload)
            .build());
    }

    // /api/probe-https — same Chicago Linode but HTTPS with valid wildcard cert
    if path == "/api/probe-https" {
        let mut b = Request::builder();
        b.method(Method::Get);
        b.uri("https://probe.nats-kv.connected-cloud.io:8443/");
        b.body(Vec::<u8>::new());
        let t0 = std::time::Instant::now();
        let upstream: Response = spin_sdk::http::send(b.build()).await?;
        let elapsed_us = t0.elapsed().as_micros();
        let body_b64 = b64encode(upstream.body());
        let payload = format!(
            r#"{{"upstream_us":{elapsed_us},"status":{},"body_b64":"{body_b64}"}}"#,
            *upstream.status() as u16,
        );
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(payload)
            .build());
    }

    // /api/whereami — Spin function calls an IP-echo service so we can see where
    // FWF is actually egressing from (FWF's outbound IP, geo).
    if path == "/api/whereami" {
        let mut b = Request::builder();
        b.method(Method::Get);
        b.uri("https://ipinfo.io/json");
        b.header("Accept", "application/json");
        b.body(Vec::<u8>::new());
        let t0 = std::time::Instant::now();
        let upstream: Response = spin_sdk::http::send(b.build()).await?;
        let elapsed_us = t0.elapsed().as_micros();
        let body_b64 = b64encode(upstream.body());
        let payload = format!(
            r#"{{"upstream_us":{elapsed_us},"status":{},"body_b64":"{body_b64}"}}"#,
            *upstream.status() as u16,
        );
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(payload)
            .build());
    }

    // /api/nats/<path...>  — proxy to NATS adapter using caller's bearer key (or fallback)
    if let Some(rest) = path.strip_prefix("/api/nats/") {
        let key = caller_bearer(&req).unwrap_or_else(|| FALLBACK_TOKEN.to_string());
        let qs = req.query();
        let path_with_qs = if qs.is_empty() { rest.to_string() } else { format!("{rest}?{qs}") };
        return Ok(call_nats(req.method().clone(), &path_with_qs, req.body(), &key).await?);
    }

    // /api/cosmos/<bucket>/<key> — Spin's managed KV (Cosmos backend on FWF)
    if let Some(rest) = path.strip_prefix("/api/cosmos/") {
        return Ok(call_cosmos(req.method().clone(), rest, req.body()).await?);
    }

    // /api/control/<path...> — proxy to control plane (caller's bearer forwarded)
    if let Some(rest) = path.strip_prefix("/api/control/") {
        let bearer = req.header("authorization").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let qs = req.query();
        let path_with_qs = if qs.is_empty() { rest.to_string() } else { format!("{rest}?{qs}") };
        return Ok(call_control(req.method().clone(), &path_with_qs, req.body(), &bearer).await?);
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

// Forward the visitor's invite request body to the control plane's open
// /v1/internal/invite-requests endpoint. Reuses the same HTTPS path as
// /api/control/* (proven working) so we don't re-introduce the TLS shape
// quirks of a hand-rolled Request::builder.
async fn forward_invite_request(req: &Request) -> anyhow::Result<Response> {
    call_control(Method::Post, "v1/internal/invite-requests", req.body(), "").await
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
      <a href="/verify" ${active==='verify'?'style="text-decoration:underline"':''}>verify</a>
      <a href="/loadtest" ${active==='loadtest'?'style="text-decoration:underline"':''}>load test</a>
      <a href="/docs" ${active==='docs'?'style="text-decoration:underline"':''}>docs</a>
      <a href="/api-explorer" ${active==='api'?'style="text-decoration:underline"':''}>API</a>
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

<fieldset id="create-bucket" style="display:none">
  <legend>Create a bucket</legend>
  <div class="row">
    <div><label>name (will be prefixed with tenant ID)</label><input id="b-name" placeholder="sessions"></div>
    <div><label>replicas</label><select id="b-replicas" onchange="refreshPlacementPreview()"><option value="1">R1 (single replica, max throughput)</option><option value="3" selected>R3 (RAFT, durable)</option><option value="5">R5 (RAFT, geo-spread)</option></select></div>
    <div><label>geo (RAFT placement)</label><select id="b-geo" onchange="onGeoChange()"><option value="auto" selected>auto (latency-driven)</option><option value="na">NA</option><option value="eu">EU</option><option value="ap">AP</option><option value="sa">SA</option></select></div>
    <div id="b-anchor-row"><label>anchor region (for auto)</label><select id="b-anchor" onchange="refreshPlacementPreview()"></select></div>
  </div>
  <div id="placement-preview" class="meta" style="margin-top:8px; padding:8px; border:1px solid #30363d; border-radius:4px; background:#0d1117; display:none"></div>
  <div class="actions">
    <label style="display:flex; align-items:center; gap:6px; margin:0;"><input type="checkbox" id="b-mirrors" checked style="width:auto"> auto-create mirrors in other regions for local reads</label>
    <button onclick="createBucket()">Create</button>
  </div>
  <div id="create-out" class="meta" style="margin-top:6px"></div>
  <div id="create-decision" style="margin-top:6px; display:none"></div>
</fieldset>

<fieldset id="buckets" style="display:none">
  <legend>Your buckets</legend>
  <div class="actions"><button class="secondary" onclick="loadBuckets()">Refresh</button></div>
  <table id="b-tbl" style="width:100%; font-size:12px; border-collapse:collapse;">
    <thead><tr><th style="text-align:left;border-bottom:1px solid #30363d;padding:6px">Bucket</th><th style="border-bottom:1px solid #30363d;padding:6px">Repl</th><th style="text-align:left;border-bottom:1px solid #30363d;padding:6px">RAFT</th><th style="border-bottom:1px solid #30363d;padding:6px">Mirrors</th></tr></thead>
    <tbody><tr><td colspan="4" class="meta">(click Refresh)</td></tr></tbody>
  </table>
</fieldset>

<script>__NAV_JS__
// Regions list — kept in sync with internal/placement/geo.go AllRegions.
// Declared up here (before any function call site) so populateAnchors()
// doesn't hit a TDZ ReferenceError when the page-load block runs.
const ALL_REGIONS = [
  "us-ord","us-east","us-central","us-west","us-southeast","us-lax","us-mia","us-sea","ca-central","br-gru",
  "gb-lon","eu-central","de-fra-2","fr-par-2","nl-ams","se-sto","it-mil",
  "ap-south","sg-sin-2","ap-northeast","jp-tyo-3","jp-osa","ap-west","in-bom-2","in-maa","id-cgk","ap-southeast",
];

renderNav('dash');
if (!userKey()) {
  document.getElementById("signed-out").style.display = "block";
} else {
  document.getElementById("info").style.display = "block";
  document.getElementById("create-bucket").style.display = "block";
  document.getElementById("buckets").style.display = "block";
  populateAnchors();
  onGeoChange();         // render initial preview (default geo=auto, anchor=us-ord, R3)
  loadMe();
  loadBuckets();
}
async function loadMe() {
  const r = await authedFetch("/api/control/v1/me", { headers: {"Authorization": "Bearer " + userKey()} });
  const j = await r.json();
  document.getElementById("t-id").textContent = j.tenant_id || "(unknown)";
  document.getElementById("t-tag").textContent = j.tag || "(no tag)";
}

function populateAnchors() {
  const sel = document.getElementById("b-anchor");
  sel.innerHTML = "";
  for (const r of ALL_REGIONS) {
    const opt = document.createElement("option");
    opt.value = r; opt.textContent = r;
    sel.appendChild(opt);
  }
  // Default to whatever /api/whereami says about the FWF egress region.
  fetch("/api/whereami").then(r=>r.json()).then(w=>{
    try {
      const ip = JSON.parse(atob(w.body_b64||"")).region || "";
      // fall through; we don't have a mapping from generic geo to NATS region,
      // so just leave default us-ord and let the user pick.
    } catch (e) {}
  }).catch(()=>{});
  sel.value = "us-ord";
}

function onGeoChange() {
  const geo = document.getElementById("b-geo").value;
  document.getElementById("b-anchor-row").style.display = geo === "auto" ? "" : "none";
  refreshPlacementPreview();
}

let _previewSeq = 0;
async function refreshPlacementPreview() {
  const geo = document.getElementById("b-geo").value;
  const previewEl = document.getElementById("placement-preview");
  if (geo !== "auto") {
    previewEl.style.display = "none";
    return;
  }
  const replicas = parseInt(document.getElementById("b-replicas").value, 10);
  const anchor = document.getElementById("b-anchor").value;
  const seq = ++_previewSeq;
  previewEl.style.display = "block";
  previewEl.innerHTML = `<span class="meta">computing placement for anchor=${anchor} R${replicas}…</span>`;
  try {
    const r = await fetch(`/api/control/v1/placement/preview?anchor=${encodeURIComponent(anchor)}&replicas=${replicas}`);
    if (seq !== _previewSeq) return; // newer preview already in flight
    if (!r.ok) {
      const j = await r.json().catch(()=>({error:`HTTP ${r.status}`}));
      previewEl.innerHTML = `<span class="err">${j.error||r.status}</span>`;
      return;
    }
    const d = await r.json();
    previewEl.innerHTML = renderPlacement(d);
  } catch (e) {
    if (seq !== _previewSeq) return;
    previewEl.innerHTML = `<span class="err">preview failed: ${e.message}</span>`;
  }
}

function renderPlacement(d) {
  const cands = (d.candidates||[]).map(c => {
    const winner = c.geo === d.chosen_geo;
    const cls = winner ? "ok" : (c.eligible ? "" : "meta");
    const score = c.eligible ? `${c.write_latency_ms.toFixed(1)}ms` : "—";
    const regions = (c.regions||[]).join(", ") || "—";
    return `<tr style="${winner?'font-weight:600':''}"><td style="padding:2px 8px"><span class="${cls}">${c.geo}</span>${winner?' ★':''}</td><td style="padding:2px 8px">${score}</td><td style="padding:2px 8px">${c.eligible?c.quorum_edge_ms.toFixed(1)+'ms':'—'}</td><td style="padding:2px 8px;font-size:11px">${regions}</td><td style="padding:2px 8px;font-size:11px;color:#8b949e">${c.reason||''}</td></tr>`;
  }).join("");
  const sampled = d.matrix_sampled_at ? new Date(d.matrix_sampled_at).toLocaleTimeString() : "—";
  // Predicted vs actual: the engine's chosen_regions is the top-k by RTT from
  // anchor. The placement tag we hand JetStream is just `geo:<g>`, so NATS
  // picks 3-5 servers carrying that tag using its own load-balance — different
  // members of the same geo, not necessarily our top-k. Show both when we
  // have it.
  const predicted = (d.chosen_regions||[]).join(", ");
  const actual = (d.actual_regions||[]).join(", ");
  let placementLine = `Winner: <span class="ok"><b>${d.chosen_geo}</b></span> · expected write ${d.write_latency_ms.toFixed(1)}ms · quorum edge ${d.quorum_edge_ms.toFixed(1)}ms`;
  if (actual) {
    placementLine += `<br><span class="meta">predicted regions: <code>${predicted}</code></span><br>actual regions: <code>${actual}</code> <span class="meta">(NATS picks within geo:${d.chosen_geo})</span>`;
  } else {
    placementLine += ` · regions <code>${predicted}</code>`;
  }
  return `
    <div style="margin-bottom:6px"><b>Auto-placement</b> · anchor <code>${d.anchor}</code> · R${d.replicas} · matrix sampled ${sampled}</div>
    <div style="margin-bottom:6px">${placementLine}</div>
    <table style="width:100%; border-collapse:collapse; font-size:12px">
      <thead><tr style="border-bottom:1px solid #30363d"><th style="text-align:left;padding:2px 8px">geo</th><th style="text-align:left;padding:2px 8px">write</th><th style="text-align:left;padding:2px 8px">quorum edge</th><th style="text-align:left;padding:2px 8px">regions</th><th style="text-align:left;padding:2px 8px">reason</th></tr></thead>
      <tbody>${cands}</tbody>
    </table>
    ${(d.notes||[]).length?`<div class="meta" style="margin-top:4px">${(d.notes||[]).map(n=>'• '+n).join('<br>')}</div>`:''}
  `;
}

async function createBucket() {
  const body = {
    name: document.getElementById("b-name").value.trim(),
    replicas: parseInt(document.getElementById("b-replicas").value, 10),
    geo: document.getElementById("b-geo").value,
    no_mirrors: !document.getElementById("b-mirrors").checked,
    history: 8,
  };
  if (body.geo === "auto") {
    body.anchor = document.getElementById("b-anchor").value;
  }
  if (!body.name) { alert("name required"); return; }
  const r = await authedFetch("/api/control/v1/me/buckets", {
    method: "POST",
    headers: { "Content-Type": "application/json", "Authorization": "Bearer " + userKey() },
    body: JSON.stringify(body),
  });
  const j = await r.json();
  if (!r.ok) { document.getElementById("create-out").innerHTML = `<span class="err">${j.error||r.status}</span>`; return; }
  document.getElementById("create-out").innerHTML = `<span class="ok">created ${j.bucket}</span> · ${(j.mirrors||[]).length} mirrors`;
  // Render the placement decision the server actually applied (matches preview
  // when the user accepted defaults; differs if they manually picked a geo).
  const decEl = document.getElementById("create-decision");
  if (j.placement) {
    decEl.style.display = "block";
    decEl.innerHTML = `<div style="padding:8px; border:1px solid #30363d; border-radius:4px; background:#0d1117">${renderPlacement(j.placement)}</div>`;
  } else {
    decEl.style.display = "none";
  }
  document.getElementById("b-name").value = "";
  loadBuckets();
}
async function loadBuckets() {
  const r = await authedFetch("/api/control/v1/me/buckets", { headers: {"Authorization": "Bearer " + userKey()} });
  const j = await r.json();
  const tb = document.getElementById("b-tbl").querySelector("tbody");
  if (!r.ok) {
    tb.innerHTML = `<tr><td colspan="4" class="err">${j.error||r.status}</td></tr>`;
    return;
  }
  const details = j.details || [];
  if (details.length === 0) {
    tb.innerHTML = '<tr><td colspan="4" class="meta">(no buckets yet — create one above)</td></tr>';
    return;
  }
  tb.innerHTML = details.map(b => {
    const peers = (b.peers||[]).map(p => p.name.replace('kv-','')).join(', ') || (b.leader||'').replace('kv-','');
    return `<tr><td style="padding:6px;border-bottom:1px solid #30363d">${b.name}</td><td style="text-align:center;padding:6px;border-bottom:1px solid #30363d">R${b.replicas||'?'}</td><td class="meta" style="padding:6px;border-bottom:1px solid #30363d">${peers}</td><td style="text-align:center;padding:6px;border-bottom:1px solid #30363d">${b.mirror_count||0}</td></tr>`;
  }).join("");
}
</script>
</body></html>
"##;

const TOPOLOGY_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV topology</title><style>__SHARED_CSS__
  svg { background:#0d1117; border:1px solid #30363d; border-radius:6px; display:block; margin:0 auto; }
  .land { fill:#1c2128; stroke:#30363d; stroke-width:0.4; pointer-events:none; }
  .graticule { stroke:#161b22; stroke-width:0.3; fill:none; pointer-events:none; }
  .sphere { fill:#0d1117; stroke:#30363d; stroke-width:0.5; pointer-events:none; }
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
    <radialGradient id="leader-glow"><stop offset="0%" stop-color="#3fb950" stop-opacity="0.6"/><stop offset="100%" stop-color="#3fb950" stop-opacity="0"/></radialGradient>
  </defs>
  <g id="graticule"></g>
  <g id="land"></g>
  <g id="dots"></g>
  <g id="overlay"></g>
  <g id="labels"></g>
</svg>

<fieldset id="bucket-detail" style="margin-top:16px; display:none">
  <legend>Bucket detail</legend>
  <div id="detail-out"></div>
</fieldset>

<script src="https://cdn.jsdelivr.net/npm/d3@7"></script>
<script src="https://cdn.jsdelivr.net/npm/topojson-client@3"></script>
<script>__NAV_JS__
renderNav('topology');

// Equirectangular projection that *matches* the hand-rolled proj() below:
//   x = (lon + 180) / 360 * 1000   ⇒  d3 scale = 1000 / (2π), translate = [500, 250]
//   y = (90  - lat) / 180 * 500
// Country paths therefore line up exactly with the region dots.
async function drawWorld() {
  if (typeof d3 === "undefined" || typeof topojson === "undefined") return;
  const projection = d3.geoEquirectangular()
    .scale(1000 / (2 * Math.PI))
    .translate([500, 250]);
  const path = d3.geoPath(projection);
  // 15° graticule for subtle dark-theme polish.
  const graticule = d3.geoGraticule().step([15, 15]);
  document.getElementById("graticule").innerHTML =
    `<path class="graticule" d="${path(graticule())}"/>`;
  try {
    const r = await fetch("https://cdn.jsdelivr.net/npm/world-atlas@2/countries-110m.json");
    const world = await r.json();
    const countries = topojson.feature(world, world.objects.countries);
    const g = document.getElementById("land");
    g.innerHTML = countries.features
      .map(f => `<path class="land" d="${path(f)}"/>`)
      .join("");
  } catch (e) {
    console.warn("world-atlas load failed; topology page will render without coastlines", e);
  }
}
drawWorld();

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
  const overlay = document.getElementById("overlay");
  overlay.innerHTML = "";
  // RAFT peers
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
  // RAFT centroid for mirror dashed lines
  let cx = 0, cy = 0; for (const p of pts) { cx += p.x; cy += p.y; }
  if (pts.length) { cx /= pts.length; cy /= pts.length; }
  // Mirror replicas
  const mirrors = (b.mirrors || []).map(m => {
    const leaderRegion = m.leader ? regionFromPeer(m.leader) : null;
    if (!leaderRegion || !REGIONS[leaderRegion]) return null;
    const [x,y] = proj(REGIONS[leaderRegion].lat, REGIONS[leaderRegion].lon);
    return { x, y, region: leaderRegion, lag_msgs: m.lag_msgs||0, tags: m.placement_tags||[] };
  }).filter(Boolean);
  for (const m of mirrors) {
    overlay.insertAdjacentHTML("beforeend", `<line x1="${cx.toFixed(1)}" y1="${cy.toFixed(1)}" x2="${m.x.toFixed(1)}" y2="${m.y.toFixed(1)}" stroke="#bc8cff" stroke-width="1" stroke-dasharray="3 3" opacity="0.6"/>`);
    overlay.insertAdjacentHTML("beforeend", `<circle cx="${m.x}" cy="${m.y}" r="3" fill="#bc8cff"><title>mirror in ${m.region} (lag ${m.lag_msgs} msgs)</title></circle>`);
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
    <div><strong>${b.name}</strong> <span class="pill ${repClass}">R${b.replicas||1}</span> ${(b.mirrors||[]).length ? `<span class="pill mirror">+${b.mirrors.length} mirrors</span>` : ''}</div>
    <div class="meta">cluster: ${b.cluster||'?'} · values: ${b.values||0} · bytes: ${b.bytes||0} · history: ${b.history||0}</div>
    <h3 style="font-size:13px; margin:12px 0 4px">RAFT replicas</h3>
    <table style="width:100%; font-size:12px;">
      <thead><tr><th style="text-align:left">Peer</th><th>Role</th><th>Current</th></tr></thead>
      <tbody>${(b.peers||[]).map(p => `
        <tr>
          <td>${p.name}</td>
          <td>${p.role}</td>
          <td>${p.current ? '<span class="ok">✓</span>' : '<span class="err">✗</span>'}</td>
        </tr>`).join("")}</tbody>
    </table>
    ${(b.mirrors||[]).length ? `
    <h3 style="font-size:13px; margin:12px 0 4px">Async mirrors (read replicas) <span class="meta" style="font-weight:normal;font-size:11px">— ${b.mirrors.length} of 27 regions</span></h3>
    <table style="width:100%; font-size:12px;">
      <thead><tr><th style="text-align:left">Stream</th><th>Leader</th><th>Tags</th></tr></thead>
      <tbody>${(b.mirrors||[]).map(m => `
        <tr>
          <td>${m.stream}</td>
          <td>${m.leader||'?'}</td>
          <td>${(m.placement_tags||[]).join(',')}</td>
        </tr>`).join("")}</tbody>
    </table>` : ''}
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
  <legend>NATS bucket under test</legend>
  <div class="row">
    <div><label>bucket</label><select id="play-bucket" onchange="updateBucketInfo()"></select></div>
    <div style="flex:2"><label>placement</label><div id="play-bucket-info" class="meta" style="font-size:11px; padding:6px 0">(loading…)</div></div>
  </div>
  <p class="meta" style="margin:0; font-size:11px;">Pick any bucket you own (plus the shared <code>demo</code>) to compare topologies — R1 vs R3 vs R5, or different geo placements. NATS calls below run against the selected bucket; Cosmos always uses Spin's managed default store.</p>
</fieldset>

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

// Selected bucket for all NATS-side playground operations. Cosmos always uses
// Spin's managed default store — there's only one of those — so the picker
// only affects NATS calls.
function natsBucket() {
  const sel = $("play-bucket");
  return (sel && sel.value) || "demo";
}

// Populate the picker: shared `demo` always available; tenant buckets if signed in.
// Caches details for the info panel (placement / mirror count) keyed by bucket name.
const _bucketDetails = {};
async function loadPlayBuckets() {
  const sel = $("play-bucket");
  const opts = [{name: "demo", label: "demo (shared, R3 NA, 27 mirrors)", details: null}];
  if (userKey()) {
    try {
      const r = await fetch("/api/control/v1/me/buckets", { headers: {"Authorization": "Bearer " + userKey()} });
      if (r.ok) {
        const j = await r.json();
        for (const d of (j.details||[])) {
          // strip the tenant prefix from the visible label so the picker is readable.
          const short = d.name.includes("__") ? d.name.split("__").slice(1).join("__") : d.name;
          opts.push({name: d.name, label: `${short} (R${d.replicas||1}, ${d.mirror_count||0} mirrors)`, details: d});
        }
      }
    } catch (e) { /* leave demo only */ }
  }
  // Try to fetch the demo bucket's details too (anyone can read /v1/admin/buckets via a valid key).
  try {
    const r = await fetch("/api/nats/v1/admin/buckets", { headers: {"X-KV-Key": userKey() || "akv_demo_open"} });
    const j = await r.json();
    const inner = JSON.parse(atob(j.body_b64||""));
    for (const b of (inner.buckets||[])) {
      _bucketDetails[b.name] = b;
    }
  } catch (e) {}
  for (const o of opts) {
    if (!_bucketDetails[o.name] && o.details) _bucketDetails[o.name] = o.details;
  }
  sel.innerHTML = opts.map(o => `<option value="${o.name}">${o.label}</option>`).join("");
  updateBucketInfo();
}

function updateBucketInfo() {
  const name = natsBucket();
  const d = _bucketDetails[name];
  const el = $("play-bucket-info");
  if (!d) { el.textContent = `bucket=${name} (no details available)`; return; }
  const peers = (d.peers||[]).map(p => (p.name||'').replace(/^kv-/,'')).join(", ") || (d.leader||'').replace(/^kv-/,'') || "?";
  const tags = (d.placement_tags||[]).join(",") || "(none)";
  const mirrorCount = d.mirror_count !== undefined ? d.mirror_count : (d.mirrors||[]).length;
  el.innerHTML = `<code>${name}</code> · R${d.replicas||1} · placement <code>${tags}</code> · peers <code>${peers}</code> · ${mirrorCount} read mirrors`;
}

async function op(backend, verb) {
  const k = $("key").value;
  const v = $("value").value;
  const b = natsBucket();
  if (verb === "INCR") {
    return fetchOp(`nats/v1/kv/${encodeURIComponent(b)}/${encodeURIComponent(k)}/incr`, "POST", undefined, "single-out", `NATS INCR (${b})`, "nats");
  }
  const path = backend === "nats" ? `nats/v1/kv/${encodeURIComponent(b)}/${encodeURIComponent(k)}` : `cosmos/default/${encodeURIComponent(k)}`;
  let body = verb === "PUT" ? v : undefined;
  const label = backend === "nats" ? `NATS ${verb} (${b})` : `COSMOS ${verb}`;
  return fetchOp(path, verb, body, "single-out", label, backend);
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
  const b = natsBucket();
  $("nats-stats").innerHTML = `running… (bucket: <code>${b}</code>)`;
  $("cosmos-stats").textContent = "running...";
  const collect = async (backend) => {
    const upstream = []; const browser = [];
    const path = backend === "nats" ? `nats/v1/kv/${encodeURIComponent(b)}/${key}` : `cosmos/default/${key}`;
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
  const b = natsBucket();
  const k = "demo-key-" + Date.now();
  for (let i = 1; i <= 5; i++) {
    await authedFetch(`/api/nats/v1/kv/${encodeURIComponent(b)}/${k}`, { method: "PUT", body: "version-" + i });
  }
  const r = await authedFetch(`/api/nats/v1/kv/${encodeURIComponent(b)}/${k}/history`);
  const j = await r.json();
  $("demo-out").textContent = `Wrote 5 revisions of "${k}" in ${b}. History (server-side ${(j.upstream_us/1000).toFixed(1)} ms):\n` + atob(j.body_b64);
}
async function natsSubject() {
  const b = natsBucket();
  const id = Date.now();
  for (const u of ["alice","bob","carol","dave"]) {
    await authedFetch(`/api/nats/v1/kv/${encodeURIComponent(b)}/users.${u}.${id}.session`, { method: "PUT", body: u });
  }
  const r = await authedFetch(`/api/nats/v1/kv/${encodeURIComponent(b)}/keys?match=users.*.${id}.session`);
  const j = await r.json();
  $("demo-out").textContent = `Wrote 4 keys with subject pattern users.<name>.${id}.session in ${b}\nWildcard query users.*.${id}.session (server-side ${(j.upstream_us/1000).toFixed(1)} ms):\n` + atob(j.body_b64) + `\n\nCosmos has no equivalent — keys must be exact-match or full table scan.`;
}
async function natsCluster() {
  const r = await authedFetch(`/api/nats/v1/admin/cluster`);
  const j = await r.json();
  $("demo-out").textContent = atob(j.body_b64);
}

loadPlayBuckets();
</script>
</body></html>
"##;

const DOCS_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV — docs</title><style>__SHARED_CSS__
  .toc { position:sticky; top:8px; float:right; width:220px; margin:0 0 16px 16px; padding:10px; border:1px solid #30363d; border-radius:4px; background:#0d1117; font-size:12px; }
  .toc a { display:block; padding:2px 0; color:#8b949e; text-decoration:none; }
  .toc a:hover { color:#58a6ff; }
  h2 { margin-top:32px; padding-top:8px; border-top:1px solid #21262d; }
  h3 { margin-top:18px; }
  pre { background:#0d1117; border:1px solid #30363d; border-radius:4px; padding:10px; overflow-x:auto; font-size:12px; }
  pre code { background:none; padding:0; font-size:12px; }
  table.cmp { width:100%; border-collapse:collapse; font-size:13px; margin:8px 0; }
  table.cmp th, table.cmp td { padding:6px 10px; border-bottom:1px solid #30363d; text-align:left; vertical-align:top; }
  table.cmp th { background:#161b22; font-weight:600; }
  table.cmp .yes { color:#3fb950; }
  table.cmp .no  { color:#f85149; }
  table.cmp .partial { color:#d29922; }
  .api { margin:8px 0 16px; padding:10px; border-left:3px solid #30363d; background:#0d1117; border-radius:0 4px 4px 0; }
  .api .verb { display:inline-block; padding:2px 6px; border-radius:3px; font-size:11px; font-weight:600; margin-right:6px; }
  .api .verb.GET    { background:#1f6feb; color:white; }
  .api .verb.PUT    { background:#3fb950; color:#0d1117; }
  .api .verb.POST   { background:#bc8cff; color:#0d1117; }
  .api .verb.DELETE { background:#f85149; color:white; }
  .callout { padding:10px 14px; border-radius:4px; margin:12px 0; font-size:13px; }
  .callout.warn { background:#3a2a0d; border:1px solid #d29922; }
  .callout.note { background:#0e2a3a; border:1px solid #1f6feb; }
</style></head><body>
<h1>NATS-KV for Akamai Functions — docs &amp; API guide</h1>
<p class="sub">Globally distributed NATS JetStream KV with HTTP API, fronted by GTM across 27 regions. Built as a research POC to explore what a stronger backing store for Akamai Functions could look like.</p>

<aside class="toc">
  <a href="#intro">1. What is this?</a>
  <a href="#vs-cosmos">2. vs Cosmos (Spin's default)</a>
  <a href="#architecture">3. Architecture (1 page)</a>
  <a href="#quickstart">4. Quickstart</a>
  <a href="#api">5. API reference</a>
  <a href="#sample">6. Sample Spin function</a>
  <a href="#unique">7. NATS-unique features</a>
  <a href="#perf">8. Perf characteristics</a>
  <a href="#throughput">9. Throughput (projected)</a>
  <a href="#caveats">10. Caveats &amp; status</a>
</aside>

<h2 id="intro">1. What is this?</h2>
<p>An HTTP-fronted, globally replicated key-value store designed for Akamai Functions (Fermyon Spin). Each of 27 Linode regions runs an adapter process with embedded NATS JetStream; functions reach the nearest one through GTM at <code>edge.nats-kv.connected-cloud.io</code>. The cluster is a single super-mesh — buckets pick a RAFT geo (NA/EU/AP) and read mirrors are placed in every other region for sub-millisecond local reads.</p>
<p>The point of the experiment: Spin's default <code>spin:key-value</code> WIT only exposes get/set/delete against a managed Cosmos-backed store. That covers the common case, but anything richer — atomic increment, revision history, subject-pattern key wildcards, distributed locks, watches — has to leave the function and go elsewhere. NATS KV exposes all of those primitives natively. We wrapped the cluster in an HTTP adapter so any Spin function (or any HTTP client) can use them today, without waiting for a native <code>key-value-nats</code> Spin factor crate.</p>

<h2 id="vs-cosmos">2. NATS-KV vs Cosmos (Spin's default backend)</h2>
<table class="cmp">
  <thead><tr><th>Capability</th><th>Cosmos (Spin default)</th><th>NATS-KV (this)</th></tr></thead>
  <tbody>
    <tr><td>get / set / delete</td><td class="yes">✓</td><td class="yes">✓</td></tr>
    <tr><td>Atomic increment / counter</td><td class="no">✗ (read-modify-write race)</td><td class="yes">✓ (<code>POST /:key/incr</code>)</td></tr>
    <tr><td>Revision history per key</td><td class="no">✗</td><td class="yes">✓ (configurable depth, default 8)</td></tr>
    <tr><td>Compare-and-swap (CAS)</td><td class="no">✗</td><td class="yes">✓ (<code>If-Match</code> / <code>If-None-Match</code>)</td></tr>
    <tr><td>Subject-pattern key match</td><td class="no">✗ (exact key only)</td><td class="yes">✓ (<code>users.*.session</code>)</td></tr>
    <tr><td>Live watch / SSE</td><td class="no">✗</td><td class="yes">✓ (browser EventSource, server clients)</td></tr>
    <tr><td>Distributed lock recipe</td><td class="no">✗ (no CAS to build it)</td><td class="yes">✓ (CAS + TTL pattern)</td></tr>
    <tr><td>Geographic placement control</td><td class="no">✗ (managed)</td><td class="yes">✓ (R1/R3/R5 with geo:na|eu|ap tags)</td></tr>
    <tr><td>Read locality</td><td class="partial">~ (managed POPs)</td><td class="yes">✓ (per-region mirror at every node)</td></tr>
    <tr><td>Per-tenant isolation</td><td class="yes">✓ (per-app store)</td><td class="yes">✓ (tenant-prefixed buckets)</td></tr>
    <tr><td>Call shape from Spin</td><td>in-process WIT (no network)</td><td>HTTP via wasi-http (one network hop today; native crate possible later)</td></tr>
    <tr><td>Steady-state read latency from FWF (intra-CHI)</td><td>18-22ms</td><td>3-9ms (local mirror) / 25-50ms (cross-region depending on GTM mapping)</td></tr>
    <tr><td><b>Read throughput limit</b></td><td class="no">~1,000 reads/sec (FWF Spin KV gate)</td><td><b>~15-30k reads/sec per region</b> (projected, see §9)</td></tr>
    <tr><td><b>Write throughput limit</b></td><td class="no">~100 writes/sec (FWF Spin KV gate)</td><td><b>~5-15k writes/sec per R3 bucket</b> (projected, see §9)</td></tr>
    <tr><td>Production support / SLA</td><td class="yes">✓ (Akamai-managed)</td><td class="no">✗ (demo-grade — see §10)</td></tr>
  </tbody>
</table>
<p class="meta">The latency advantage flips both ways: NATS-KV is faster than Cosmos when the local mirror is reachable in one hop, and slower when GTM routes the function to a non-local NB (the in-process WIT path Cosmos uses has zero network cost).</p>

<h2 id="architecture">3. Architecture (one page)</h2>
<ul>
  <li><b>27 regions, each running one adapter</b> = a Go binary embedding <code>nats-server</code> with JetStream + a small HTTP API. Single super-cluster mesh (one NATS account globally).</li>
  <li><b>Buckets</b>: NATS JetStream KV. Tier choices: <b>R1</b> (single replica, max throughput), <b>R3</b> (RAFT, durable, ~30ms quorum write within a geo), <b>R5</b> (RAFT spread across more peers in the geo).</li>
  <li><b>Read mirrors</b>: every bucket auto-creates a one-replica mirror in every region (27 per bucket). Adapter reads always hit the mirror local to the node serving the request → sub-ms.</li>
  <li><b>Placement engine</b>: when you pick <code>geo: auto</code>, the control plane queries <code>project-latency</code> for the live RTT matrix, scores each geo's median quorum edge from your anchor, and picks the winner. Decision is returned in the create response and rendered in the dashboard.</li>
  <li><b>GTM</b>: <code>edge.nats-kv.connected-cloud.io</code> resolves to the closest healthy adapter per Akamai's perf scoring. (Note: ECS isn't on for this domain yet — perf routing falls back to per-resolver mapping. Affects which leaf FWF lands on.)</li>
  <li><b>Auth</b>: bearer token in <code>Authorization: Bearer &lt;key&gt;</code>. Tenants are minted by an admin app (invite → claim flow).</li>
</ul>
<p>For the live picture see <a href="/topology">/topology</a>.</p>

<h2 id="quickstart">4. Quickstart</h2>
<ol>
  <li>Open <a href="/">the home page</a> and either claim an invite link (you'll get a key) or use the open <code>akv_demo_open</code> token for the shared <code>demo</code> bucket.</li>
  <li>Set <code>Authorization: Bearer &lt;key&gt;</code> on every request.</li>
  <li>Hit the GTM endpoint:
    <pre><code>curl -H "Authorization: Bearer akv_demo_open" \
     https://edge.nats-kv.connected-cloud.io/v1/kv/demo/hello

curl -X PUT -H "Authorization: Bearer akv_demo_open" \
     --data 'world' \
     https://edge.nats-kv.connected-cloud.io/v1/kv/demo/hello</code></pre>
  </li>
  <li>To create your own bucket (after claiming a tenant key), POST to the control plane:
    <pre><code>curl -X POST -H "Authorization: Bearer $YOUR_KEY" \
     -H "Content-Type: application/json" \
     -d '{"name":"sessions","replicas":3,"geo":"auto","anchor":"us-ord"}' \
     https://cp.nats-kv.connected-cloud.io/v1/me/buckets</code></pre>
    The response includes the placement <b>Decision</b> showing which geo was picked and why.</li>
</ol>

<h2 id="api">5. API reference</h2>
<p>Two services, both bearer-auth'd:</p>
<ul>
  <li><b>Data plane</b> (adapter, GTM-routed): <code>https://edge.nats-kv.connected-cloud.io/v1/...</code> — bucket reads/writes/history/incr.</li>
  <li><b>Control plane</b> (LZ-only): <code>https://cp.nats-kv.connected-cloud.io/v1/...</code> — tenants, buckets, placement preview.</li>
</ul>

<h3>Data-plane endpoints</h3>

<div class="api"><span class="verb GET">GET</span><code>/v1/kv/:bucket/:key</code>
  <p>Read the latest value. Response body is the raw value bytes; <code>X-Revision</code>, <code>X-Read-Source</code> (<code>local-mirror</code> or <code>source</code>), <code>X-Read-Stream</code>, <code>X-Latency-Ms</code> headers describe how it was served. Add <code>?revision=&lt;n&gt;</code> for a specific historical revision.</p>
</div>

<div class="api"><span class="verb PUT">PUT</span><code>/v1/kv/:bucket/:key</code>
  <p>Write. Body is the value. Optional <code>If-Match: &lt;revision&gt;</code> for compare-and-swap (returns 412 if mismatch); <code>If-None-Match: *</code> for create-if-absent.</p>
  <p>Response: <code>{"revision":&lt;n&gt;}</code>.</p>
</div>

<div class="api"><span class="verb DELETE">DELETE</span><code>/v1/kv/:bucket/:key</code>
  <p>Tombstone the key. Optional <code>If-Match</code> for CAS delete. Subsequent GET returns 404.</p>
</div>

<div class="api"><span class="verb GET">GET</span><code>/v1/kv/:bucket/:key/history</code>
  <p>All revisions still in history (default depth = 8). Returns JSON: <code>[{"revision":1,"value_b64":"..."}, ...]</code>.</p>
</div>

<div class="api"><span class="verb POST">POST</span><code>/v1/kv/:bucket/:key/incr</code>
  <p>Atomic counter. Body: <code>{"by":1}</code>. Returns the new value. Treats missing key as 0. NATS-side it's a CAS loop on the underlying revision so it's safe under concurrent writers.</p>
</div>

<div class="api"><span class="verb GET">GET</span><code>/v1/kv/:bucket/keys?match=&lt;subject-pattern&gt;</code>
  <p>List keys matching a NATS subject pattern. <code>users.*.session</code>, <code>users.&gt;</code> (everything under <code>users</code>), etc. Returns <code>{"keys":[...]}</code>.</p>
  <p class="meta">This is the headline NATS-only feature — subject wildcards work because NATS KV stores entries on <code>$KV.&lt;bucket&gt;.&lt;key&gt;</code> subjects.</p>
</div>

<div class="api"><span class="verb GET">GET</span><code>/v1/admin/buckets</code>
  <p>List every bucket on the cluster with placement, peers, and mirror count. Used by the topology page.</p>
</div>

<div class="api"><span class="verb GET">GET</span><code>/v1/admin/cluster</code>
  <p>Returns <code>{"region":"us-ord","server":"kv-us-ord","local_mirrors":{...}}</code> — useful in functions to know which leaf you landed on.</p>
</div>

<div class="api"><span class="verb GET">GET</span><code>/v1/health</code>
  <p>Liveness probe. Returns <code>{"status":"ok","region":"..."}</code>. Open (no auth).</p>
</div>

<h3>Control-plane endpoints</h3>

<div class="api"><span class="verb GET">GET</span><code>/v1/me</code>
  <p>Returns the calling tenant's identity, quotas, and endpoint hints. Bearer = user key.</p>
</div>

<div class="api"><span class="verb GET">GET</span><code>/v1/me/buckets</code>
  <p>List the calling tenant's buckets with enriched details (replicas, leader, peers, placement_tags, mirror_count).</p>
</div>

<div class="api"><span class="verb POST">POST</span><code>/v1/me/buckets</code>
  <p>Create a bucket. Body:</p>
  <pre><code>{
  "name": "sessions",            // tenant prefix added automatically
  "replicas": 3,                 // 1 | 3 | 5
  "geo": "auto",                 // auto | anchor:&lt;region&gt; | na | eu | ap | sa
  "anchor": "us-ord",            // hint for "auto" mode (default us-ord)
  "history": 8,                  // revisions to keep per key
  "no_mirrors": false            // opt-out of mirrors-everywhere
}</code></pre>
  <p>Response includes a <code>placement</code> Decision: chosen geo, predicted regions, actual regions NATS picked, expected write latency, runner-up alternatives.</p>
</div>

<div class="api"><span class="verb GET">GET</span><code>/v1/placement/preview?anchor=&amp;replicas=&amp;mode=</code>
  <p>Run the placement engine without creating a bucket. Lets the dashboard preview the decision before you commit.</p>
</div>

<h2 id="sample">6. Sample function code</h2>
<p>Three idioms below — Rust (Spin function, the production deployment target), TypeScript (browser/Node fetch — Spin's JS SDK works the same), and Python (scripts, batch jobs, integration tests).</p>
<h3>Rust (Spin function)</h3>
<p>A Spin <code>wasi-http</code> handler that uses NATS-KV as a session store with subject-pattern lookup — something Cosmos can't express:</p>

<pre><code>use spin_sdk::http::{IntoResponse, Method, Request, Response, send};
use spin_sdk::http_component;

const KV_BASE: &amp;str = "http://edge.nats-kv.connected-cloud.io:8080";
// Token from your tenant claim flow. In production, inject via Spin variables.
const KV_KEY:  &amp;str = "akv_int_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
const BUCKET:  &amp;str = "your_tenant__sessions";

#[http_component]
async fn handle_request(req: Request) -&gt; anyhow::Result&lt;impl IntoResponse&gt; {
    let user = req.header("x-user-id").and_then(|v| v.as_str())
        .unwrap_or("anonymous").to_string();
    let id = uuid_v4_or_whatever();

    // Write a session with subject-style key — this enables wildcard query later.
    let put = Request::builder()
        .method(Method::Put)
        .uri(format!("{KV_BASE}/v1/kv/{BUCKET}/users.{user}.{id}.session"))
        .header("Authorization", format!("Bearer {KV_KEY}"))
        .body(b"active".to_vec())
        .build();
    let _: Response = send(put).await?;

    // List all active sessions for this user (impossible in Cosmos).
    let q = Request::builder()
        .method(Method::Get)
        .uri(format!("{KV_BASE}/v1/kv/{BUCKET}/keys?match=users.{user}.*.session"))
        .header("Authorization", format!("Bearer {KV_KEY}"))
        .body(Vec::&lt;u8&gt;::new())
        .build();
    let resp: Response = send(q).await?;

    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(resp.into_body())
        .build())
}</code></pre>

<p>And the same idea but using <b>atomic increment</b> as a request counter — a NATS-only primitive:</p>

<pre><code>let req = Request::builder()
    .method(Method::Post)
    .uri(format!("{KV_BASE}/v1/kv/{BUCKET}/req-counter.{path}/incr"))
    .header("Authorization", format!("Bearer {KV_KEY}"))
    .header("content-type", "application/json")
    .body(br#"{"by":1}"#.to_vec())
    .build();
let resp: Response = send(req).await?;
// resp body = the new counter value, atomic across all 27 regions.</code></pre>

<h3>TypeScript (browser, Node, or Spin JS SDK)</h3>
<p>The same session-store + wildcard-query pattern. Works unchanged in the browser (CORS is open on the data plane), in Node, and in a Spin JS function (the JS SDK exposes <code>fetch</code> through <code>wasi:http</code>):</p>

<pre><code>const KV_BASE = "https://edge.nats-kv.connected-cloud.io";
const KV_KEY  = process.env.KV_KEY;            // or Spin variable
const BUCKET  = "your_tenant__sessions";

async function recordSession(userId: string, sessionId: string) {
  // Subject-pattern keys enable wildcard query later
  const r = await fetch(`${KV_BASE}/v1/kv/${BUCKET}/users.${userId}.${sessionId}.session`, {
    method: "PUT",
    headers: { "Authorization": `Bearer ${KV_KEY}`, "Content-Type": "text/plain" },
    body: "active",
  });
  if (!r.ok) throw new Error(`PUT failed: ${r.status}`);
  const { revision } = await r.json();
  return revision;
}

async function listUserSessions(userId: string): Promise&lt;string[]&gt; {
  // Cosmos can't do this — it's exact-key only.
  const r = await fetch(`${KV_BASE}/v1/kv/${BUCKET}/keys?match=users.${userId}.*.session`, {
    headers: { "Authorization": `Bearer ${KV_KEY}` },
  });
  const { keys } = await r.json();
  return keys ?? [];
}

// Atomic counter — safe under concurrent writers, no read-modify-write race.
async function bumpCounter(name: string): Promise&lt;number&gt; {
  const r = await fetch(`${KV_BASE}/v1/kv/${BUCKET}/counters.${name}/incr`, {
    method: "POST",
    headers: { "Authorization": `Bearer ${KV_KEY}`, "Content-Type": "application/json" },
    body: JSON.stringify({ by: 1 }),
  });
  return Number(await r.text());
}

// Compare-and-swap: only update if the revision matches what we read.
async function casUpdate(key: string, expectRev: number, newValue: string) {
  const r = await fetch(`${KV_BASE}/v1/kv/${BUCKET}/${key}`, {
    method: "PUT",
    headers: {
      "Authorization": `Bearer ${KV_KEY}`,
      "Content-Type": "text/plain",
      "If-Match": String(expectRev),
    },
    body: newValue,
  });
  if (r.status === 412) throw new Error("CAS conflict — somebody else updated first");
  if (!r.ok) throw new Error(`PUT failed: ${r.status}`);
}</code></pre>

<h3>Python (scripts, batch jobs, integration tests)</h3>
<p>Plain <code>requests</code>. Useful for backfills, audit scrapers, or wiring NATS-KV into a Python data pipeline:</p>

<pre><code>import os, requests

KV_BASE = "https://edge.nats-kv.connected-cloud.io"
KV_KEY  = os.environ["KV_KEY"]
BUCKET  = "your_tenant__sessions"
H = {"Authorization": f"Bearer {KV_KEY}"}

def put(key, value):
    r = requests.put(f"{KV_BASE}/v1/kv/{BUCKET}/{key}", headers=H, data=value)
    r.raise_for_status()
    return r.json()["revision"]

def get(key):
    r = requests.get(f"{KV_BASE}/v1/kv/{BUCKET}/{key}", headers=H)
    if r.status_code == 404: return None
    r.raise_for_status()
    return r.content

def history(key):
    """All retained revisions of a key (default depth = 8)."""
    r = requests.get(f"{KV_BASE}/v1/kv/{BUCKET}/{key}/history", headers=H)
    r.raise_for_status()
    return r.json()  # [{"revision":N, "value_b64":"...", "created":"..."}, ...]

def list_keys(pattern):
    """Subject-pattern wildcard query — e.g., 'users.*.session'."""
    r = requests.get(f"{KV_BASE}/v1/kv/{BUCKET}/keys",
                     headers=H, params={"match": pattern})
    r.raise_for_status()
    return r.json()["keys"]

def incr(key, by=1):
    """Atomic counter — safe under concurrent writers."""
    r = requests.post(f"{KV_BASE}/v1/kv/{BUCKET}/{key}/incr",
                      headers={**H, "Content-Type": "application/json"},
                      json={"by": by})
    r.raise_for_status()
    return int(r.text)

# Example: bulk-tag every user that had a session in the last hour.
recent = list_keys("users.*.session")
print(f"{len(recent)} active sessions")
for k in recent:
    user = k.split(".")[1]
    print(f"  {user} -> rev {put(f'tags.{user}.last_seen', 'recent')}")
</code></pre>

<div class="callout note">
  <b>Spin runtime tip</b>: Spin's HTTP client (<code>wasi:http/outgoing-handler</code>) pools connections per function instance. The first call from a cold instance pays a TCP+TLS setup (~50-200ms); subsequent calls reuse the connection (~5-10ms). For latency-sensitive paths, do a no-op warmup at instance init. The same applies in reverse for the browser TS sample — keep-alive is per-tab.
</div>

<h2 id="unique">7. NATS-unique features (vs the Spin KV WIT)</h2>
<ul>
  <li><b>Atomic increment</b> — a CAS loop on the value's revision; safe under N concurrent writers without read-modify-write races.</li>
  <li><b>Revision history</b> — last 8 versions per key by default, queryable individually. Good for "undo last write" or audit.</li>
  <li><b>Subject-pattern wildcards</b> — keys live on NATS subjects, so <code>match=users.*.session</code> or <code>match=foo.&gt;</code> work natively. No table scan.</li>
  <li><b>Compare-and-swap</b> (<code>If-Match: &lt;rev&gt;</code>) — the building block for distributed locks, leader election, idempotency keys, rate limiters.</li>
  <li><b>Geographic placement choice</b> — pick the geo (and survive geo-level failures with R5 spread).</li>
  <li><b>Per-region mirror reads</b> — every region has a local mirror by default; reads are sub-ms.</li>
  <li><b>Live watch via SSE</b> — long-lived browser/server clients see changes without polling. (Not used inside functions because Spin's request-response shape doesn't fit long-lived watches.)</li>
</ul>

<h2 id="perf">8. Perf characteristics (steady-state from FWF, Chicago)</h2>
<table class="cmp">
  <thead><tr><th>Path</th><th>Read (ms)</th><th>Write (ms)</th></tr></thead>
  <tbody>
    <tr><td>NATS R1, GTM lands on us-ord</td><td>3-4</td><td>3-4</td></tr>
    <tr><td>NATS R3, GTM lands on us-ord, leader=us-ord</td><td>3-4</td><td>~33 (quorum cost)</td></tr>
    <tr><td>NATS R3, GTM lands on ca-central, leader=us-ord</td><td>~25 (FWF↔ca-central)</td><td>~50 (FWF↔ca-central + cross-region forward)</td></tr>
    <tr><td>Cosmos via spin:key-value WIT</td><td>18-22</td><td>22-25</td></tr>
  </tbody>
</table>
<p class="meta">Numbers are <code>upstream_us</code> from the playground (FWF Spin function ⇄ adapter, excluding browser↔FWF). NATS-KV beats Cosmos when GTM lands the function on the same region as the bucket's RAFT leader. The FWF↔adapter network hop is the floor — would close to zero with a native <code>key-value-nats</code> Spin factor crate (in-process WIT call instead of HTTP).</p>

<h2 id="throughput">9. Throughput estimates <span class="meta" style="font-weight:normal;font-size:13px">— projected, NOT load-tested</span></h2>
<div class="callout warn">
  <b>These numbers are architecture-derived ceilings, not measured.</b> Per the project's scale-honesty rule, treat them as <i>"designed for"</i> not <i>"supports"</i>. The <a href="/loadtest">/loadtest</a> page lets you drive real traffic against any bucket and produce defensible measurements you can substitute in here.
</div>

<h3>Per-node ceiling (Linode <code>g8-dedicated-4-2</code>: 4 vCPU, 8GB RAM, ~4-6 Gbps sustained NIC, block-volume storage)</h3>
<table class="cmp">
  <thead><tr><th>Path</th><th>Projected ceiling</th><th>Bottleneck</th></tr></thead>
  <tbody>
    <tr><td>Reads (local mirror, direct-get)</td><td><b>~15-30k reads/sec/node</b></td><td>HTTP/wasi-http parse + 4-core CPU; for very cold keys, block-volume read IOPS</td></tr>
    <tr><td>Writes — R1 source on this node</td><td><b>~20-30k writes/sec</b></td><td>WAL fsync (NATS batches well); HTTP adapter overhead</td></tr>
    <tr><td>Writes — R3 leader on this node</td><td><b>~5-15k writes/sec/bucket</b></td><td>RAFT quorum wait (intra-geo ~30ms RTT) is the typical cap, not network</td></tr>
    <tr><td>Writes — R5 leader on this node</td><td><b>~3-10k writes/sec/bucket</b></td><td>Same as R3, plus the third-fastest peer's RTT in the quorum path</td></tr>
  </tbody>
</table>

<h3>Mirror fan-out write amplification (revised: Linode NICs sustain 4-6 Gbps, not 1)</h3>
<p>Every write to an R3 source replicates to <b>24 mirrors</b>. Source's NIC budget is consumed by the fan-out — but Linode VMs sustain ~5 Gbps, so the fan-out tax is much less binding than the typical "1 Gbps NIC" assumption suggests:</p>
<table class="cmp">
  <thead><tr><th>Value size</th><th>Egress per write (24 mirrors)</th><th>NIC-bound ceiling per source (5 Gbps)</th><th>Effective ceiling (min of NIC, RAFT, IOPS)</th></tr></thead>
  <tbody>
    <tr><td>100 B</td><td>2.4 KB</td><td>~260,000 writes/sec</td><td><b>~10-15k</b> (RAFT-bound)</td></tr>
    <tr><td>1 KB</td><td>24 KB</td><td>~26,000 writes/sec</td><td><b>~10-15k</b> (RAFT-bound)</td></tr>
    <tr><td>10 KB</td><td>240 KB</td><td>~2,600 writes/sec</td><td><b>~2,500</b> (NIC-bound)</td></tr>
    <tr><td>100 KB</td><td>2.4 MB</td><td>~260 writes/sec</td><td><b>~260</b> (NIC-bound)</td></tr>
  </tbody>
</table>
<p class="meta">For typical small-payload workloads (≤ a few KB) the binding constraint is <b>RAFT quorum cost</b>, not network — that flips at ~10 KB and above where NIC fan-out dominates. <code>no_mirrors=true</code> on bucket creation removes the fan-out entirely (at the cost of losing local-mirror reads everywhere). Earlier draft of this table assumed a 1 Gbps NIC and undercounted by ~5×.</p>

<h3>Aggregate cluster throughput</h3>
<table class="cmp">
  <thead><tr><th>Workload</th><th>Aggregate projection</th><th>Why</th></tr></thead>
  <tbody>
    <tr><td>Globally distributed reads (every region serves its locals from local mirror)</td><td><b>~400k-800k reads/sec total</b></td><td>27 nodes × 15-30k each, no contention — reads are fully parallel.</td></tr>
    <tr><td>Writes, 3 R3 buckets one per geo (NA/EU/AP), 1 KB values</td><td><b>~15-45k writes/sec total</b></td><td>Each bucket's leader caps around 5-15k; three independent geos sum.</td></tr>
    <tr><td>Writes, 10 R3 buckets evenly spread across 3 geos, 1 KB values</td><td><b>~30-80k writes/sec total</b></td><td>Each bucket = own RAFT leader, own throughput unit. NIC headroom (~26k/source at 1KB) leaves room to stack many sources per region.</td></tr>
    <tr><td>Writes, single R1 bucket, 1 KB values, no mirrors</td><td><b>~20-30k writes/sec</b></td><td>One node's WAL fsync rate.</td></tr>
  </tbody>
</table>

<div class="callout note">
  <b>For sales-grade numbers, run <a href="/loadtest">/loadtest</a></b> against your scenario and replace the projections above. The four scenarios most worth measuring: (1) single-region read throughput from one client, (2) R3 writes with <code>no_mirrors=true</code> isolating RAFT cost, (3) R3 writes with mirrors-everywhere measuring the mirror tax directly, (4) value-size sweep at fixed concurrency.
</div>

<h2 id="caveats">10. Caveats &amp; status</h2>
<div class="callout warn">
  <b>This is a research POC, not a production service.</b> Built to explore what a richer KV substrate for Akamai Functions could look like, what the placement engineering tradeoffs feel like in practice, and how much performance is leaving the table when functions go through a network hop instead of in-process WIT. Treat numbers as illustrative.
</div>
<ul>
  <li><b>No SLA, no support, no on-call.</b> Demo cluster runs on the presales account. Buckets and tenants can be wiped without notice (and have been — see project DECISIONS.md ADR-022).</li>
  <li><b>Auth is bearer-token only.</b> No EAA, no per-key ACL granularity, no quotas enforcement beyond storage exhaustion.</li>
  <li><b>Storage tier is single block volume per region.</b> If a Linode block volume goes away, R1 buckets on that node are gone. R3/R5 survive single-node loss.</li>
  <li><b>Spin's HTTP path is the latency floor.</b> Until a native <code>key-value-nats</code> Spin factor crate exists, every function call to NATS-KV pays one wasi-http hop (~5-10ms steady state to the local NB, more if GTM routes elsewhere).</li>
  <li><b>GTM ECS is currently off on the shared domain</b> — affects which leaf functions land on. Backend request open to enable it.</li>
  <li><b>Watches via SSE</b> work for browser/long-lived clients but aren't usable from inside functions (Spin's request-response shape).</li>
  <li><b>What this is meant to inform</b>: should Akamai consider exposing a richer KV WIT to function authors? Should the Cosmos backend grow CAS / counters / wildcards? Is there appetite for a NATS-class store as an alternative backend behind <code>spin:key-value</code>? This demo gives concrete data to answer those.</li>
</ul>

<p class="meta" style="margin-top:32px;">Source: <a href="https://github.com/ccie7599/nats-kv">github.com/ccie7599/nats-kv</a> · SCOPE.md and 22 ADRs in DECISIONS.md document every architecture choice, with the misses documented honestly.</p>

<script>__NAV_JS__
renderNav('docs');
</script>
</body></html>
"##;

// =====================================================================
// /loadtest — browser-driven throughput probe.
// Drives concurrent fetches against a chosen bucket, reports ops/sec,
// p50/p95/p99 latency, error count. Two modes:
//   • "via FWF" — what a Spin function would see (rate-limited by FWF
//     instance fan-out; each call goes through wasi-http to the adapter).
//   • "direct browser → adapter" — bypasses FWF entirely; CORS to the
//     GTM-routed leaf. Ceiling is browser ↔ adapter network plus the
//     adapter's per-node throughput limit (no Spin overhead).
// =====================================================================
const LOADTEST_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV — load test</title><style>__SHARED_CSS__
  .grid { display:grid; grid-template-columns:repeat(auto-fill,minmax(200px,1fr)); gap:8px; }
  .stat-box { padding:10px; border:1px solid #30363d; border-radius:4px; background:#0d1117; }
  .stat-box .label { color:#8b949e; font-size:11px; text-transform:uppercase; letter-spacing:0.4px; }
  .stat-box .value { font-size:24px; font-weight:600; margin-top:2px; }
  .stat-box .value.ok { color:#3fb950; }
  .stat-box .value.warn { color:#d29922; }
  .stat-box .value.err { color:#f85149; }
  .progress { height:6px; background:#21262d; border-radius:3px; overflow:hidden; margin-top:8px; }
  .progress > div { height:100%; background:#58a6ff; transition:width 0.2s; }
  table.run { width:100%; border-collapse:collapse; font-size:12px; margin-top:12px; }
  table.run th, table.run td { padding:4px 8px; border-bottom:1px solid #30363d; text-align:right; }
  table.run th { background:#161b22; text-align:right; }
  table.run td:first-child, table.run th:first-child { text-align:left; }
</style></head><body>
<h1>Load test — drive throughput, get measured numbers</h1>
<p class="sub">Browser-side concurrent fetch loop. Replaces the projected throughput in <a href="/docs#throughput">/docs §9</a> with measured numbers you can defend.</p>

<div class="callout note" style="padding:10px; border-radius:4px; margin:12px 0; background:#0e2a3a; border:1px solid #1f6feb; font-size:13px;">
  <b>What you're measuring:</b> in <i>FWF</i> mode, each fetch hits the user-app's Spin function which proxies to the adapter — exactly what a function-author's NATS-KV calls look like. In <i>direct</i> mode, the browser hits the adapter through GTM directly, bypassing FWF entirely. The gap between the two is the cost of the function-side wasi-http hop.
</div>

<fieldset>
  <legend>Configure</legend>
  <div class="row">
    <div><label>bucket</label><select id="lt-bucket"></select></div>
    <div><label>operation</label><select id="lt-op">
      <option value="GET">GET</option>
      <option value="PUT">PUT</option>
      <option value="MIX">MIX (90% GET / 10% PUT)</option>
    </select></div>
    <div><label>path</label><select id="lt-path">
      <option value="fwf">via FWF (Spin function)</option>
      <option value="direct">direct browser → adapter (CORS)</option>
    </select></div>
  </div>
  <div class="row">
    <div><label>concurrency</label><input id="lt-conc" type="number" value="10" min="1" max="200"></div>
    <div><label>duration (sec)</label><input id="lt-dur" type="number" value="10" min="1" max="120"></div>
    <div><label>value size (bytes, PUTs)</label><select id="lt-vsize">
      <option value="100">100 B</option>
      <option value="1024" selected>1 KB</option>
      <option value="10240">10 KB</option>
    </select></div>
    <div><label>keyspace</label><select id="lt-keyspace">
      <option value="1">single key (worst case for cache contention)</option>
      <option value="100" selected>100 keys</option>
      <option value="10000">10,000 keys</option>
    </select></div>
  </div>
  <div class="actions">
    <button onclick="runLoad()" id="lt-run">Run</button>
    <button onclick="stopLoad()" id="lt-stop" class="secondary" disabled>Stop</button>
    <span id="lt-status" class="meta" style="margin-left:12px"></span>
  </div>
  <div class="progress"><div id="lt-progress" style="width:0%"></div></div>
</fieldset>

<fieldset>
  <legend>Live results</legend>
  <div class="grid">
    <div class="stat-box"><div class="label">Throughput</div><div class="value" id="lt-rps">—</div><div class="label" style="margin-top:4px">ops / sec</div></div>
    <div class="stat-box"><div class="label">Total ops</div><div class="value" id="lt-total">0</div></div>
    <div class="stat-box"><div class="label">Errors</div><div class="value" id="lt-errs">0</div></div>
    <div class="stat-box"><div class="label">p50 latency</div><div class="value" id="lt-p50">—</div><div class="label" style="margin-top:4px">ms</div></div>
    <div class="stat-box"><div class="label">p95 latency</div><div class="value" id="lt-p95">—</div><div class="label" style="margin-top:4px">ms</div></div>
    <div class="stat-box"><div class="label">p99 latency</div><div class="value" id="lt-p99">—</div><div class="label" style="margin-top:4px">ms</div></div>
  </div>
  <table class="run">
    <thead><tr><th>elapsed</th><th>ops/sec</th><th>p50 ms</th><th>p95 ms</th><th>p99 ms</th><th>errs</th></tr></thead>
    <tbody id="lt-tick-body"></tbody>
  </table>
</fieldset>

<fieldset>
  <legend>History (this session)</legend>
  <table class="run" id="lt-history">
    <thead><tr><th>when</th><th>bucket</th><th>op</th><th>path</th><th>conc</th><th>dur</th><th>ops/sec</th><th>p50</th><th>p95</th><th>p99</th><th>errs</th></tr></thead>
    <tbody></tbody>
  </table>
</fieldset>

<script>__NAV_JS__
renderNav('loadtest');
const $ = (id) => document.getElementById(id);

const ALL_REGIONS = [
  "us-ord","us-east","us-central","us-west","us-southeast","us-lax","us-mia","us-sea","ca-central","br-gru",
  "gb-lon","eu-central","de-fra-2","fr-par-2","nl-ams","se-sto","it-mil",
  "ap-south","sg-sin-2","ap-northeast","jp-tyo-3","jp-osa","ap-west","in-bom-2","in-maa","id-cgk","ap-southeast",
];

const ADAPTER_DIRECT_URL = "https://edge.nats-kv.connected-cloud.io";

async function loadBuckets() {
  const sel = $("lt-bucket");
  const opts = [{name:"demo", label:"demo (shared, R3 NA, 27 mirrors)"}];
  if (userKey()) {
    try {
      const r = await fetch("/api/control/v1/me/buckets", { headers: {"Authorization": "Bearer " + userKey()} });
      if (r.ok) {
        const j = await r.json();
        for (const d of (j.details||[])) {
          const short = d.name.includes("__") ? d.name.split("__").slice(1).join("__") : d.name;
          opts.push({name: d.name, label: `${short} (R${d.replicas||1}, ${d.mirror_count||0} mirrors)`});
        }
      }
    } catch (e) {}
  }
  sel.innerHTML = opts.map(o => `<option value="${o.name}">${o.label}</option>`).join("");
}
loadBuckets();

let running = false;
let stopRequested = false;

function p(arr, q) {
  if (arr.length === 0) return 0;
  const sorted = [...arr].sort((a,b)=>a-b);
  return sorted[Math.min(sorted.length-1, Math.floor(sorted.length * q))];
}

async function runLoad() {
  if (running) return;
  const bucket = $("lt-bucket").value;
  const op = $("lt-op").value;
  const conc = Math.max(1, parseInt($("lt-conc").value, 10));
  const durMs = Math.max(1000, parseInt($("lt-dur").value, 10) * 1000);
  const vsize = parseInt($("lt-vsize").value, 10);
  const keyspace = parseInt($("lt-keyspace").value, 10);
  const path = $("lt-path").value;
  const valueBlob = "x".repeat(vsize);
  const key = (i) => keyspace === 1 ? "lt-key" : `lt-${i % keyspace}`;
  const buildUrl = (k) => path === "fwf"
    ? `/api/nats/v1/kv/${encodeURIComponent(bucket)}/${encodeURIComponent(k)}`
    : `${ADAPTER_DIRECT_URL}/v1/kv/${encodeURIComponent(bucket)}/${encodeURIComponent(k)}`;
  const buildHeaders = () => {
    const h = {};
    if (path === "fwf") {
      if (userKey()) h["X-KV-Key"] = userKey();
    } else {
      h["Authorization"] = "Bearer " + (userKey() || "akv_demo_open");
    }
    return h;
  };

  // reset UI
  running = true; stopRequested = false;
  $("lt-run").disabled = true; $("lt-stop").disabled = false;
  $("lt-status").textContent = `running ${op} for ${durMs/1000}s @ concurrency ${conc} via ${path}…`;
  $("lt-tick-body").innerHTML = "";
  $("lt-rps").textContent = "—"; $("lt-total").textContent = 0; $("lt-errs").textContent = 0;
  $("lt-p50").textContent = "—"; $("lt-p95").textContent = "—"; $("lt-p99").textContent = "—";

  const t0 = performance.now();
  const deadline = t0 + durMs;
  let total = 0, errs = 0, opCounter = 0;
  const lats = [];               // all latencies (full run)
  const tickLats = [];           // per-tick latencies (cleared each tick)
  let lastTick = t0, lastTickTotal = 0;

  // Worker fires fetches as fast as it can until deadline.
  async function worker(workerId) {
    while (performance.now() < deadline && !stopRequested) {
      const i = opCounter++;
      const k = key(i);
      const isPut = op === "PUT" || (op === "MIX" && (i % 10 === 0));
      const opts = { method: isPut ? "PUT" : "GET", headers: buildHeaders() };
      if (isPut) opts.body = valueBlob;
      const t1 = performance.now();
      try {
        const r = await fetch(buildUrl(k), opts);
        if (path === "fwf") {
          // FWF wraps response; status inside envelope.
          const j = await r.json();
          if (j.status >= 400 && j.status !== 404) errs++;
        } else {
          // Direct adapter: 200/204/404 are all fine.
          if (!r.ok && r.status !== 404) errs++;
          await r.arrayBuffer(); // drain so connection can be reused
        }
        const dt = performance.now() - t1;
        lats.push(dt);
        tickLats.push(dt);
        total++;
      } catch (e) { errs++; }
    }
  }

  // Stats ticker: updates UI every 500ms
  const tickH = setInterval(() => {
    const now = performance.now();
    const elapsed = (now - t0) / 1000;
    const tickElapsed = (now - lastTick) / 1000;
    const tickOps = total - lastTickTotal;
    const tickRps = tickElapsed > 0 ? tickOps / tickElapsed : 0;
    $("lt-rps").textContent = tickRps.toFixed(0);
    $("lt-total").textContent = total;
    $("lt-errs").textContent = errs;
    if (lats.length) {
      $("lt-p50").textContent = p(lats, 0.50).toFixed(1);
      $("lt-p95").textContent = p(lats, 0.95).toFixed(1);
      $("lt-p99").textContent = p(lats, 0.99).toFixed(1);
    }
    $("lt-progress").style.width = Math.min(100, elapsed * 1000 / durMs * 100).toFixed(1) + "%";
    if (tickLats.length) {
      const tb = $("lt-tick-body");
      const row = document.createElement("tr");
      row.innerHTML = `<td>${elapsed.toFixed(1)}s</td><td>${tickRps.toFixed(0)}</td><td>${p(tickLats,0.5).toFixed(1)}</td><td>${p(tickLats,0.95).toFixed(1)}</td><td>${p(tickLats,0.99).toFixed(1)}</td><td>${errs}</td>`;
      tb.appendChild(row);
      while (tb.children.length > 30) tb.removeChild(tb.firstChild);
    }
    tickLats.length = 0;
    lastTick = now; lastTickTotal = total;
  }, 500);

  // Spin up workers in parallel
  const workers = [];
  for (let w = 0; w < conc; w++) workers.push(worker(w));
  await Promise.all(workers);
  clearInterval(tickH);

  const totalElapsed = (performance.now() - t0) / 1000;
  const finalRps = total / totalElapsed;
  $("lt-rps").textContent = finalRps.toFixed(0);
  $("lt-total").textContent = total;
  $("lt-errs").textContent = errs;
  if (lats.length) {
    $("lt-p50").textContent = p(lats, 0.50).toFixed(1);
    $("lt-p95").textContent = p(lats, 0.95).toFixed(1);
    $("lt-p99").textContent = p(lats, 0.99).toFixed(1);
  }
  $("lt-status").textContent = `done — ${total} ops in ${totalElapsed.toFixed(1)}s @ ${finalRps.toFixed(0)} ops/sec (${errs} errors)`;
  $("lt-run").disabled = false; $("lt-stop").disabled = true;
  running = false;

  // history row
  const histBody = $("lt-history").querySelector("tbody");
  const histRow = document.createElement("tr");
  histRow.innerHTML = `<td>${new Date().toLocaleTimeString()}</td><td>${bucket}</td><td>${op}</td><td>${path}</td><td>${conc}</td><td>${(durMs/1000).toFixed(0)}s</td><td>${finalRps.toFixed(0)}</td><td>${lats.length?p(lats,0.5).toFixed(1):'—'}</td><td>${lats.length?p(lats,0.95).toFixed(1):'—'}</td><td>${lats.length?p(lats,0.99).toFixed(1):'—'}</td><td>${errs}</td>`;
  histBody.insertBefore(histRow, histBody.firstChild);
}

function stopLoad() {
  stopRequested = true;
  $("lt-status").textContent = "stop requested…";
}
</script>
</body></html>
"##;

// =====================================================================
// /api-explorer — Swagger UI loaded from CDN, pointed at /openapi.yaml.
// Lets users explore + execute every endpoint interactively without
// crafting curl by hand.
// =====================================================================
const API_EXPLORER_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV — API explorer</title>
<link rel="stylesheet" type="text/css" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui.css">
<style>__SHARED_CSS__
  /* ---- Dark-theme overrides for Swagger UI 5 ---- */
  /* Site palette: bg #0d1117, panel #161b22, border #30363d, text #c9d1d9, muted #8b949e, accent #58a6ff. */
  /* Verb colours preserved (GET blue, POST purple, PUT green, DELETE red) per /docs convention. */
  #swagger-ui { background:#0d1117; border:1px solid #30363d; border-radius:6px; min-height:400px; padding:8px 0; }
  .swagger-ui, .swagger-ui .info, .swagger-ui .info p, .swagger-ui .info li,
  .swagger-ui .info table, .swagger-ui .opblock-tag, .swagger-ui .opblock .opblock-summary-path,
  .swagger-ui .opblock .opblock-summary-description, .swagger-ui .opblock-description-wrapper p,
  .swagger-ui .opblock-external-docs-wrapper, .swagger-ui .opblock-section-header h4,
  .swagger-ui .opblock-section-header > label, .swagger-ui .opblock-section-header,
  .swagger-ui .response-col_status, .swagger-ui .response-col_description,
  .swagger-ui .responses-inner h4, .swagger-ui .responses-inner h5,
  .swagger-ui .parameter__name, .swagger-ui .parameter__type, .swagger-ui .parameter__deprecated,
  .swagger-ui .parameter__in, .swagger-ui .parameter__extension, .swagger-ui table thead tr th,
  .swagger-ui table thead tr td, .swagger-ui table tbody tr td, .swagger-ui .tab li,
  .swagger-ui .opblock-title_normal, .swagger-ui .model, .swagger-ui .model-title,
  .swagger-ui section.models h4, .swagger-ui section.models h5, .swagger-ui .servers > label,
  .swagger-ui label, .swagger-ui .scheme-container .schemes > label,
  .swagger-ui .scheme-container, .swagger-ui .info .title small pre,
  .swagger-ui .markdown p, .swagger-ui .markdown li, .swagger-ui .renderedMarkdown p,
  .swagger-ui .renderedMarkdown li, .swagger-ui .response-control-media-type,
  .swagger-ui .response-control-media-type__title, .swagger-ui .response-controls,
  .swagger-ui .opblock-body, .swagger-ui .topbar { color:#c9d1d9; }

  .swagger-ui .info .title { color:#58a6ff; }
  .swagger-ui .info hgroup.main a { color:#58a6ff; }
  .swagger-ui a { color:#58a6ff; }
  .swagger-ui a:hover { color:#79c0ff; }

  /* Tag headers (e.g. /v1/kv group) */
  .swagger-ui .opblock-tag { background:#161b22; border-bottom:1px solid #30363d; padding:8px 16px; }
  .swagger-ui .opblock-tag:hover { background:#1c2128; }
  .swagger-ui .opblock-tag small { color:#8b949e; }

  /* Operation panels */
  .swagger-ui .opblock { background:#161b22; border:1px solid #30363d; box-shadow:none; margin:6px 0; }
  .swagger-ui .opblock .opblock-summary { border-bottom:1px solid #30363d; }
  .swagger-ui .opblock.opblock-get .opblock-summary { border-color:#1f6feb; }
  .swagger-ui .opblock.opblock-get { border-color:#1f6feb55; }
  .swagger-ui .opblock.opblock-post .opblock-summary { border-color:#bc8cff; }
  .swagger-ui .opblock.opblock-post { border-color:#bc8cff55; }
  .swagger-ui .opblock.opblock-put .opblock-summary { border-color:#3fb950; }
  .swagger-ui .opblock.opblock-put { border-color:#3fb95055; }
  .swagger-ui .opblock.opblock-delete .opblock-summary { border-color:#f85149; }
  .swagger-ui .opblock.opblock-delete { border-color:#f8514955; }
  .swagger-ui .opblock-summary-method { font-weight:600; }
  .swagger-ui .opblock-summary-path, .swagger-ui .opblock-summary-path__deprecated { color:#c9d1d9; }

  /* Inner sections (parameters / responses) */
  .swagger-ui .opblock-section-header { background:#0d1117; border-top:1px solid #30363d; }
  .swagger-ui .opblock-description-wrapper, .swagger-ui .opblock-external-docs-wrapper,
  .swagger-ui .opblock .opblock-section .opblock-section-header { background:#0d1117; }
  .swagger-ui .table-container, .swagger-ui .responses-wrapper, .swagger-ui .parameters-container { background:transparent; }
  .swagger-ui .parameters-col_description input[type=text],
  .swagger-ui input[type=text], .swagger-ui input[type=password], .swagger-ui input[type=search],
  .swagger-ui input[type=email], .swagger-ui input[type=file], .swagger-ui textarea, .swagger-ui select {
    background:#0d1117; color:#c9d1d9; border:1px solid #30363d; border-radius:3px;
  }
  .swagger-ui .parameters-col_description .markdown p { color:#8b949e; }

  /* Tables (responses, parameters, schemas) */
  .swagger-ui table thead tr td, .swagger-ui table thead tr th { background:#161b22; border-bottom:1px solid #30363d; color:#c9d1d9; }
  .swagger-ui table tbody tr td { border-bottom:1px solid #21262d; }
  .swagger-ui .response { border:none; }
  .swagger-ui .responses-inner h4 { color:#c9d1d9; }
  .swagger-ui .response-col_status { color:#3fb950; }
  .swagger-ui .response-col_links { color:#8b949e; }

  /* Schema / model boxes */
  .swagger-ui .model-box, .swagger-ui section.models { background:#0d1117; border:1px solid #30363d; }
  .swagger-ui .model-toggle:after { background:none; }
  .swagger-ui section.models .model-container { background:#161b22; border:1px solid #30363d; }
  .swagger-ui .model .property.primitive { color:#8b949e; }
  .swagger-ui .model .prop-type { color:#79c0ff; }
  .swagger-ui .model .prop-name { color:#c9d1d9; }
  .swagger-ui .model-title { color:#c9d1d9; }
  .swagger-ui .prop-format { color:#8b949e; }

  /* Code / highlighted blocks (response body samples, curl) */
  .swagger-ui .highlight-code, .swagger-ui .microlight, .swagger-ui pre {
    background:#0d1117 !important; color:#c9d1d9 !important; border:1px solid #30363d; border-radius:3px;
  }
  .swagger-ui .opblock-body pre.microlight { background:#0d1117 !important; }

  /* Buttons */
  .swagger-ui .btn { background:#21262d; color:#c9d1d9; border:1px solid #30363d; }
  .swagger-ui .btn:hover { background:#30363d; }
  .swagger-ui .btn.execute { background:#1f6feb; color:#fff; border-color:#1f6feb; }
  .swagger-ui .btn.execute:hover { background:#388bfd; }
  .swagger-ui .btn.cancel { background:#21262d; color:#f85149; border-color:#f85149; }
  .swagger-ui .btn.authorize { background:#3fb950; color:#0d1117; border-color:#3fb950; }
  .swagger-ui .btn.authorize svg { fill:#0d1117; }
  .swagger-ui .btn.authorize:hover { background:#4cc56b; }

  /* Auth modal */
  .swagger-ui .dialog-ux .modal-ux { background:#161b22; border:1px solid #30363d; color:#c9d1d9; }
  .swagger-ui .dialog-ux .modal-ux-header { background:#0d1117; border-bottom:1px solid #30363d; }
  .swagger-ui .dialog-ux .modal-ux-header h3 { color:#c9d1d9; }
  .swagger-ui .auth-container h4, .swagger-ui .auth-container p { color:#c9d1d9; }
  .swagger-ui .auth-container input[type=text],
  .swagger-ui .auth-container input[type=password] { background:#0d1117; color:#c9d1d9; border:1px solid #30363d; }

  /* Server selector */
  .swagger-ui .scheme-container { background:#161b22; border:1px solid #30363d; box-shadow:none; padding:10px; }
  .swagger-ui .servers > label select { background:#0d1117; color:#c9d1d9; border:1px solid #30363d; }

  /* Filter / search */
  .swagger-ui .filter .operation-filter-input { background:#0d1117; color:#c9d1d9; border:1px solid #30363d; }

  /* Topbar (we hide it — we have our own nav) */
  .swagger-ui .topbar { display:none; }

  /* SVG icons (arrows, lock) — flip stroke to readable colour */
  .swagger-ui svg:not(:root) { fill:#c9d1d9; }
  .swagger-ui .opblock .opblock-summary svg { fill:#8b949e; }
  .swagger-ui .opblock .opblock-summary .authorization__btn svg { fill:#d29922; }
  .swagger-ui .opblock-summary-control:focus { outline:none; }

  /* Tabs (e.g. example/schema toggle in responses) */
  .swagger-ui .tab li { color:#8b949e; }
  .swagger-ui .tab li.active { color:#c9d1d9; border-bottom:2px solid #58a6ff; }

  /* Required asterisks etc. */
  .swagger-ui .parameter__name.required:after { color:#f85149; }
  .swagger-ui .parameter__name.required span { color:#f85149; }
</style></head><body>
<h1>API explorer</h1>
<p class="sub">Interactive Swagger UI for the NATS-KV data plane and control plane. Authorize once with your bearer token and run any endpoint live.</p>
<div id="swagger-ui"></div>
<script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
<script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui-standalone-preset.js"></script>
<script>__NAV_JS__
renderNav('api');
window.onload = () => {
  window.ui = SwaggerUIBundle({
    url: "/openapi.yaml",
    dom_id: "#swagger-ui",
    presets: [SwaggerUIBundle.presets.apis, SwaggerUIStandalonePreset],
    layout: "BaseLayout",
    deepLinking: true,
    persistAuthorization: true,
  });
};
</script>
</body></html>
"##;

// =====================================================================
// /openapi.yaml — OpenAPI 3.0 spec for both planes.
// Hand-maintained (small enough surface). Server URLs match the live
// production endpoints; "Authorize" in Swagger UI sets the bearer.
// =====================================================================
const OPENAPI_YAML: &str = r##"openapi: 3.0.3
info:
  title: NATS-KV for Akamai Functions
  description: |
    Globally distributed NATS JetStream KV with HTTP API, fronted by GTM
    across 27 regions. Demo-grade research POC — see /docs for status,
    architecture, and Cosmos comparison.
  version: 0.2.5
servers:
  - url: https://edge.nats-kv.connected-cloud.io
    description: Data plane (GTM-routed to nearest of 27 regions)
  - url: https://cp.nats-kv.connected-cloud.io
    description: Control plane (LZ only — tenants, buckets, placement)
security:
  - bearerAuth: []
components:
  securitySchemes:
    bearerAuth:
      type: http
      scheme: bearer
      description: |
        Per-tenant API key minted via the admin app's invite → claim flow.
        For the open demo, use `akv_demo_open` (read/write to the shared `demo` bucket).
  schemas:
    PutResponse:
      type: object
      properties:
        revision:
          type: integer
          format: int64
          example: 42
    HistoryEntry:
      type: object
      properties:
        revision: { type: integer, format: int64 }
        value_b64: { type: string }
        created: { type: string, format: date-time }
    KeysResponse:
      type: object
      properties:
        keys:
          type: array
          items: { type: string }
    BucketSummary:
      type: object
      properties:
        name: { type: string }
        replicas: { type: integer }
        peers:
          type: array
          items:
            type: object
            properties:
              name: { type: string }
              role: { type: string, enum: [leader, replica] }
              current: { type: boolean }
        mirrors:
          type: array
          items: { type: object }
    PlacementDecision:
      type: object
      properties:
        mode: { type: string, enum: [auto, anchor, manual] }
        replicas: { type: integer }
        anchor: { type: string, example: us-ord }
        chosen_geo: { type: string, example: na }
        chosen_regions:
          type: array
          items: { type: string }
          description: predicted top-k regions by RTT-from-anchor
        actual_regions:
          type: array
          items: { type: string }
          description: regions JetStream actually placed on (may differ from chosen)
        write_latency_ms: { type: number, format: double }
        quorum_edge_ms: { type: number, format: double }
        notes:
          type: array
          items: { type: string }
    CreateBucketRequest:
      type: object
      required: [name]
      properties:
        name: { type: string, example: sessions }
        replicas: { type: integer, enum: [1, 3, 5], default: 3 }
        geo:
          type: string
          description: 'auto | anchor:&lt;region&gt; | na | eu | ap | sa'
          default: auto
        anchor: { type: string, description: 'hint for auto mode (default us-ord)' }
        history: { type: integer, default: 8, description: 'revisions to keep per key' }
        no_mirrors: { type: boolean, default: false }
paths:
  /v1/health:
    get:
      summary: Liveness probe
      security: []
      responses:
        '200':
          description: OK
          content:
            application/json:
              schema:
                type: object
                properties:
                  status: { type: string, example: ok }
                  region: { type: string, example: us-ord }
  /v1/kv/{bucket}/{key}:
    get:
      summary: Read latest value
      parameters:
        - { in: path, name: bucket, required: true, schema: { type: string } }
        - { in: path, name: key,    required: true, schema: { type: string } }
        - { in: query, name: revision, required: false, schema: { type: integer, format: int64 }, description: 'specific historical revision' }
      responses:
        '200':
          description: value bytes; X-Revision header carries the revision
          content:
            application/octet-stream:
              schema: { type: string, format: binary }
        '404': { description: not found }
    put:
      summary: Write
      parameters:
        - { in: path, name: bucket, required: true, schema: { type: string } }
        - { in: path, name: key,    required: true, schema: { type: string } }
        - { in: header, name: If-Match,      required: false, schema: { type: integer }, description: 'CAS — only write if current revision matches' }
        - { in: header, name: If-None-Match, required: false, schema: { type: string },  description: 'set to * to create-if-absent' }
      requestBody:
        required: true
        content:
          application/octet-stream:
            schema: { type: string, format: binary }
      responses:
        '200':
          description: written
          content:
            application/json:
              schema: { $ref: '#/components/schemas/PutResponse' }
        '412': { description: CAS mismatch }
    delete:
      summary: Tombstone the key
      parameters:
        - { in: path, name: bucket, required: true, schema: { type: string } }
        - { in: path, name: key,    required: true, schema: { type: string } }
        - { in: header, name: If-Match, required: false, schema: { type: integer } }
      responses:
        '200': { description: deleted }
        '412': { description: CAS mismatch }
  /v1/kv/{bucket}/{key}/history:
    get:
      summary: All retained revisions of a key
      parameters:
        - { in: path, name: bucket, required: true, schema: { type: string } }
        - { in: path, name: key,    required: true, schema: { type: string } }
      responses:
        '200':
          description: array of HistoryEntry
          content:
            application/json:
              schema:
                type: array
                items: { $ref: '#/components/schemas/HistoryEntry' }
  /v1/kv/{bucket}/{key}/incr:
    post:
      summary: Atomic counter (NATS-only)
      parameters:
        - { in: path, name: bucket, required: true, schema: { type: string } }
        - { in: path, name: key,    required: true, schema: { type: string } }
      requestBody:
        required: false
        content:
          application/json:
            schema:
              type: object
              properties:
                by: { type: integer, default: 1 }
      responses:
        '200':
          description: new value (atomic, CAS-loop server-side)
          content:
            application/json:
              schema:
                type: object
                properties:
                  value: { type: integer, format: int64 }
                  revision: { type: integer, format: int64 }
  /v1/kv/{bucket}/keys:
    get:
      summary: List keys matching a NATS subject pattern (NATS-only)
      parameters:
        - { in: path, name: bucket, required: true, schema: { type: string } }
        - { in: query, name: match, required: true, schema: { type: string }, example: 'users.*.session' }
      responses:
        '200':
          description: matching keys
          content:
            application/json:
              schema: { $ref: '#/components/schemas/KeysResponse' }
  /v1/admin/buckets:
    get:
      summary: List every bucket on the cluster
      responses:
        '200':
          description: bucket list with placement, peers, mirror count
          content:
            application/json:
              schema:
                type: object
                properties:
                  buckets:
                    type: array
                    items: { $ref: '#/components/schemas/BucketSummary' }
                  served_by: { type: string, example: kv-us-ord }
  /v1/admin/cluster:
    get:
      summary: Identify which leaf served this request
      responses:
        '200':
          description: this adapter's region + local mirror map
  /v1/me:
    get:
      summary: Calling tenant's identity (control plane)
      servers:
        - { url: 'https://cp.nats-kv.connected-cloud.io' }
      responses:
        '200': { description: tenant info }
  /v1/me/buckets:
    get:
      summary: Calling tenant's buckets, enriched (control plane)
      servers:
        - { url: 'https://cp.nats-kv.connected-cloud.io' }
      responses:
        '200':
          description: buckets + per-bucket details
          content:
            application/json:
              schema:
                type: object
                properties:
                  buckets:
                    type: array
                    items: { type: string }
                  details:
                    type: array
                    items: { $ref: '#/components/schemas/BucketSummary' }
                  tenant_id: { type: string }
    post:
      summary: Create a bucket (control plane)
      servers:
        - { url: 'https://cp.nats-kv.connected-cloud.io' }
      requestBody:
        required: true
        content:
          application/json:
            schema: { $ref: '#/components/schemas/CreateBucketRequest' }
      responses:
        '200':
          description: created — response includes the placement Decision
          content:
            application/json:
              schema:
                type: object
                properties:
                  bucket: { type: string }
                  replicas: { type: integer }
                  mirror_count: { type: integer }
                  placement: { $ref: '#/components/schemas/PlacementDecision' }
  /v1/placement/preview:
    get:
      summary: Run the placement engine without creating a bucket (control plane)
      servers:
        - { url: 'https://cp.nats-kv.connected-cloud.io' }
      security: []
      parameters:
        - { in: query, name: anchor,   required: false, schema: { type: string }, example: us-ord }
        - { in: query, name: replicas, required: false, schema: { type: integer }, example: 3 }
        - { in: query, name: mode,     required: false, schema: { type: string, enum: [auto, anchor] }, example: anchor }
      responses:
        '200':
          description: placement Decision
          content:
            application/json:
              schema: { $ref: '#/components/schemas/PlacementDecision' }
"##;

// =====================================================================
// /verify — functional smoke test that exercises every NATS-KV surface
// against a chosen bucket. Each test is independent; reports per-test
// pass/fail with latency. Cleans up its own keys (random suffix).
// =====================================================================
const VERIFY_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV — verify</title><style>__SHARED_CSS__
  table.tests { width:100%; border-collapse:collapse; font-size:13px; }
  table.tests th, table.tests td { padding:8px; border-bottom:1px solid #30363d; vertical-align:top; }
  table.tests th { text-align:left; background:#161b22; }
  table.tests td.status { width:70px; text-align:center; font-weight:600; }
  td.status.pass { color:#3fb950; }
  td.status.fail { color:#f85149; }
  td.status.skip { color:#8b949e; }
  td.status.run  { color:#58a6ff; }
  td.status.idle { color:#8b949e; }
  td.detail { font-family:ui-monospace, monospace; font-size:11px; color:#8b949e; max-width:600px; word-break:break-word; white-space:pre-wrap; }
  td.detail.fail { color:#f85149; }
  td.lat { width:80px; text-align:right; font-family:ui-monospace, monospace; color:#8b949e; }
  .summary { padding:12px; border-radius:4px; margin:12px 0; font-weight:600; font-size:14px; }
  .summary.pass { background:#0e2a17; border:1px solid #3fb950; color:#3fb950; }
  .summary.fail { background:#3a1414; border:1px solid #f85149; color:#f85149; }
  .summary.run { background:#0e1a3a; border:1px solid #58a6ff; color:#58a6ff; }
</style></head><body>
<h1>Functional verification</h1>
<p class="sub">End-to-end smoke test for every NATS-KV surface. Pick a bucket, click Run, watch each test go green. Use this on any new bucket / region / version to confirm the system is honest.</p>

<fieldset>
  <legend>Configure</legend>
  <div class="row">
    <div><label>bucket</label><select id="vfy-bucket"></select></div>
    <div style="flex:2"><label>placement</label><div id="vfy-bucket-info" class="meta" style="font-size:11px; padding:6px 0">(loading…)</div></div>
  </div>
  <p class="meta" style="margin:0; font-size:11px;">Tests use random key suffixes (<code>vfy-&lt;ts&gt;-&lt;n&gt;</code>) and clean up after themselves. Safe to run on production demo data.</p>
  <div class="actions">
    <button onclick="runVerify()" id="vfy-run">Run all tests</button>
    <span id="vfy-status" class="meta" style="margin-left:12px"></span>
  </div>
</fieldset>

<div id="vfy-summary"></div>

<fieldset>
  <legend>Tests</legend>
  <table class="tests">
    <thead><tr><th style="width:32%">Test</th><th class="status">Status</th><th>Detail</th><th class="lat">Latency</th></tr></thead>
    <tbody id="vfy-rows"></tbody>
  </table>
</fieldset>

<script>__NAV_JS__
renderNav('verify');
const $ = (id) => document.getElementById(id);

const ALL_REGIONS = [
  "us-ord","us-east","us-central","us-west","us-southeast","us-lax","us-mia","us-sea","ca-central","br-gru",
  "gb-lon","eu-central","de-fra-2","fr-par-2","nl-ams","se-sto","it-mil",
  "ap-south","sg-sin-2","ap-northeast","jp-tyo-3","jp-osa","ap-west","in-bom-2","in-maa","id-cgk","ap-southeast",
];

// ---- Bucket picker (mirrors playground logic) ----
const _bucketDetails = {};
async function loadVerifyBuckets() {
  const sel = $("vfy-bucket");
  const opts = [{name: "demo", label: "demo (shared, R3 NA, 27 mirrors)"}];
  if (userKey()) {
    try {
      const r = await fetch("/api/control/v1/me/buckets", { headers: {"Authorization": "Bearer " + userKey()} });
      if (r.ok) {
        const j = await r.json();
        for (const d of (j.details||[])) {
          const short = d.name.includes("__") ? d.name.split("__").slice(1).join("__") : d.name;
          opts.push({name: d.name, label: `${short} (R${d.replicas||1}, ${d.mirror_count||0} mirrors)`});
          _bucketDetails[d.name] = d;
        }
      }
    } catch (e) {}
  }
  try {
    const r = await fetch("/api/nats/v1/admin/buckets", { headers: {"X-KV-Key": userKey() || "akv_demo_open"} });
    const j = await r.json();
    const inner = JSON.parse(atob(j.body_b64||""));
    for (const b of (inner.buckets||[])) {
      if (!_bucketDetails[b.name]) _bucketDetails[b.name] = b;
    }
  } catch (e) {}
  sel.innerHTML = opts.map(o => `<option value="${o.name}">${o.label}</option>`).join("");
  sel.onchange = updateBucketInfo;
  updateBucketInfo();
}
function updateBucketInfo() {
  const name = $("vfy-bucket").value;
  const d = _bucketDetails[name];
  const el = $("vfy-bucket-info");
  if (!d) { el.textContent = `bucket=${name}`; return; }
  const peers = (d.peers||[]).map(p => (p.name||'').replace(/^kv-/,'')).join(", ");
  const tags = (d.placement_tags||[]).join(",") || "(none)";
  const mc = d.mirror_count !== undefined ? d.mirror_count : (d.mirrors||[]).length;
  el.innerHTML = `<code>${name}</code> · R${d.replicas||1} · placement <code>${tags}</code> · peers <code>${peers}</code> · ${mc} read mirrors`;
}
loadVerifyBuckets();

// ---- Test harness ----
function nb(bucket) { return encodeURIComponent(bucket); }
function nk(key) { return encodeURIComponent(key); }
function bearerHeaders() {
  const h = { "X-KV-Key": userKey() || "akv_demo_open" };
  return h;
}
async function call(method, path, body, opts={}) {
  const init = { method, headers: { ...bearerHeaders(), ...(opts.headers||{}) } };
  if (body !== undefined) init.body = body;
  const t0 = performance.now();
  const r = await fetch("/api" + path, init);
  const dt = performance.now() - t0;
  const env = await r.json();   // FWF wraps; envelope.status is the real status
  const inner = env.body_b64 ? atob(env.body_b64) : "";
  return { status: env.status, headers: env, body: inner, ms: dt };
}

// Each test: { name, run: async () => { return {ok, detail} } }
const TESTS = [
  {
    name: "1. PUT then GET (basic write/read round-trip)",
    run: async (bucket, suffix) => {
      const k = `vfy-${suffix}-roundtrip`;
      const put = await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "hello-roundtrip");
      if (put.status !== 200) return { ok:false, detail:`PUT failed: status=${put.status} body=${put.body.slice(0,200)}` };
      const get = await call("GET", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`);
      if (get.status !== 200) return { ok:false, detail:`GET failed: status=${get.status}` };
      if (get.body !== "hello-roundtrip") return { ok:false, detail:`expected 'hello-roundtrip', got '${get.body}'` };
      return { ok:true, detail:`PUT rev=${JSON.parse(put.body).revision} → GET ok` };
    }
  },
  {
    name: "2. DELETE then GET returns 404",
    run: async (bucket, suffix) => {
      const k = `vfy-${suffix}-delete`;
      await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "to-delete");
      const del = await call("DELETE", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`);
      if (del.status !== 200 && del.status !== 204) return { ok:false, detail:`DELETE failed: status=${del.status}` };
      const get = await call("GET", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`);
      if (get.status !== 404) return { ok:false, detail:`expected 404 after delete, got status=${get.status}` };
      return { ok:true, detail:`DELETE ok → GET 404 as expected` };
    }
  },
  {
    name: "3. Revision history (3 PUTs, GET history returns ≥3 revisions)",
    run: async (bucket, suffix) => {
      const k = `vfy-${suffix}-history`;
      for (let i = 1; i <= 3; i++) {
        await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, `version-${i}`);
      }
      const h = await call("GET", `/nats/v1/kv/${nb(bucket)}/${nk(k)}/history`);
      if (h.status !== 200) return { ok:false, detail:`history GET status=${h.status}` };
      let parsed;
      try { parsed = JSON.parse(h.body); } catch (e) { return { ok:false, detail:`history not JSON: ${h.body.slice(0,200)}` }; }
      const arr = Array.isArray(parsed) ? parsed : (parsed.history || []);
      if (arr.length < 3) return { ok:false, detail:`expected ≥3 revisions, got ${arr.length}` };
      return { ok:true, detail:`history depth=${arr.length}` };
    }
  },
  {
    name: "4. Read specific revision (?revision=1 returns first value)",
    run: async (bucket, suffix) => {
      const k = `vfy-${suffix}-rev`;
      const put1 = await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "first");
      if (put1.status !== 200) return { ok:false, detail:`first PUT failed status=${put1.status}` };
      const rev1 = JSON.parse(put1.body).revision;
      await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "second");
      const get = await call("GET", `/nats/v1/kv/${nb(bucket)}/${nk(k)}?revision=${rev1}`);
      if (get.status !== 200) return { ok:false, detail:`historical GET status=${get.status}` };
      if (get.body !== "first") return { ok:false, detail:`expected 'first' at rev=${rev1}, got '${get.body}'` };
      return { ok:true, detail:`historical revision ${rev1} returned 'first' as expected` };
    }
  },
  {
    name: "5. CAS write succeeds with correct If-Match",
    run: async (bucket, suffix) => {
      const k = `vfy-${suffix}-cas-ok`;
      const put1 = await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "v1");
      if (put1.status !== 200) return { ok:false, detail:`initial PUT failed status=${put1.status}` };
      const rev = JSON.parse(put1.body).revision;
      const cas = await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "v2", { headers:{"If-Match": String(rev)} });
      if (cas.status !== 200) return { ok:false, detail:`CAS PUT (correct rev=${rev}) failed status=${cas.status} body=${cas.body.slice(0,120)}` };
      return { ok:true, detail:`CAS succeeded at rev=${rev}` };
    }
  },
  {
    name: "6. CAS write rejects stale If-Match (returns 412)",
    run: async (bucket, suffix) => {
      const k = `vfy-${suffix}-cas-fail`;
      const put1 = await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "v1");
      if (put1.status !== 200) return { ok:false, detail:`initial PUT failed status=${put1.status}` };
      const stale = JSON.parse(put1.body).revision;
      await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "v2");          // bumps rev
      const cas = await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "v3", { headers:{"If-Match": String(stale)} });
      if (cas.status !== 412) return { ok:false, detail:`expected 412 with stale rev=${stale}, got status=${cas.status}` };
      return { ok:true, detail:`CAS correctly rejected stale rev=${stale} with 412` };
    }
  },
  {
    name: "7. Atomic increment (5× incr by 1, value === 5)",
    run: async (bucket, suffix) => {
      const k = `vfy-${suffix}-counter`;
      let last;
      for (let i = 1; i <= 5; i++) {
        const r = await call("POST", `/nats/v1/kv/${nb(bucket)}/${nk(k)}/incr`, JSON.stringify({by:1}), { headers:{"Content-Type":"application/json"} });
        if (r.status !== 200) return { ok:false, detail:`incr ${i} failed status=${r.status} body=${r.body.slice(0,200)}` };
        last = r.body;
      }
      const final = await call("GET", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`);
      const v = parseInt(final.body, 10);
      if (v !== 5) return { ok:false, detail:`expected counter=5, got '${final.body}' (last incr response: ${last.slice(0,120)})` };
      return { ok:true, detail:`5 atomic increments → value=5` };
    }
  },
  {
    name: "8. Subject-pattern wildcard query returns all matching keys",
    run: async (bucket, suffix) => {
      const id = suffix;
      for (const u of ["alice","bob","carol"]) {
        const r = await call("PUT", `/nats/v1/kv/${nb(bucket)}/users.${u}.${id}.session`, u);
        if (r.status !== 200) return { ok:false, detail:`PUT users.${u} failed status=${r.status}` };
      }
      const q = await call("GET", `/nats/v1/kv/${nb(bucket)}/keys?match=users.*.${id}.session`);
      if (q.status !== 200) return { ok:false, detail:`keys query status=${q.status}` };
      let keys;
      try { keys = (JSON.parse(q.body).keys || []); } catch (e) { return { ok:false, detail:`keys not JSON: ${q.body.slice(0,200)}` }; }
      if (keys.length < 3) return { ok:false, detail:`expected ≥3 matching keys, got ${keys.length}: ${JSON.stringify(keys)}` };
      return { ok:true, detail:`wildcard matched ${keys.length} keys: ${keys.slice(0,3).join(", ")}…` };
    }
  },
  {
    name: "9. Reads served from local-mirror (X-Read-Source header)",
    run: async (bucket, suffix) => {
      // Hit the adapter directly via FWF — the GTM-routed leaf will read its own local mirror if one exists.
      const k = `vfy-${suffix}-local`;
      await call("PUT", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`, "local-test");
      // Brief settle so mirrors catch up
      await new Promise(r => setTimeout(r, 300));
      // Re-fetch and check the wrapped envelope's headers (Spin proxy strips most, but we look for served_by + adapter_ms)
      const r = await call("GET", `/nats/v1/kv/${nb(bucket)}/${nk(k)}`);
      const adapterMs = r.headers.adapter_ms;
      const servedBy = r.headers.served_by;
      // If adapter_ms is 0 the read served from local mirror or local source replica — both are fine.
      const ok = (adapterMs === "0" || adapterMs === 0 || (typeof adapterMs === "string" && parseInt(adapterMs,10) <= 5));
      if (!ok) return { ok:false, detail:`expected adapter_ms ≤ 5 (local-mirror or local replica), got adapter_ms='${adapterMs}' served_by='${servedBy}'` };
      return { ok:true, detail:`served_by=${servedBy} adapter_ms=${adapterMs}` };
    }
  },
  {
    name: "10. Bucket appears in /v1/admin/buckets listing",
    run: async (bucket, suffix) => {
      const r = await call("GET", "/nats/v1/admin/buckets");
      if (r.status !== 200) return { ok:false, detail:`admin/buckets status=${r.status}` };
      let parsed;
      try { parsed = JSON.parse(r.body); } catch(e) { return { ok:false, detail:`not JSON: ${r.body.slice(0,200)}` }; }
      const found = (parsed.buckets || []).some(b => b.name === bucket);
      if (!found) return { ok:false, detail:`bucket ${bucket} not found in admin/buckets list` };
      return { ok:true, detail:`bucket present in admin/buckets` };
    }
  },
];

function setRow(idx, status, detail, latency) {
  const row = $("vfy-row-" + idx);
  if (!row) return;
  row.querySelector(".status").className = "status " + status;
  row.querySelector(".status").textContent =
    status === "pass" ? "PASS" :
    status === "fail" ? "FAIL" :
    status === "run"  ? "…" :
    status === "skip" ? "SKIP" : "—";
  row.querySelector(".detail").className = "detail " + (status==="fail"?"fail":"");
  row.querySelector(".detail").textContent = detail || "";
  row.querySelector(".lat").textContent = latency != null ? latency.toFixed(0) + " ms" : "";
}

async function runVerify() {
  const bucket = $("vfy-bucket").value;
  if (!bucket) { alert("pick a bucket first"); return; }
  $("vfy-run").disabled = true;
  $("vfy-status").textContent = "running…";
  $("vfy-summary").innerHTML = `<div class="summary run">running ${TESTS.length} tests against bucket <code>${bucket}</code>…</div>`;

  // (Re)build the rows fresh
  const tb = $("vfy-rows");
  tb.innerHTML = TESTS.map((t, i) =>
    `<tr id="vfy-row-${i}"><td>${t.name}</td><td class="status idle">—</td><td class="detail"></td><td class="lat"></td></tr>`
  ).join("");

  const suffix = Date.now().toString(36) + "-" + Math.floor(Math.random()*1000);
  let passed = 0, failed = 0;
  const results = [];
  for (let i = 0; i < TESTS.length; i++) {
    setRow(i, "run", "running…", null);
    const t0 = performance.now();
    let ok = false, detail = "";
    try {
      const out = await TESTS[i].run(bucket, suffix);
      ok = !!out.ok;
      detail = out.detail || "";
    } catch (e) {
      ok = false;
      detail = "exception: " + (e && e.message || String(e));
    }
    const dt = performance.now() - t0;
    setRow(i, ok ? "pass" : "fail", detail, dt);
    if (ok) passed++; else failed++;
    results.push({ name: TESTS[i].name, ok, detail });
  }

  const cls = failed === 0 ? "pass" : "fail";
  $("vfy-summary").innerHTML = `<div class="summary ${cls}">${passed}/${TESTS.length} passed${failed?` · ${failed} failed`:""}</div>`;
  $("vfy-status").textContent = `done — ${passed}/${TESTS.length} passed`;
  $("vfy-run").disabled = false;
}

// Render an empty test list immediately so the layout doesn't shift on first run
$("vfy-rows").innerHTML = TESTS.map((t, i) =>
  `<tr id="vfy-row-${i}"><td>${t.name}</td><td class="status idle">—</td><td class="detail"></td><td class="lat"></td></tr>`
).join("");
</script>
</body></html>
"##;

// =====================================================================
// GATE_HTML — shown to any visitor without a valid UI gate cookie / token.
// They can either request an invite (queued for admin to grant) or paste
// an `?access=<token>` URL given to them by the admin.
// Note: we don't reuse the shared nav here — the gate is the *only* page
// unauthed visitors ever see.
// =====================================================================
const GATE_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV — request access</title><style>__SHARED_CSS__
  body { max-width:680px; }
  .hero { padding:24px; border:1px solid #30363d; border-radius:6px; background:#161b22; margin:24px 0; }
  .hero h1 { margin:0 0 8px; }
  .hero p { color:#8b949e; }
  .twocol { display:grid; grid-template-columns:1fr; gap:16px; }
  fieldset { padding:16px; }
  textarea { min-height:60px; }
  #out { margin-top:12px; padding:10px; border-radius:4px; font-size:13px; display:none; }
  #out.ok { background:#0e2a17; border:1px solid #3fb950; color:#3fb950; display:block; }
  #out.err { background:#3a1414; border:1px solid #f85149; color:#f85149; display:block; }
</style></head><body>
<div class="hero">
  <h1>NATS-KV for Akamai Functions</h1>
  <p>Demo-grade research POC: a globally distributed NATS JetStream KV exposed as an HTTP API for Spin functions. Atomic increment, revision history, CAS, subject-pattern wildcards, geo-pinned RAFT placement, mirrors-everywhere reads.</p>
  <p style="margin-top:8px">This is a private demo. Request an invite below — Brian will share an access link once approved.</p>
</div>

<div class="twocol">
  <fieldset>
    <legend>Request an invite</legend>
    <div class="row">
      <div><label>name</label><input id="rq-name" placeholder="Jane Smith"></div>
      <div><label>email</label><input id="rq-email" placeholder="jane@example.com"></div>
    </div>
    <div><label>what do you want to try? (optional)</label><textarea id="rq-reason" placeholder="What you're hoping to evaluate, what kind of workload, etc."></textarea></div>
    <div class="actions" style="margin-top:8px"><button onclick="submitRequest()">Submit request</button></div>
    <div id="out"></div>
  </fieldset>

  <fieldset>
    <legend>Already have an access link?</legend>
    <p class="meta" style="margin:0 0 8px">Paste the token from the URL Brian sent you (the part after <code>?access=</code>) or just click the link directly.</p>
    <div class="row">
      <div><label>access token</label><input id="acc-token" placeholder="e.g. 7c9f4..."></div>
      <div><label>&nbsp;</label><button onclick="useToken()">Unlock</button></div>
    </div>
  </fieldset>
</div>

<p class="meta" style="margin-top:16px;font-size:11px">
  Read the <a href="/health" style="color:#58a6ff">/health</a> endpoint without a token if you just want to verify the demo is up. Source: <a href="https://github.com/ccie7599/nats-kv" style="color:#58a6ff">github.com/ccie7599/nats-kv</a>.
</p>

<script>
async function submitRequest() {
  const name = document.getElementById("rq-name").value.trim();
  const email = document.getElementById("rq-email").value.trim();
  const reason = document.getElementById("rq-reason").value.trim();
  const out = document.getElementById("out");
  if (!name || !email) {
    out.className = "err"; out.textContent = "name and email required"; return;
  }
  try {
    const r = await fetch("/api/request-invite", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, email, reason }),
    });
    if (r.ok) {
      out.className = "ok";
      out.textContent = "Thanks — request received. Brian will follow up with an access link once approved.";
      document.getElementById("rq-name").value = "";
      document.getElementById("rq-email").value = "";
      document.getElementById("rq-reason").value = "";
    } else {
      const j = await r.json().catch(()=>({error:`HTTP ${r.status}`}));
      out.className = "err"; out.textContent = "Submit failed: " + (j.error || r.status);
    }
  } catch (e) {
    out.className = "err"; out.textContent = "Submit failed: " + e.message;
  }
}
function useToken() {
  const t = document.getElementById("acc-token").value.trim();
  if (!t) return;
  // The server sees ?access=<t>, validates, sets cookie, and 302s back to /
  window.location = "/?access=" + encodeURIComponent(t);
}
</script>
</body></html>
"##;
