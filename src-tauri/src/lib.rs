use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use base64::Engine;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Manager;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use walkdir::WalkDir;

// ─── Data Structures ───

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct FileEntry {
    name: String,
    rel_path: String,
    size: u64,
    last_modified: u64,
    ext: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct FileMove {
    src: String,
    dest: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OrganizeResult {
    moved: usize,
    failed: usize,
    errors: Vec<String>,
    conflicts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MoveLogEntry {
    original_path: String,
    new_path: String,
    file_name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MoveBatch {
    batch_id: String,
    root_folder: String,
    timestamp: u64,
    file_count: usize,
    entries: Vec<MoveLogEntry>,
}

// ─── Web Server State ───

#[derive(Clone)]
struct AppState {
    folder_path: Arc<RwLock<String>>,
    password_hash: Arc<RwLock<String>>,
    sessions: Arc<Mutex<Vec<Session>>>,
    files_cache: Arc<RwLock<Vec<FileEntry>>>,
    shutdown_tx: Arc<RwLock<Option<mpsc::Sender<()>>>>,
    port: Arc<RwLock<u16>>,
}

#[derive(Clone)]
struct Session {
    token: String,
    expires_at: std::time::Instant,
}

static APP_STATE: Lazy<AppState> = Lazy::new(|| AppState {
    folder_path: Arc::new(RwLock::new(String::new())),
    password_hash: Arc::new(RwLock::new(String::new())),
    sessions: Arc::new(Mutex::new(Vec::new())),
    files_cache: Arc::new(RwLock::new(Vec::new())),
    shutdown_tx: Arc::new(RwLock::new(None)),
    port: Arc::new(RwLock::new(8080)),
});

// ─── Auth Middleware ───

fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path().to_string();

    // Allow login endpoint without auth
    if path == "/api/auth" || path == "/api/qr" || !path.starts_with("/api/") {
        return Ok(next.run(request).await);
    }

    let token = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token = match token {
        Some(t) => t.to_string(),
        None => return Err(StatusCode::UNAUTHORIZED),
    };

    let sessions = state.sessions.lock().await;
    let valid = sessions.iter().any(|s| {
        s.token == token && std::time::Instant::now() < s.expires_at
    });

    if !valid {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}

// ─── API Handlers ───

#[derive(Deserialize)]
struct AuthRequest {
    password: String,
}

#[derive(Serialize)]
struct AuthResponse {
    token: String,
}

async fn handle_auth(
    State(state): State<AppState>,
    Json(body): Json<AuthRequest>,
) -> Result<Json<AuthResponse>, StatusCode> {
    let stored_hash = state.password_hash.read().await;
    let input_hash = hash_password(&body.password);

    if *stored_hash != input_hash {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let token = generate_token();
    let session = Session {
        token: token.clone(),
        expires_at: std::time::Instant::now() + std::time::Duration::from_secs(86400),
    };

    let mut sessions = state.sessions.lock().await;
    sessions.retain(|s| std::time::Instant::now() < s.expires_at);
    sessions.push(session);

    Ok(Json(AuthResponse { token }))
}

#[derive(Serialize)]
struct FileInfo {
    files: Vec<FileEntry>,
    folder: String,
}

async fn handle_files(
    State(state): State<AppState>,
) -> Result<Json<FileInfo>, StatusCode> {
    let folder = state.folder_path.read().await.clone();
    let files = state.files_cache.read().await.clone();
    Ok(Json(FileInfo { files, folder }))
}

#[derive(Serialize)]
struct InfoResponse {
    folder: String,
    file_count: usize,
}

async fn handle_info(
    State(state): State<AppState>,
) -> Result<Json<InfoResponse>, StatusCode> {
    let folder = state.folder_path.read().await.clone();
    let files = state.files_cache.read().await;
    Ok(Json(InfoResponse {
        folder,
        file_count: files.len(),
    }))
}

#[derive(Deserialize)]
struct OrganizeRequest {
    moves: Vec<FileMove>,
}

async fn handle_organize(
    State(state): State<AppState>,
    Json(body): Json<OrganizeRequest>,
) -> Result<Json<OrganizeResult>, StatusCode> {
    let root = state.folder_path.read().await.clone();
    let root = PathBuf::from(&root);

    if !root.is_dir() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut moved = 0usize;
    let mut failed = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for m in &body.moves {
        let src = root.join(&m.src);
        let dest = root.join(&m.dest);

        if !src.exists() {
            failed += 1;
            errors.push(format!("Missing source: {}", m.src));
            continue;
        }

        if let Some(parent) = dest.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                failed += 1;
                errors.push(format!("Cannot create {}: {}", parent.display(), e));
                continue;
            }
        }

        match fs::rename(&src, &dest) {
            Ok(_) => moved += 1,
            Err(e) => {
                failed += 1;
                errors.push(format!("Failed {} -> {}: {}", m.src, m.dest, e));
            }
        }
    }

    // Refresh cache
    let folder_str = state.folder_path.read().await.clone();
    let files = scan_folder_internal(&folder_str);
    *state.files_cache.write().await = files;

    Ok(Json(OrganizeResult { moved, failed, errors, conflicts: Vec::new() }))
}

async fn handle_preview(
    State(state): State<AppState>,
    axum::extract::Path(rel_path): axum::extract::Path<String>,
) -> Result<Response, StatusCode> {
    let folder = state.folder_path.read().await.clone();
    let full_path = PathBuf::from(&folder).join(&rel_path);

    if !full_path.exists() || !full_path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }

    // Security: ensure the path is within the folder
    if let Ok(canonical) = full_path.canonicalize() {
        let folder_canonical = PathBuf::from(&folder)
            .canonicalize()
            .unwrap_or_default();
        if !canonical.starts_with(&folder_canonical) {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let ext = full_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "md" | "json" | "js" | "ts" | "html" | "css" | "xml" | "csv" => "text/plain",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    };

    match fs::read(&full_path) {
        Ok(bytes) => Ok(
            Response::builder()
                .header(header::CONTENT_TYPE, mime)
                .header(header::CACHE_CONTROL, "public, max-age=3600")
                .body(axum::body::Body::from(bytes))
                .unwrap(),
        ),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn handle_qr(State(state): State<AppState>) -> impl IntoResponse {
    let port = *state.port.read().await;
    let folder = state.folder_path.read().await.clone();

    // Get local IP
    let local_ip = get_local_ip();
    let url = format!("http://{}:{}", local_ip, port);

    let qr = qrcode::QrCode::new(&url).unwrap();
    let image = qr.render::<qrcode::render::unicode::Dense1x2>().module_dimensions(2, 2).build();

    Html(format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>QR Code</title>
<style>body{{font-family:monospace;display:flex;flex-direction:column;align-items:center;justify-content:center;height:100vh;margin:0;background:#F2EAD8;color:#20291F;}}
pre{{font-size:8px;line-height:1;margin:20px 0;}}
p{{font-size:14px;}}</style></head>
<body><h2>Scan to connect</h2><pre>{}</pre><p>{}</p><p><small>Folder: {}</small></p></body></html>"#,
        image, url, folder
    ))
}

// ─── Embedded Remote Web UI ───

const REMOTE_UI: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>deskmatee — Remote</title>
<style>
:root{--paper:#F2EAD8;--paper-deep:#E8DCC0;--manila:#E4BE7F;--manila-dark:#C99A5A;--ink:#20291F;--ink-soft:#4B5245;--rust:#BD4B28;--forest:#33533D;--line:#c9b98d;--card:#FBF6E9;--shadow:3px 4px 0 rgba(32,41,31,0.18);}
*{box-sizing:border-box;margin:0;}
body{background:var(--paper);color:var(--ink);font-family:'Segoe UI',system-ui,sans-serif;min-height:100vh;padding:16px;}
.wrap{max-width:900px;margin:0 auto;}
header{border-bottom:3px solid var(--ink);padding-bottom:12px;margin-bottom:16px;display:flex;align-items:center;justify-content:space-between;gap:12px;}
h1{font-size:24px;font-weight:700;}
h1 em{color:var(--rust);font-style:italic;}
.folder-tag{font-size:11px;color:var(--ink-soft);background:var(--paper-deep);padding:3px 8px;border-radius:4px;}
.stats{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:14px;font-size:12px;}
.chip{background:var(--paper-deep);border:1px solid var(--line);padding:4px 10px;border-radius:16px;}
.chip.warn{background:#F4DCC7;border-color:var(--rust);color:var(--rust);}
.toolbar{display:flex;gap:8px;margin-bottom:12px;flex-wrap:wrap;}
.search{flex:1;min-width:200px;padding:10px 14px;border:2px solid var(--ink);border-radius:4px;font-size:14px;background:var(--card);}
.cat-tabs{display:flex;gap:6px;flex-wrap:wrap;margin-bottom:14px;}
.cat-tab{padding:6px 14px;border:2px solid var(--ink);border-radius:4px;font-size:12px;cursor:pointer;background:var(--manila);}
.cat-tab.active{background:var(--card);border-bottom-color:var(--rust);}
.file-list{display:flex;flex-direction:column;gap:6px;}
.file{background:var(--card);border:1px solid var(--line);border-radius:4px;padding:10px 14px;display:grid;grid-template-columns:1fr auto auto auto;gap:12px;align-items:center;font-size:13px;}
.file .name{font-weight:500;word-break:break-word;}
.file .path{font-size:10px;color:var(--ink-soft);margin-top:2px;}
.file .tag{font-size:10px;background:var(--manila);padding:2px 8px;border-radius:12px;white-space:nowrap;}
.file .size{font-size:11px;color:var(--ink-soft);white-space:nowrap;}
.file .date{font-size:11px;color:var(--ink-soft);white-space:nowrap;}
.file .actions{display:flex;gap:4px;}
.file .actions button{padding:4px 8px;font-size:11px;}
.btn{padding:8px 16px;border:2px solid var(--ink);border-radius:4px;cursor:pointer;font-size:13px;font-weight:500;}
.btn-primary{background:var(--ink);color:var(--paper);box-shadow:2px 2px 0 var(--rust);}
.btn-sm{padding:4px 10px;font-size:11px;}
.btn-ghost{background:transparent;color:var(--ink);}
.organize-bar{margin-top:16px;background:var(--forest);color:var(--paper);border-radius:4px;padding:14px 18px;display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:10px;}
.organize-bar h3{font-size:16px;}
.organize-bar p{font-size:12px;opacity:.85;}
.modal-overlay{position:fixed;inset:0;background:rgba(32,41,31,0.6);display:none;align-items:center;justify-content:center;z-index:100;}
.modal-overlay.open{display:flex;}
.modal{background:var(--card);border:2px solid var(--ink);border-radius:4px;padding:24px;width:320px;max-width:90vw;}
.modal h3{margin-bottom:14px;}
.modal input{width:100%;padding:10px;border:2px solid var(--ink);border-radius:4px;font-size:14px;margin-bottom:12px;}
.empty{text-align:center;padding:40px;color:var(--ink-soft);font-size:13px;}
.preview-overlay{position:fixed;inset:0;background:rgba(0,0,0,0.85);display:none;align-items:center;justify-content:center;z-index:200;}
.preview-overlay.open{display:flex;}
.preview-overlay img,.preview-overlay video,.preview-overlay audio,.preview-overlay object{max-width:90vw;max-height:90vh;}
.preview-close{position:fixed;top:16px;right:16px;color:white;font-size:24px;cursor:pointer;z-index:201;background:rgba(0,0,0,0.5);width:40px;height:40px;border-radius:50%;display:flex;align-items:center;justify-content:center;}
@media(max-width:600px){.file{grid-template-columns:1fr auto;}.file .date,.file .tag{display:none;}}
</style>
</head>
<body>
<div class="wrap" id="app" style="display:none;">
  <header>
    <h1>deskmatee</h1>
    <span class="folder-tag" id="folderTag"></span>
  </header>
  <div class="stats" id="stats"></div>
  <div class="cat-tabs" id="catTabs"></div>
  <div class="toolbar">
    <input class="search" id="search" placeholder="Search files...">
  </div>
  <div class="file-list" id="fileList"></div>
  <div class="organize-bar" id="organizeBar" style="display:none;">
    <div><h3>Organize Files</h3><p>Move all files into category folders</p></div>
    <button class="btn" onclick="doOrganize()">Organize Now</button>
  </div>
</div>

<!-- Login Modal -->
<div class="modal-overlay open" id="loginModal">
  <div class="modal">
    <h3>Enter Password</h3>
    <input type="password" id="loginPass" placeholder="Password" autofocus>
    <button class="btn btn-primary" onclick="doLogin()" style="width:100%;">Connect</button>
    <p id="loginError" style="color:var(--rust);font-size:12px;margin-top:8px;display:none;"></p>
  </div>
</div>

<!-- Preview -->
<div class="preview-overlay" id="previewOverlay" onclick="closePreview()">
  <div class="preview-close" onclick="closePreview()">✕</div>
  <div id="previewContent"></div>
</div>

<script>
let token = localStorage.getItem('fds_token');
let files = [];
let activeCat = 'All';

async function doLogin() {
  const pass = document.getElementById('loginPass').value;
  try {
    const r = await fetch('/api/auth', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({password: pass})
    });
    if (!r.ok) throw new Error('Wrong password');
    const data = await r.json();
    token = data.token;
    localStorage.setItem('fds_token', token);
    document.getElementById('loginModal').classList.remove('open');
    loadFiles();
  } catch(e) {
    const err = document.getElementById('loginError');
    err.textContent = 'Wrong password. Try again.';
    err.style.display = 'block';
  }
}

document.getElementById('loginPass').addEventListener('keydown', e => {
  if (e.key === 'Enter') doLogin();
});

async function apiFetch(url, opts = {}) {
  opts.headers = opts.headers || {};
  opts.headers['Authorization'] = 'Bearer ' + token;
  return fetch(url, opts);
}

async function loadFiles() {
  try {
    const r = await apiFetch('/api/files');
    if (r.status === 401) { document.getElementById('loginModal').classList.add('open'); return; }
    const data = await r.json();
    files = data.files.map(f => ({
      ...f,
      category: categorize(f.ext),
      tag: smartTag(f.name)
    }));
    document.getElementById('folderTag').textContent = data.folder;
    render();
    document.getElementById('app').style.display = 'block';
  } catch(e) { console.error(e); }
}

function categorize(ext) {
  const rules = [
    ['PDFs',['pdf']],['Documents',['doc','docx','txt','rtf','odt']],['Spreadsheets',['xls','xlsx','csv','ods']],
    ['Presentations',['ppt','pptx','odp']],['Images',['jpg','jpeg','png','gif','bmp','svg','webp','heic','tif','tiff']],
    ['Videos',['mp4','mov','avi','mkv','wmv','m4v']],['Audio',['mp3','wav','aac','flac','m4a']],
    ['Archives',['zip','rar','7z','tar','gz']],['Code & Data',['js','ts','py','html','css','json','java','cpp','c','php','xml','sql']],
    ['Installers',['exe','msi','apk','dmg']]
  ];
  for (const [name, exts] of rules) if (exts.includes(ext)) return name;
  return 'Other';
}

function smartTag(name) {
  const lower = name.toLowerCase();
  const tags = [
    ['Finance',['invoice','receipt','budget','payroll','expense','financial','accounting']],
    ['HR',['resume','cv','contract','employment','leave','payslip']],
    ['Legal',['agreement','contract','nda','legal','memo']],
    ['Reports',['report','minutes','summary','narrative']],
    ['Presentation',['deck','presentation','pitch']]
  ];
  for (const [tag, words] of tags) if (words.some(w => lower.includes(w))) return tag;
  return null;
}

function fmtSize(b) {
  if (b < 1024) return b + ' B';
  if (b < 1048576) return (b/1024).toFixed(1) + ' KB';
  if (b < 1073741824) return (b/1048576).toFixed(1) + ' MB';
  return (b/1073741824).toFixed(2) + ' GB';
}

function fmtDate(ts) {
  return new Date(Number(ts)).toLocaleDateString('en-US', {month:'short',day:'numeric',year:'numeric'});
}

function render() {
  const q = document.getElementById('search').value.toLowerCase();
  const filtered = files.filter(f =>
    (activeCat === 'All' || f.category === activeCat) &&
    (!q || f.name.toLowerCase().includes(q) || f.category.toLowerCase().includes(q) || (f.tag && f.tag.toLowerCase().includes(q)))
  );
  filtered.sort((a,b) => Number(b.lastModified) - Number(a.lastModified));

  // Stats
  const cats = [...new Set(files.map(f => f.category))];
  const totalSize = files.reduce((a,f) => a + Number(f.size), 0);
  document.getElementById('stats').innerHTML =
    `<span class="chip">${files.length} files</span><span class="chip">${fmtSize(totalSize)} total</span><span class="chip">${cats.length} categories</span>`;

  // Category tabs
  const counts = {};
  files.forEach(f => counts[f.category] = (counts[f.category]||0)+1);
  const sorted = Object.entries(counts).sort((a,b) => b[1]-a[1]);
  document.getElementById('catTabs').innerHTML =
    `<div class="cat-tab ${activeCat==='All'?'active':''}" onclick="setCat('All')">All (${files.length})</div>` +
    sorted.map(([c,n]) => `<div class="cat-tab ${activeCat===c?'active':''}" onclick="setCat('${c}')">${c} (${n})</div>`).join('');

  // File list
  const el = document.getElementById('fileList');
  if (!filtered.length) { el.innerHTML = '<div class="empty">No files match.</div>'; return; }
  el.innerHTML = filtered.slice(0, 500).map(f => `
    <div class="file">
      <div><div class="name">${esc(f.name)}</div><div class="path">${esc(f.rel_path)}</div></div>
      <span class="tag">${f.tag || f.category}</span>
      <span class="date">${fmtDate(f.lastModified)}</span>
      <span class="size">${fmtSize(Number(f.size))}</span>
    </div>
  `).join('');

  document.getElementById('organizeBar').style.display = files.length ? 'flex' : 'none';
}

function setCat(c) { activeCat = c; render(); }
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }

document.getElementById('search').addEventListener('input', render);

async function doOrganize() {
  if (!confirm('Organize ' + files.length + ' files into category folders?')) return;
  const moves = files.map(f => ({
    src: f.rel_path,
    dest: (f.tag ? f.category + '/' + f.tag : f.category) + '/' + f.name
  }));
  try {
    const r = await apiFetch('/api/organize', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({moves})
    });
    const res = await r.json();
    alert('Done! Moved ' + res.moved + ' files.' + (res.failed ? ' ' + res.failed + ' failed.' : ''));
    loadFiles();
  } catch(e) { alert('Error: ' + e); }
}

function closePreview() { document.getElementById('previewOverlay').classList.remove('open'); }

// Init
if (token) { loadFiles(); } else { document.getElementById('loginModal').classList.add('open'); }
</script>
</body>
</html>"#;

// ─── Tauri Commands ───

#[tauri::command]
async fn ai_chat(messages: Vec<ChatMessage>, api_key: String, model: String, provider: String) -> Result<String, String> {
    if api_key.is_empty() {
        let provider_name = match provider.as_str() {
            "openai" => "OpenAI",
            "claude" => "Claude",
            "gemini" => "Gemini",
            _ => "Groq",
        };
        return Err(format!("API key is required. Open settings to enter your {} API key.", provider_name));
    }

    let client = reqwest::Client::new();

    match provider.as_str() {
        "openai" => chat_openai(&client, &api_key, &model, &messages).await,
        "claude" => chat_claude(&client, &api_key, &model, &messages).await,
        "gemini" => chat_gemini(&client, &api_key, &model, &messages).await,
        _ => chat_groq(&client, &api_key, &model, &messages).await,
    }
}

async fn chat_groq(client: &reqwest::Client, api_key: &str, model: &str, messages: &[ChatMessage]) -> Result<String, String> {
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": 0.7,
        "max_tokens": 2048,
    });

    let resp = client
        .post("https://api.groq.com/openai/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Network error: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Failed to read response: {}", e))?;

    if !status.is_success() {
        return extract_error(&text, status);
    }

    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("Parse error: {}", e))?;
    v["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No response from model".into())
}

async fn chat_openai(client: &reqwest::Client, api_key: &str, model: &str, messages: &[ChatMessage]) -> Result<String, String> {
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": 0.7,
        "max_tokens": 2048,
    });

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Network error: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Failed to read response: {}", e))?;

    if !status.is_success() {
        return extract_error(&text, status);
    }

    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("Parse error: {}", e))?;
    v["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No response from model".into())
}

