use spin_sdk::http::{IntoResponse, Method, Request, Response};
use spin_sdk::http_component;
use spin_sdk::variables;

const CONTROL_BASE: &str = "https://cp.nats-kv.connected-cloud.io";
const ADMIN_GATE_COOKIE: &str = "nats-kv-admin-gate";

fn admin_gate_token() -> String {
    variables::get("admin_gate_token").unwrap_or_else(|_| "admin-gate-2026-rotateme".to_string())
}

// True if the request carries a valid admin-gate cookie OR a valid `?access=`
// query. Whitelisted: /health only.
fn is_admin_authed(req: &Request) -> bool {
    let want = admin_gate_token();
    if let Some(cookie_hdr) = req.header("cookie").and_then(|v| v.as_str()) {
        for kv in cookie_hdr.split(';') {
            let p = kv.trim();
            if let Some(rest) = p.strip_prefix(&format!("{ADMIN_GATE_COOKIE}=")) {
                if rest == want {
                    return true;
                }
            }
        }
    }
    let q = req.query();
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("access=") {
            if v == want {
                return true;
            }
        }
    }
    false
}

#[http_component]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    let path = req.path();

    if path == "/health" {
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true,"app":"nats-kv-admin"}"#)
            .build());
    }

    // Admin gate: every other path requires the cookie OR ?access=<token>.
    if !is_admin_authed(&req) {
        return Ok(Response::builder()
            .status(403)
            .header("content-type", "text/html; charset=utf-8")
            .body(GATE_HTML)
            .build());
    }
    // Capture ?access=<token> on first hit — set cookie and 302 to clean URL.
    if req.query().contains("access=") {
        let token = admin_gate_token();
        let cookie = format!("{ADMIN_GATE_COOKIE}={token}; Path=/; Max-Age=1209600; HttpOnly; SameSite=Lax; Secure");
        return Ok(Response::builder()
            .status(302)
            .header("location", path)
            .header("set-cookie", cookie)
            .body(Vec::<u8>::new())
            .build());
    }

    if path == "/" || path == "/index.html" {
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "text/html; charset=utf-8")
            .body(INDEX_HTML)
            .build());
    }

    if let Some(rest) = path.strip_prefix("/api/") {
        let admin_token = req
            .header("authorization")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(proxy(req.method().clone(), rest, req.body(), &admin_token).await?);
    }

    Ok(Response::builder().status(404).body("not found").build())
}