async fn chat_claude(client: &reqwest::Client, api_key: &str, model: &str, messages: &[ChatMessage]) -> Result<String, String> {
    // Extract system message (must be separate in Claude API)
    let mut system_text = String::new();
    let mut claude_messages: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        if msg.role == "system" {
            if !system_text.is_empty() {
                system_text.push('\n');
            }
            system_text.push_str(&msg.content);
        } else {
            claude_messages.push(serde_json::json!({
                "role": msg.role,
                "content": msg.content,
            }));
        }
    }

    // Ensure messages alternate user/assistant starting with user
    // Claude requires at least one message, and first must be user
    if claude_messages.is_empty() {
        claude_messages.push(serde_json::json!({
            "role": "user",
            "content": "Hello",
        }));
    }

    let mut body = serde_json::json!({
        "model": model,
        "messages": claude_messages,
        "max_tokens": 2048,
    });

    if !system_text.is_empty() {
        body["system"] = serde_json::json!(system_text);
    }

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Network error: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Failed to read response: {}", e))?;

    if !status.is_success() {
        return extract_error(&text, status);
    }

    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("Parse error: {}", e))?;
    v["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No response from model".into())
}

fn resolve_gemini_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return "gemini-flash-latest".to_string();
    }

    match trimmed {
        "gemini-2.0-flash" | "gemini-2.5-flash" | "gemini-1.5-pro" | "gemini-3.5-flash" | "gemini-flash-latest" => "gemini-flash-latest".to_string(),
        _ => trimmed.to_string(),
    }
}

async fn chat_gemini(client: &reqwest::Client, api_key: &str, model: &str, messages: &[ChatMessage]) -> Result<String, String> {
    let mut contents: Vec<serde_json::Value> = Vec::new();
    let mut system_instruction = serde_json::json!({});
    let resolved_model = resolve_gemini_model(model);

    for msg in messages {
        if msg.role == "system" {
            system_instruction = serde_json::json!({
                "parts": [{ "text": msg.content }]
            });
        } else {
            // Gemini uses "model" instead of "assistant"
            let role = if msg.role == "assistant" { "model" } else { &msg.role };
            contents.push(serde_json::json!({
                "role": role,
                "parts": [{ "text": msg.content }],
            }));
        }
    }

    // Ensure at least one content
    if contents.is_empty() {
        contents.push(serde_json::json!({
            "role": "user",
            "parts": [{ "text": "Hello" }],
        }));
    }

    let mut body = serde_json::json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": 2048,
            "temperature": 0.7,
        },
    });

    if !system_instruction.get("parts").is_none() {
        body["systemInstruction"] = system_instruction;
    }

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
        resolved_model
    );

    let resp = client
        .post(&url)
        .header("x-goog-api-key", api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Network error: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Failed to read response: {}", e))?;

    if !status.is_success() {
        return extract_error(&text, status);
    }

    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("Parse error: {}", e))?;
    v["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No response from model".into())
}