async fn proxy(method: Method, path: &str, body: &[u8], auth: &str) -> anyhow::Result<Response> {
    let url = format!("{CONTROL_BASE}/{path}");
    let mut b = Request::builder();
    b.method(method);
    b.uri(url);
    if !auth.is_empty() {
        b.header("Authorization", auth.to_string());
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
    resp.header("Access-Control-Allow-Origin", "*");
    Ok(resp.body(upstream.body().to_vec()).build())
}

// Minimal gate page shown to anyone hitting the admin app without the right
// query token or cookie. Distinct copy from the user app's gate page — there
// is no "request access" path here; admin access is by direct token only.
const GATE_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>NATS-KV admin</title>
<style>
  body { font-family: ui-monospace, monospace; background:#0d1117; color:#c9d1d9; max-width:540px; margin:60px auto; padding:24px; }
  h1 { color:#f85149; margin-top:0; }
  .box { padding:16px; border:1px solid #30363d; border-radius:6px; background:#161b22; }
  input { width:100%; background:#0d1117; color:#c9d1d9; border:1px solid #30363d; border-radius:4px; padding:8px; font-family:inherit; font-size:13px; box-sizing:border-box; }
  button { background:#f85149; color:#0d1117; border:0; padding:8px 14px; border-radius:4px; font-weight:600; font-family:inherit; cursor:pointer; margin-top:8px; }
  .meta { color:#8b949e; font-size:12px; margin-top:16px; }
</style></head><body>
<h1>NATS-KV admin</h1>
<div class="box">
  <p>This admin console is gated. Paste the admin gate token below — separate from the admin <em>bearer</em> token (the bearer comes after).</p>
  <input id="t" type="password" placeholder="admin gate token">
  <button onclick="window.location='?access=' + encodeURIComponent(document.getElementById('t').value)">Unlock</button>
</div>
<p class="meta">If you're not Brian, you're in the wrong place.</p>
</body></html>
"##;

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>NATS-KV admin</title>
<style>
  :root { color-scheme: dark; --bg:#0d1117; --fg:#c9d1d9; --accent:#f85149; --muted:#8b949e; --ok:#3fb950; --warn:#d29922; }
  * { box-sizing: border-box; }
  body { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; background:var(--bg); color:var(--fg); margin:0; padding:24px; max-width:1100px; margin:0 auto; }
  h1 { color:var(--accent); margin:0 0 4px 0; }
  .sub { color:var(--muted); margin:0 0 16px 0; }
  fieldset { border:1px solid #30363d; border-radius:6px; padding:16px; margin:0 0 16px 0; }
  legend { color:var(--accent); padding:0 8px; }
  label { display:block; color:var(--muted); margin:6px 0 2px 0; font-size:12px; }
  input, textarea { width:100%; background:#161b22; color:var(--fg); border:1px solid #30363d; border-radius:4px; padding:8px; font-family:inherit; font-size:13px; }
  button { background:var(--accent); color:#0d1117; border:0; padding:8px 14px; border-radius:4px; font-family:inherit; font-size:13px; cursor:pointer; font-weight:600; }
  button:hover { filter:brightness(1.15); }
  button.secondary { background:#30363d; color:var(--fg); }
  button.warn { background:var(--warn); color:#0d1117; }
  button.danger { background:#a40e26; color:#fff; }
  .row { display:grid; grid-template-columns: 2fr 1fr 100px; gap:8px; align-items:end; }
  .actions { display:flex; gap:8px; margin-top:10px; flex-wrap:wrap; align-items:center; }
  pre { background:#161b22; border:1px solid #30363d; border-radius:4px; padding:12px; max-height:300px; overflow:auto; font-size:12px; }
  table { width:100%; border-collapse:collapse; font-size:12px; }
  th, td { text-align:left; padding:6px 8px; border-bottom:1px solid #30363d; }
  th { color:var(--muted); }
  td.actions-col { white-space:nowrap; }
  .meta { color:var(--muted); font-size:11px; }
  .ok { color:var(--ok); } .err { color:#f85149; } .warn { color:var(--warn); }
  .pill { display:inline-block; background:#30363d; padding:2px 6px; border-radius:3px; font-size:10px; }
  .copy { display:inline-block; background:#161b22; border:1px solid #30363d; padding:6px 10px; border-radius:4px; font-family:monospace; cursor:pointer; user-select:all; word-break:break-all; }
</style>
</head>
<body>
  <h1>NATS-KV admin</h1>
  <p class="sub">Internal POC management. Bearer token gates every action; loaded from local browser storage.</p>

  <fieldset>
    <legend>Admin token</legend>
    <label>Paste admin bearer token (kept in browser localStorage only)</label>
    <input id="token" type="password" placeholder="06ddd5c4...">
    <div class="actions">
      <button onclick="saveToken()">Save</button>
      <button class="secondary" onclick="clearToken()">Clear</button>
      <button class="secondary" onclick="checkToken()">Verify</button>
      <span id="token-status" class="meta"></span>
    </div>
  </fieldset>

  <fieldset>
    <legend>Issue invite</legend>
    <div class="row">
      <div><label>Tag (label for who you're giving this to)</label><input id="invite-tag" placeholder="alice from gaming team"></div>
      <div><label>Expires in</label><input id="invite-expires" value="7d"></div>
      <div><label>&nbsp;</label><button onclick="createInvite()">Create</button></div>
    </div>
    <pre id="invite-out">(no invite yet)</pre>
  </fieldset>

  <fieldset>
    <legend>Pending invite requests <span class="meta" id="pir-count"></span></legend>
    <p class="meta" style="margin:0 0 8px">Visitors who hit the gated demo UI and submitted the "request invite" form. Click <b>Approve</b> to mint an invite, get a complete URL to send back to the requester, and mark the request fulfilled.</p>
    <div class="actions">
      <button class="secondary" onclick="loadInviteRequests('pending')">Refresh (pending)</button>
      <button class="secondary" onclick="loadInviteRequests('')">All requests</button>
    </div>
    <table id="pir-tbl"><thead><tr><th>When</th><th>Name</th><th>Email</th><th>Reason</th><th>Status</th><th>Actions</th></tr></thead><tbody><tr><td colspan="6" class="meta">(click Refresh)</td></tr></tbody></table>
    <div id="pir-out" style="margin-top:8px"></div>
  </fieldset>

  <fieldset>
    <legend>Outstanding invites</legend>
    <div class="actions">
      <button class="secondary" onclick="loadInvites(false)">Refresh (unclaimed)</button>
      <button class="secondary" onclick="loadInvites(true)">Show claimed too</button>
    </div>
    <table id="invites-tbl"><thead><tr><th>Token</th><th>Tag</th><th>Created</th><th>Expires</th><th>Status</th><th>Actions</th></tr></thead><tbody><tr><td colspan="6" class="meta">(click Refresh)</td></tr></tbody></table>
  </fieldset>

  <fieldset>
    <legend>Tenants</legend>
    <div class="actions">
      <button class="secondary" onclick="loadTenants()">Refresh</button>
    </div>
    <table id="tenants-tbl"><thead><tr><th>ID</th><th>Tag</th><th>Created</th><th>Status</th><th>Quota</th><th>Actions</th></tr></thead><tbody><tr><td colspan="6" class="meta">(click Refresh)</td></tr></tbody></table>
  </fieldset>

<script>
const $ = (id) => document.getElementById(id);
const tokenKey = "nats-kv-admin-token";

function token() { return localStorage.getItem(tokenKey) || ""; }
function saveToken() {
  const t = $("token").value.trim();
  if (!t) return;
  localStorage.setItem(tokenKey, t);
  $("token").value = "";
  $("token-status").innerHTML = '<span class="ok">saved (' + t.length + ' chars)</span>';
}
function clearToken() {
  localStorage.removeItem(tokenKey);
  $("token-status").innerHTML = '<span class="warn">cleared</span>';
}
async function checkToken() {
  const r = await fetch("/api/v1/admin/tenants", { headers: { Authorization: "Bearer " + token() } });
  $("token-status").innerHTML = r.ok ? '<span class="ok">valid (HTTP ' + r.status + ')</span>' : '<span class="err">HTTP ' + r.status + '</span>';
}

async function authFetch(path, opts={}) {
  opts.headers = opts.headers || {};
  opts.headers["Authorization"] = "Bearer " + token();
  return fetch("/api/" + path.replace(/^\//,""), opts);
}

async function createInvite() {
  const tag = $("invite-tag").value.trim();
  const expires = $("invite-expires").value.trim() || "7d";
  const r = await authFetch("v1/admin/invites", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tag, expires_in: expires })
  });
  const j = await r.json();
  if (!r.ok) { $("invite-out").textContent = "ERR " + r.status + ": " + JSON.stringify(j); return; }
  $("invite-out").innerHTML = `
    <div><strong>Send this URL to the tester:</strong></div>
    <div class="copy" onclick="navigator.clipboard.writeText('${j.url}')">${j.url}</div>
    <div class="meta">Click to copy. Expires ${j.expires_at}.</div>
  `;
  loadInvites(false);
}

async function loadInvites(includeClaimed) {
  const r = await authFetch("v1/admin/invites" + (includeClaimed ? "?include_claimed=true" : ""));
  const j = await r.json();
  if (!r.ok) { $("invites-tbl").querySelector("tbody").innerHTML = '<tr><td colspan="6" class="err">' + (j.error || r.status) + '</td></tr>'; return; }
  const rows = (j.invites || []).map(inv => `
    <tr>
      <td><span class="pill">${inv.token.slice(0,18)}…</span></td>
      <td>${inv.tag||""}</td>
      <td class="meta">${new Date(inv.created_at).toLocaleString()}</td>
      <td class="meta">${new Date(inv.expires_at).toLocaleString()}</td>
      <td>${inv.claimed_at ? '<span class="meta">claimed → ' + (inv.tenant_id||"") + '</span>' : '<span class="ok">unclaimed</span>'}</td>
      <td class="actions-col">${inv.claimed_at ? "" : '<button class="warn" onclick="revokeInvite(\'' + inv.token + '\')">Revoke</button>'}</td>
    </tr>`).join("");
  $("invites-tbl").querySelector("tbody").innerHTML = rows || '<tr><td colspan="6" class="meta">(none)</td></tr>';
}

async function revokeInvite(tk) {
  if (!confirm("Revoke invite " + tk.slice(0,18) + "…?")) return;
  const r = await authFetch("v1/admin/invites/" + tk, { method: "DELETE" });
  if (!r.ok) alert("Failed: " + r.status);
  loadInvites(false);
}

async function loadTenants() {
  const r = await authFetch("v1/admin/tenants");
  const j = await r.json();
  if (!r.ok) { $("tenants-tbl").querySelector("tbody").innerHTML = '<tr><td colspan="6" class="err">' + (j.error || r.status) + '</td></tr>'; return; }
  const rows = (j.tenants || []).map(t => `
    <tr>
      <td><span class="pill">${t.id.slice(0,18)}…</span></td>
      <td>${t.tag||""}</td>
      <td class="meta">${new Date(t.created_at).toLocaleString()}</td>
      <td>${t.suspended ? '<span class="warn">SUSPENDED</span>' : '<span class="ok">active</span>'}</td>
      <td class="meta">${(t.quotas.storage_bytes/1024/1024/1024).toFixed(0)} GiB / ${t.quotas.max_buckets} buckets</td>
      <td class="actions-col">
        <button class="warn" onclick="regenKey('${t.id}')">Regen key</button>
        <button class="secondary" onclick="suspend('${t.id}', ${!t.suspended})">${t.suspended ? "Unsuspend" : "Suspend"}</button>
        <button class="danger" onclick="deleteTenant('${t.id}', '${t.tag||""}')">Delete</button>
      </td>
    </tr>`).join("");
  $("tenants-tbl").querySelector("tbody").innerHTML = rows || '<tr><td colspan="6" class="meta">(no tenants yet)</td></tr>';
}

async function regenKey(id) {
  if (!confirm("Regenerate API key for " + id + "?\nAll existing keys for this tenant will be revoked.")) return;
  const r = await authFetch("v1/admin/tenants/" + id + "/regen-key", { method: "POST" });
  const j = await r.json();
  if (!r.ok) { alert("Failed: " + (j.error || r.status)); return; }
  alert("New key (copy now — won't be shown again):\n\n" + j.key);
  loadTenants();
}

async function suspend(id, on) {
  const op = on ? "suspend" : "unsuspend";
  if (!confirm(op + " " + id + "?")) return;
  const r = await authFetch("v1/admin/tenants/" + id + "/" + op, { method: "POST" });
  if (!r.ok) alert("Failed: " + r.status);
  loadTenants();
}

async function deleteTenant(id, tag) {
  if (!confirm("PERMANENTLY DELETE tenant " + id + " (" + tag + ")?\n\nThis revokes all keys and removes the tenant record. Buckets remain in NATS until manually cleaned.")) return;
  if (!confirm("Are you really sure?")) return;
  const r = await authFetch("v1/admin/tenants/" + id, { method: "DELETE" });
  if (!r.ok) alert("Failed: " + r.status);
  loadTenants();
}

// ---- Pending invite requests ----
async function loadInviteRequests(status) {
  const qs = status ? "?status=" + encodeURIComponent(status) : "";
  const r = await authFetch("v1/admin/invite-requests" + qs);
  const j = await r.json();
  const tb = $("pir-tbl").querySelector("tbody");
  if (!r.ok) {
    tb.innerHTML = '<tr><td colspan="6" class="err">' + (j.error || r.status) + '</td></tr>';
    $("pir-count").textContent = "";
    return;
  }
  const list = (j.requests || []).sort((a,b) => (b.created_at||"").localeCompare(a.created_at||""));
  $("pir-count").textContent = list.length ? `(${list.length})` : "";
  if (list.length === 0) {
    tb.innerHTML = '<tr><td colspan="6" class="meta">(no requests)</td></tr>';
    return;
  }
  tb.innerHTML = list.map(rq => {
    const status = rq.status || "pending";
    const statusEl = status === "pending" ? '<span class="warn">pending</span>'
      : status === "approved" ? '<span class="ok">approved</span>'
      : '<span class="err">declined</span>';
    const actions = status === "pending"
      ? `<button onclick="approveRequest('${rq.id}','${escapeHtml(rq.email)}')">Approve</button> <button class="warn" onclick="declineRequest('${rq.id}')">Decline</button>`
      : (rq.invite_token ? `<span class="meta">→ ${rq.invite_token.slice(0,18)}…</span>` : '');
    const reason = (rq.reason || "").length > 80 ? rq.reason.slice(0, 80) + "…" : (rq.reason || "");
    return `<tr>
      <td class="meta">${new Date(rq.created_at).toLocaleString()}</td>
      <td>${escapeHtml(rq.name || "")}</td>
      <td><span class="pill">${escapeHtml(rq.email || "")}</span></td>
      <td class="meta" title="${escapeHtml(rq.reason || "")}">${escapeHtml(reason)}</td>
      <td>${statusEl}</td>
      <td class="actions-col">${actions}</td>
    </tr>`;
  }).join("");
}

function escapeHtml(s) {
  return String(s||"").replace(/[&<>"']/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
}

// UI gate token used when building the share URL — pulled from a constant
// shared with the user app's spin variable. Admin keeps a copy in localStorage
// so they can rotate via `spin aka variables set ui_gate_token=...` and paste
// the new value here. Default matches the user-app default.
function uiGateToken() {
  return localStorage.getItem("nats-kv-ui-gate") || "demo-open-2026";
}

async function approveRequest(id, email) {
  if (!confirm("Approve invite request from " + email + "?\nMints a 7-day invite + access URL.")) return;
  const r = await authFetch("v1/admin/invite-requests/" + encodeURIComponent(id) + "/approve", { method: "POST" });
  const j = await r.json();
  if (!r.ok) { alert("Failed: " + (j.error || r.status)); return; }
  // Build a complete URL: include both the UI gate token (so the click bypasses the gate)
  // AND the claim path. The user app's /claim/<token> flow already runs through the gate
  // since /claim/* is in the public path whitelist, but we also append ?access= so they
  // get the unlocked UI cookie set immediately on first click.
  const userAppOrigin = "https://nats-kv.connected-cloud.io"; // production hostname (post-Akamai)
  const fwfFallback = "https://3c5be533-8e6d-423b-9962-87d9da8d16cd.fwf.app"; // direct FWF (works today before Akamai property activates)
  const claim = j.claim_path; // e.g. /claim/k_inv_abc123
  const gate = encodeURIComponent(uiGateToken());
  const akamaiUrl = `${userAppOrigin}${claim}?access=${gate}`;
  const fwfUrl = `${fwfFallback}${claim}?access=${gate}`;
  $("pir-out").innerHTML = `
    <div style="padding:10px; border:1px solid #3fb950; border-radius:4px; background:#0e2a17;">
      <b style="color:#3fb950">Approved.</b> Send this URL to the requester:
      <div class="copy" style="margin-top:6px" onclick="navigator.clipboard.writeText('${akamaiUrl}')">${akamaiUrl}</div>
      <div class="meta" style="margin-top:4px">Or via FWF direct (works pre-Akamai-property):</div>
      <div class="copy" onclick="navigator.clipboard.writeText('${fwfUrl}')">${fwfUrl}</div>
      <div class="meta" style="margin-top:4px">Click either to copy. Invite expires in 7 days.</div>
    </div>`;
  loadInviteRequests('pending');
  loadInvites(false);
}

async function declineRequest(id) {
  const note = prompt("Optional note (visible in audit log):") || "";
  const r = await authFetch("v1/admin/invite-requests/" + encodeURIComponent(id) + "/decline" + (note ? "?note=" + encodeURIComponent(note) : ""), { method: "POST" });
  if (!r.ok) { alert("Failed: " + r.status); return; }
  loadInviteRequests('pending');
}

if (token()) {
  loadTenants();
  loadInvites(false);
  loadInviteRequests('pending');
}
</script>
</body>
</html>
"##;