fn extract_error(text: &str, status: reqwest::StatusCode) -> Result<String, String> {
    if let Ok(err) = serde_json::from_str::<serde_json::Value>(text) {
        // Try common error formats
        let msg = err["error"]["message"]
            .as_str()
            .or_else(|| err["error"]["details"][0]["message"].as_str())
            .or_else(|| err["message"].as_str())
            .unwrap_or("Unknown API error");
        Err(format!("{} (status {})", msg, status))
    } else {
        Err(format!("API error (status {})", status))
    }
}

fn scan_folder_internal(path: &str) -> Vec<FileEntry> {
    let root = PathBuf::from(path);
    if !root.is_dir() {
        return Vec::new();
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(&root).min_depth(1).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        let rel = match p.strip_prefix(&root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let meta = match fs::metadata(p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let last_modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        files.push(FileEntry {
            name,
            rel_path: rel,
            size: meta.len(),
            last_modified,
            ext,
        });
    }
    files
}

#[tauri::command]
fn scan_folder(path: String) -> Result<Vec<FileEntry>, String> {
    let root = PathBuf::from(&path);
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }
    Ok(scan_folder_internal(&path))
}

fn move_history_path(app_handle: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let mut dir = app_handle.path().app_data_dir().map_err(|e| format!("Data dir error: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create data dir: {}", e))?;
    dir.push("move_history.json");
    Ok(dir)
}

fn read_move_history(path: &std::path::Path) -> Vec<MoveBatch> {
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_move_history(path: &std::path::Path, batches: &[MoveBatch]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(batches).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

fn generate_batch_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("batch_{}", ts)
}

#[tauri::command]
fn organize_files(app_handle: tauri::AppHandle, root: String, moves: Vec<FileMove>, dry_run: bool, conflict_strategy: String) -> Result<OrganizeResult, String> {
    let root = PathBuf::from(&root);
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()));
    }

    let mut moved = 0usize;
    let mut failed = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut log_entries: Vec<MoveLogEntry> = Vec::new();

    for m in moves {
        let src = root.join(&m.src);
        let dest = root.join(&m.dest);
        if !src.exists() {
            failed += 1;
            errors.push(format!("Missing source: {}", m.src));
            continue;
        }
        if dest.exists() && !dry_run && conflict_strategy == "skip" {
            failed += 1;
            errors.push(format!("Conflict skipped: {} already exists", m.dest));
            continue;
        }
        let final_dest = if dest.exists() && !dry_run && conflict_strategy == "rename" {
            let resolved = resolve_conflict(&dest);
            let name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            conflicts.push(format!("{} -> {}", m.dest, name));
            resolved
        } else {
            dest.clone()
        };
        if let Some(parent) = final_dest.parent() {
            if !dry_run {
                if let Err(e) = fs::create_dir_all(parent) {
                    failed += 1;
                    errors.push(format!("Cannot create {}: {}", parent.display(), e));
                    continue;
                }
            }
        }
        if dry_run {
            moved += 1;
            continue;
        }
        match fs::rename(&src, &final_dest) {
            Ok(_) => {
                moved += 1;
                log_entries.push(MoveLogEntry {
                    original_path: m.src.clone(),
                    new_path: m.dest.clone(),
                    file_name: src.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string(),
                });
            }
            Err(e) => {
                failed += 1;
                errors.push(format!("Failed {} -> {}: {}", m.src, m.dest, e));
            }
        }
    }

    // Write move history if any files were moved
    if !dry_run && !log_entries.is_empty() {
        if let Ok(path) = move_history_path(&app_handle) {
            let mut history = read_move_history(&path);
            let batch = MoveBatch {
                batch_id: generate_batch_id(),
                root_folder: root.to_string_lossy().to_string(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                file_count: log_entries.len(),
                entries: log_entries,
            };
            history.push(batch);
            // Keep max 50 batches
            if history.len() > 50 {
                history.drain(..history.len() - 50);
            }
            let _ = write_move_history(&path, &history);
        }
    }

    Ok(OrganizeResult { moved, failed, errors, conflicts })
}

fn resolve_conflict(dest: &std::path::Path) -> std::path::PathBuf {
    let parent = dest.parent().unwrap();
    let stem = dest.file_stem().unwrap().to_string_lossy().to_string();
    let ext = dest.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut counter = 1;
    loop {
        let candidate = parent.join(format!("{} ({}){}", stem, counter, ext));
        if !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}

#[derive(Debug, Serialize)]
struct UndoResult {
    restored: usize,
    failed: usize,
    errors: Vec<String>,
}

#[tauri::command]
fn undo_batch(app_handle: tauri::AppHandle, batch_id: String) -> Result<UndoResult, String> {
    let path = move_history_path(&app_handle)?;
    let mut history = read_move_history(&path);
    let batch_idx = history.iter().position(|b| b.batch_id == batch_id)
        .ok_or_else(|| format!("Batch not found: {}", batch_id))?;
    let batch = &history[batch_idx];

    let root = PathBuf::from(&batch.root_folder);
    let mut restored = 0usize;
    let mut failed = 0usize;
    let mut errors: Vec<String> = Vec::new();

    // Reverse in reverse order to restore original state
    for entry in batch.entries.iter().rev() {
        let original = root.join(&entry.original_path);
        let current = root.join(&entry.new_path);

        if !current.exists() {
            failed += 1;
            errors.push(format!("Cannot undo {}: file no longer at expected location", entry.new_path));
            continue;
        }

        if let Some(parent) = original.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                failed += 1;
                errors.push(format!("Cannot create parent dir for {}: {}", entry.original_path, e));
                continue;
            }
        }

        match fs::rename(&current, &original) {
            Ok(_) => restored += 1,
            Err(e) => {
                failed += 1;
                errors.push(format!("Failed to restore {}: {}", entry.original_path, e));
            }
        }
    }

    // Remove batch from history
    if failed == 0 {
        history.remove(batch_idx);
        let _ = write_move_history(&path, &history);
    }

    Ok(UndoResult { restored, failed, errors })
}

#[tauri::command]
fn get_move_history(app_handle: tauri::AppHandle) -> Result<Vec<MoveBatch>, String> {
    let path = move_history_path(&app_handle)?;
    Ok(read_move_history(&path))
}

#[tauri::command]
async fn start_sharing(folder: String, password: String, port: u16) -> Result<String, String> {
    let state = &*APP_STATE;

    // Set state
    *state.folder_path.write().await = folder.clone();
    *state.password_hash.write().await = hash_password(&password);
    *state.port.write().await = port;

    // Scan files
    let files = scan_folder_internal(&folder);
    *state.files_cache.write().await = files.clone();

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    *state.shutdown_tx.write().await = Some(shutdown_tx);

    let app_state = state.clone();

    let router = Router::new()
        .route("/", get(handle_index))
        .route("/api/auth", post(handle_auth))
        .route("/api/files", get(handle_files))
        .route("/api/info", get(handle_info))
        .route("/api/organize", post(handle_organize))
        .route("/api/preview/{*path}", get(handle_preview))
        .route("/api/qr", get(handle_qr))
        .layer(middleware::from_fn_with_state(app_state.clone(), auth_middleware))
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("Failed to bind to port {}: {}", port, e))?;

    let local_ip = get_local_ip();
    let url = format!("http://{}:{}", local_ip, port);

    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                shutdown_rx.recv().await;
            })
            .await
            .ok();
    });

    Ok(url)
}

#[tauri::command]
async fn stop_sharing() -> Result<(), String> {
    let state = &*APP_STATE;
    if let Some(tx) = state.shutdown_tx.write().await.take() {
        let _ = tx.send(()).await;
    }
    let mut sessions = state.sessions.lock().await;
    sessions.clear();
    Ok(())
}

#[tauri::command]
async fn get_sharing_info() -> Result<serde_json::Value, String> {
    let state = &*APP_STATE;
    let folder = state.folder_path.read().await.clone();
    let port = *state.port.read().await;
    let files = state.files_cache.read().await;
    let local_ip = get_local_ip();
    let is_running = state.shutdown_tx.read().await.is_some();

    Ok(serde_json::json!({
        "running": is_running,
        "url": if is_running { format!("http://{}:{}", local_ip, port) } else { String::new() },
        "port": port,
        "folder": folder,
        "fileCount": files.len(),
        "localIp": local_ip,
    }))
}

async fn handle_index() -> Html<&'static str> {
    Html(REMOTE_UI)
}

// ─── File Preview + System Open ───

#[derive(Serialize)]
struct PreviewResult {
    mime_type: String,
    size: u64,
    data: String,
    is_text: bool,
    extension: String,
    name: String,
}

fn mime_for_extension(ext: &str) -> &str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "txt" | "md" | "json" | "js" | "ts" | "html" | "css" | "xml" |
        "csv" | "rs" | "py" | "toml" | "yaml" | "yml" | "sh" | "bash" |
        "java" | "cpp" | "c" | "h" | "hpp" | "php" | "rb" | "go" |
        "sql" | "ini" | "cfg" | "conf" | "log" | "env" | "gitignore" |
        "dockerfile" | "makefile" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn is_text_extension(ext: &str) -> bool {
    matches!(ext, "txt" | "md" | "json" | "js" | "ts" | "html" | "css" | "xml" |
        "csv" | "rs" | "py" | "toml" | "yaml" | "yml" | "sh" | "bash" |
        "java" | "cpp" | "c" | "h" | "hpp" | "php" | "rb" | "go" |
        "sql" | "ini" | "cfg" | "conf" | "log" | "env" | "gitignore" |
        "dockerfile" | "makefile" | "jsx" | "tsx" | "vue" | "svelte")
}

#[tauri::command]
fn read_file_preview(root: String, rel_path: String) -> Result<PreviewResult, String> {
    let full_path = PathBuf::from(&root).join(&rel_path);

    // Security: prevent directory traversal
    let canonical = full_path.canonicalize().map_err(|e| format!("Invalid path: {}", e))?;
    let root_canonical = PathBuf::from(&root).canonicalize().map_err(|e| format!("Invalid root: {}", e))?;
    if !canonical.starts_with(&root_canonical) {
        return Err("Access denied: path outside root folder".into());
    }

    if !full_path.exists() || !full_path.is_file() {
        return Err("File not found".into());
    }

    let ext = full_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let name = full_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let meta = fs::metadata(&full_path).map_err(|e| format!("Metadata error: {}", e))?;
    let mime = mime_for_extension(&ext).to_string();
    let is_text = is_text_extension(&ext);
    let bytes = fs::read(&full_path).map_err(|e| format!("Read error: {}", e))?;

    // Cap at 10MB for preview
    let capped = bytes.len() > 10 * 1024 * 1024;
    let data = if capped {
        base64::engine::general_purpose::STANDARD.encode(&bytes[..10 * 1024 * 1024])
    } else {
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    };

    Ok(PreviewResult {
        mime_type: mime.to_string(),
        size: meta.len(),
        data,
        is_text,
        extension: ext,
        name,
    })
}

#[tauri::command]
fn open_with_system(path: String) -> Result<(), String> {
    use std::path::PathBuf;
    // Security: basic check
    if path.contains("..") {
        return Err("Invalid path".into());
    }
    let full_path = PathBuf::from(&path);
    open::that(&full_path).map_err(|e| format!("Failed to open file: {}", e))
}

// ─── Helpers ───

fn get_local_ip() -> String {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok();
    if let Some(s) = socket {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Some(addr) = s.local_addr().ok() {
                return addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}

// ─── Entry Point ───

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            scan_folder,
            organize_files,
            undo_batch,
            get_move_history,
            ai_chat,
            start_sharing,
            stop_sharing,
            get_sharing_info,
            read_file_preview,
            open_with_system,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
