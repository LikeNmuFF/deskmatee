use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response, Json};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use once_cell::sync::Lazy;
use qrcode::render::Svg;
use qrcode::QrCode;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use walkdir::WalkDir;

// ─────────────────────────────────────────────
// AI Companion types
// ─────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct GroqRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f64,
    max_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct GroqChoice {
    message: GroqMessage,
}

#[derive(Debug, Deserialize)]
struct GroqMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct GroqResponse {
    choices: Vec<GroqChoice>,
}

#[tauri::command]
async fn groq_chat(
    messages: Vec<ChatMessage>,
    api_key: String,
    model: String,
) -> Result<String, String> {
    if api_key.is_empty() {
        return Err("API key is required. Open settings to enter your Groq API key.".into());
    }

    let client = reqwest::Client::new();
    let body = GroqRequest {
        model,
        messages,
        temperature: 0.7,
        max_tokens: 2048,
    };

    let resp = client
        .post("https://api.groq.com/openai/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Network error: {}", e))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    if !status.is_success() {
        let msg = if let Ok(err) = serde_json::from_str::<serde_json::Value>(&text) {
            err["error"]["message"]
                .as_str()
                .unwrap_or("Unknown API error")
                .to_string()
        } else {
            format!("API error (status {})", status)
        };
        return Err(msg);
    }

    let groq_resp: GroqResponse =
        serde_json::from_str(&text).map_err(|e| format!("Parse error: {}", e))?;

    groq_resp
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .ok_or_else(|| "No response from model".into())
}

// ─────────────────────────────────────────────
// File organizer types
// ─────────────────────────────────────────────

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
}

#[tauri::command]
fn scan_folder(path: String) -> Result<Vec<FileEntry>, String> {
    let root = PathBuf::from(&path);
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }

    let mut files: Vec<FileEntry> = Vec::new();

    for entry in WalkDir::new(&root)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
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

    Ok(files)
}

#[tauri::command]
fn organize_files(
    root: String,
    moves: Vec<FileMove>,
    dry_run: bool,
) -> Result<OrganizeResult, String> {
    let root = PathBuf::from(&root);
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()));
    }

    let mut moved = 0usize;
    let mut failed = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for m in moves {
        let src = root.join(&m.src);
        let dest = root.join(&m.dest);

        if !src.exists() {
            failed += 1;
            errors.push(format!("Missing source: {}", m.src));
            continue;
        }

        if let Some(parent) = dest.parent() {
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

        match fs::rename(&src, &dest) {
            Ok(_) => moved += 1,
            Err(e) => {
                failed += 1;
                errors.push(format!("Failed {} -> {}: {}", m.src, m.dest, e));
            }
        }
    }

    Ok(OrganizeResult {
        moved,
        failed,
        errors,
    })
}

// ─────────────────────────────────────────────
// WiFi Sharing — Shared state
// ─────────────────────────────────────────────

struct Session {
    password_hash: String,
    created_at: std::time::Instant,
}

struct ServerState {
    folder_path: RwLock<String>,
    session: RwLock<Option<Session>>,
    shutdown_tx: RwLock<Option<mpsc::Sender<()>>>,
}

type SharedState = Arc<ServerState>;

fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

fn get_local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

// ─────────────────────────────────────────────
// WiFi Sharing — Auth middleware
// ─────────────────────────────────────────────

async fn auth_middleware(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path().to_string();

    let public_paths = ["/api/auth", "/"];
    if public_paths.iter().any(|p| path == *p) || path.starts_with("/api/qr") {
        return Ok(next.run(request).await);
    }

    let session = state.session.read().await;
    if session.is_none() {
        return Ok(next.run(request).await);
    }

    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match auth_header {
        Some(token) => {
            drop(session);
            let session = state.session.read().await;
            if let Some(ref s) = *session {
                if s.created_at.elapsed().as_secs() > 86400 {
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
            let _ = token;
            Ok(next.run(request).await)
        }
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

// ─────────────────────────────────────────────
// WiFi Sharing — API handlers
// ─────────────────────────────────────────────

#[derive(Deserialize)]
struct AuthRequest {
    password: String,
}

#[derive(Serialize)]
struct AuthResponse {
    token: String,
}

#[derive(Serialize)]
struct FileInfo {
    files: Vec<FileEntry>,
    categories: HashMap<String, usize>,
    total_size: u64,
    folder: String,
}

async fn api_auth(
    State(state): State<SharedState>,
    Json(body): Json<AuthRequest>,
) -> Result<Json<AuthResponse>, StatusCode> {
    let session = state.session.read().await;
    match &*session {
        Some(s) => {
            if hash_password(&body.password) == s.password_hash {
                let token = generate_token();
                Ok(Json(AuthResponse { token }))
            } else {
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        None => {
            let token = generate_token();
            Ok(Json(AuthResponse { token }))
        }
    }
}

async fn api_files(State(state): State<SharedState>) -> Result<Json<FileInfo>, String> {
    let folder = state.folder_path.read().await.clone();
    let files = scan_folder(folder.clone())?;
    let mut categories: HashMap<String, usize> = HashMap::new();
    let mut total_size = 0u64;
    for f in &files {
        let cat = categorize_ext(&f.ext);
        *categories.entry(cat).or_insert(0) += 1;
        total_size += f.size;
    }
    Ok(Json(FileInfo {
        files,
        categories,
        total_size,
        folder,
    }))
}

#[derive(Deserialize)]
struct OrganizeRequest {
    moves: Vec<FileMove>,
}

async fn api_organize(
    State(state): State<SharedState>,
    Json(body): Json<OrganizeRequest>,
) -> Result<Json<OrganizeResult>, String> {
    let folder = state.folder_path.read().await.clone();
    organize_files(folder, body.moves, false).map(Json)
}

async fn api_preview(
    State(state): State<SharedState>,
    axum::extract::Path(rel_path): axum::extract::Path<String>,
) -> Result<Response, StatusCode> {
    let folder = state.folder_path.read().await.clone();
    let full_path = PathBuf::from(&folder).join(&rel_path);

    if !full_path.starts_with(&folder) || !full_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let mime = match full_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "md" | "json" | "xml" | "csv" => "text/plain",
        "html" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        _ => "application/octet-stream",
    };

    match fs::read(&full_path) {
        Ok(bytes) => Ok(Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .body(Body::from(bytes))
            .unwrap()),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_qr(
    State(state): State<SharedState>,
) -> Result<String, StatusCode> {
    let folder = state.folder_path.read().await.clone();
    let ip = get_local_ip().unwrap_or_else(|| "localhost".into());
    let url = format!("http://{}:8080", ip);

    let code = QrCode::new(url.as_bytes()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let svg = code
        .render::<Svg>()
        .module_color(0x20291F_u8)
        .background_color(0xFBF6E9_u8)
        .quiet_zone(2)
        .build();
    Ok(svg)
}

async fn api_server_info(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let ip = get_local_ip().unwrap_or_else(|| "localhost".into());
    let has_password = state.session.read().await.is_some();
    Ok(Json(serde_json::json!({
        "ip": ip,
        "port": 8080,
        "url": format!("http://{}:8080", ip),
        "has_password": has_password,
    })))
}

// ─────────────────────────────────────────────
// WiFi Sharing — Remote UI
// ─────────────────────────────────────────────

fn categorize_ext(ext: &str) -> String {
    match ext.to_lowercase().as_str() {
        "pdf" => "PDFs".into(),
        "doc" | "docx" | "txt" | "rtf" | "odt" => "Documents".into(),
        "xls" | "xlsx" | "csv" | "ods" => "Spreadsheets".into(),
        "ppt" | "pptx" | "odp" => "Presentations".into(),
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "svg" | "webp" | "heic" | "tif" | "tiff" => "Images".into(),
        "mp4" | "mov" | "avi" | "mkv" | "wmv" | "m4v" => "Videos".into(),
        "mp3" | "wav" | "aac" | "flac" | "m4a" => "Audio".into(),
        "zip" | "rar" | "7z" | "tar" | "gz" => "Archives".into(),
        "js" | "ts" | "py" | "html" | "css" | "json" | "java" | "cpp" | "c" | "php" | "xml" | "sql" => "Code & Data".into(),
        "exe" | "msi" | "apk" | "dmg" => "Installers".into(),
        _ => "Other".into(),
    }
}

static REMOTE_UI: Lazy<String> = Lazy::new(|| {
    r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>The Filing Desk — Remote</title>
<style>
:root{--paper:#F2EAD8;--manila:#E4BE7F;--ink:#20291F;--ink-soft:#4B5245;--rust:#BD4B28;--forest:#33533D;--line:#c9b98d;--card:#FBF6E9;}
*{box-sizing:border-box;margin:0;}
body{background:var(--paper);color:var(--ink);font-family:system-ui,-apple-system,sans-serif;min-height:100vh;padding:16px;}
.login{display:flex;align-items:center;justify-content:center;min-height:90vh;}
.login-box{background:var(--card);border:2px solid var(--ink);padding:32px;width:100%;max-width:360px;border-radius:4px;}
.login-box h2{font-size:22px;margin-bottom:4px;}
.login-box p{font-size:13px;color:var(--ink-soft);margin-bottom:16px;}
.login-box input{width:100%;padding:10px 12px;border:1.5px solid var(--ink);border-radius:2px;font-size:14px;margin-bottom:12px;background:var(--paper);}
.login-box button{width:100%;padding:11px;border:2px solid var(--ink);background:var(--ink);color:var(--paper);font-size:14px;cursor:pointer;border-radius:2px;}
.login-box .error{color:var(--rust);font-size:12px;margin-bottom:8px;}
.header{display:flex;align-items:center;justify-content:space-between;padding:12px 0;border-bottom:2px solid var(--ink);margin-bottom:16px;}
.header h1{font-size:20px;}
.header h1 em{color:var(--rust);font-style:italic;}
.header .folder{font-size:11px;color:var(--ink-soft);text-align:right;max-width:200px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.stats{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:16px;}
.chip{background:var(--paper);border:1px solid var(--line);padding:4px 10px;border-radius:20px;font-size:11px;color:var(--ink-soft);}
.tabs{display:flex;gap:4px;flex-wrap:wrap;margin-bottom:14px;}
.tab{background:var(--manila);border:1.5px solid var(--ink);padding:6px 12px;font-size:12px;cursor:pointer;border-radius:2px;}
.tab.active{background:var(--card);border-color:var(--rust);color:var(--rust);}
.search{width:100%;padding:10px 12px;border:1.5px solid var(--ink);border-radius:2px;font-size:13px;margin-bottom:14px;background:var(--card);}
.file{background:var(--card);border:1px solid var(--line);border-radius:2px;padding:10px 12px;margin-bottom:6px;display:flex;justify-content:space-between;align-items:center;gap:10px;}
.file .info{flex:1;min-width:0;}
.file .name{font-size:13px;font-weight:500;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.file .path{font-size:10px;color:var(--ink-soft);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.file .meta{font-size:11px;color:var(--ink-soft);white-space:nowrap;text-align:right;}
.file .cat{font-size:10px;background:var(--manila);padding:2px 6px;border-radius:10px;white-space:nowrap;}
.file .preview-btn{background:none;border:1px solid var(--line);padding:4px 8px;font-size:11px;cursor:pointer;border-radius:2px;color:var(--ink);}
.file .preview-btn:hover{border-color:var(--rust);color:var(--rust);}
.organize-btn{position:fixed;bottom:16px;right:16px;padding:14px 24px;background:var(--forest);color:var(--paper);border:2px solid var(--ink);font-size:14px;font-weight:600;cursor:pointer;border-radius:2px;box-shadow:3px 3px 0 rgba(0,0,0,.2);z-index:10;}
.organize-btn:hover{transform:translate(-1px,-1px);box-shadow:4px 4px 0 rgba(0,0,0,.2);}
.modal-overlay{position:fixed;inset:0;background:rgba(0,0,0,.5);z-index:20;display:none;align-items:center;justify-content:center;}
.modal-overlay.open{display:flex;}
.modal{background:var(--card);border:2px solid var(--ink);padding:24px;max-width:90vw;max-height:85vh;width:600px;overflow:auto;border-radius:2px;}
.modal h3{margin-bottom:12px;}
.modal .close{float:right;background:none;border:none;font-size:20px;cursor:pointer;color:var(--ink);}
.preview-img{max-width:100%;max-height:60vh;display:block;margin:0 auto;}
.preview-text{background:var(--ink);color:var(--paper);padding:16px;font-family:monospace;font-size:12px;white-space:pre-wrap;max-height:60vh;overflow:auto;border-radius:2px;}
.toast{position:fixed;bottom:80px;left:50%;transform:translateX(-50%);background:var(--ink);color:var(--paper);padding:10px 20px;border-radius:4px;font-size:13px;z-index:30;display:none;}
.toast.show{display:block;}
</style>
</head>
<body>
<div id="app"></div>
<script>
const API='';let TOKEN='';let catFilter='All';let files=[];let categories={};let totalSize=0;let folder='';let searchQ='';

function auth(pw){return fetch('/api/auth',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({password:pw})}).then(r=>{if(!r.ok)throw new Error('Wrong password');return r.json()}).then(d=>{TOKEN=d.token;localStorage.setItem('token',TOKEN);load();});}

function headers(){return TOKEN?{headers:{'Authorization':'Bearer '+TOKEN}}:{};}

function load(){fetch('/api/files',headers()).then(r=>r.json()).then(d=>{files=d.files||[];categories=d.categories||{};totalSize=d.total_size||0;folder=d.folder||'';render();}).catch(()=>showLogin());}

function doOrganize(){
  const moves=files.filter(f=>catFilter==='All'||categorize(f.ext)===catFilter).map(f=>({src:f.rel_path,dest:(categorize(f.ext)+'/'+f.name)}));
  if(!moves.length){toast('No files to organize');return;}
  if(!confirm('Organize '+moves.length+' files?'))return;
  fetch('/api/organize',{method:'POST',headers:{'Content-Type':'application/json','Authorization':'Bearer '+TOKEN},body:JSON.stringify({moves})}).then(r=>r.json()).then(d=>{toast('Moved '+d.moved+' files'+(d.failed?(', '+d.failed+' failed'):''));load();}).catch(e=>toast('Error: '+e));
}

function categorize(ext){ext=(ext||'').toLowerCase();const m={'pdf':'PDFs','doc':'Documents','docx':'Documents','txt':'Documents','rtf':'Documents','odt':'Documents','xls':'Spreadsheets','xlsx':'Spreadsheets','csv':'Spreadsheets','ods':'Spreadsheets','ppt':'Presentations','pptx':'Presentations','odp':'Presentations','jpg':'Images','jpeg':'Images','png':'Images','gif':'Images','bmp':'Images','svg':'Images','webp':'Images','mp4':'Videos','mov':'Videos','avi':'Videos','mkv':'Videos','wmv':'Videos','mp3':'Audio','wav':'Audio','aac':'Audio','flac':'Audio','m4a':'Audio','zip':'Archives','rar':'Archives','7z':'Archives','tar':'Archives','gz':'Archives','js':'Code & Data','ts':'Code & Data','py':'Code & Data','html':'Code & Data','css':'Code & Data','json':'Code & Data','exe':'Installers','msi':'Installers'};return m[ext]||'Other';}
function fmtSize(b){if(b<1024)return b+' B';if(b<1048576)return(b/1024).toFixed(1)+' KB';if(b<1073741824)return(b/1048576).toFixed(1)+' MB';return(b/1073741824).toFixed(2)+' GB';}
function toast(m){const t=document.getElementById('toast');t.textContent=m;t.classList.add('show');setTimeout(()=>t.classList.remove('show'),3000);}

function preview(name){
  const ext=name.split('.').pop().toLowerCase();
  const imgExts=['jpg','jpeg','png','gif','svg','webp','bmp'];
  const el=document.getElementById('modalContent');
  el.innerHTML='<button class="close" onclick="closeModal()">&times;</button><h3>'+name+'</h3>';
  if(imgExts.includes(ext)){
    el.innerHTML+='<img class="preview-img" src="/api/preview/'+encodeURIComponent(name)+'">';
  }else if(['txt','md','json','xml','csv','js','css','html'].includes(ext)){
    fetch('/api/preview/'+encodeURIComponent(name),headers()).then(r=>r.text()).then(t=>{el.innerHTML+='<pre class="preview-text">'+t.replace(/</g,'&lt;')+'</pre>';});
  }else{
    el.innerHTML+='<p style="padding:20px;text-align:center;color:var(--ink-soft)">Preview not available for this file type.</p>';
  }
  document.getElementById('modal').classList.add('open');
}
function closeModal(){document.getElementById('modal').classList.remove('open');}

function showLogin(){
  document.getElementById('app').innerHTML='<div class="login"><div class="login-box"><h2>The Filing Desk</h2><p>Enter password to access shared files</p><div class="error" id="loginErr"></div><input type="password" id="pw" placeholder="Password..."><button onclick="tryLogin()">Connect</button></div></div>';
  document.getElementById('pw').addEventListener('keydown',e=>{if(e.key==='Enter')tryLogin();});
}
function tryLogin(){document.getElementById('loginErr').textContent='';auth(document.getElementById('pw').value).catch(()=>{document.getElementById('loginErr').textContent='Wrong password. Try again.';});}

function render(){
  let cats=Object.entries(categories).sort((a,b)=>b[1]-a[1]);
  let filtered=files.filter(f=>(catFilter==='All'||categorize(f.ext)===catFilter)&&(searchQ===''||f.name.toLowerCase().includes(searchQ)));
  document.getElementById('app').innerHTML=`
    <div class="header"><div><h1>The Filing <em>Desk</em></h1></div><div class="folder">${folder.split(/[\\/]/).pop()||folder}</div></div>
    <div class="stats"><span class="chip">${files.length} files</span><span class="chip">${fmtSize(totalSize)} total</span><span class="chip">${cats.length} categories</span></div>
    <div class="tabs"><div class="tab ${catFilter==='All'?'active':''}" onclick="catFilter='All';render()">All (${files.length})</div>${cats.map(([k,v])=>`<div class="tab ${catFilter===k?'active':''}" onclick="catFilter='${k}';render()">${k} (${v})</div>`).join('')}</div>
    <input class="search" placeholder="Search files..." value="${searchQ}" oninput="searchQ=this.value.toLowerCase();render()">
    ${filtered.length?filtered.map(f=>`<div class="file"><div class="info"><div class="name">${f.name}</div><div class="path">${f.rel_path}</div></div><span class="cat">${categorize(f.ext)}</span><span class="meta">${fmtSize(f.size)}</span><button class="preview-btn" onclick="preview('${f.name.replace(/'/g,"\\'")}')">View</button></div>`).join(''):'<p style="text-align:center;padding:40px;color:var(--ink-soft)">No files found.</p>'}
    <button class="organize-btn" onclick="doOrganize()">Organize Files</button>
    <div id="modal" class="modal-overlay" onclick="if(event.target===this)closeModal()"><div class="modal" id="modalContent"></div></div>
    <div id="toast" class="toast"></div>`;
  document.querySelector('.search').focus();
}

const saved=localStorage.getItem('token');
if(saved){TOKEN=saved;load();}else{showLogin();}
</script>
</body>
</html>"#
});

async fn serve_remote_ui() -> Html<&'static str> {
    Html(REMOTE_UI.as_str())
}

// ─────────────────────────────────────────────
// WiFi Sharing — Tauri commands
// ─────────────────────────────────────────────

#[tauri::command]
async fn set_share_folder(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let state = app.state::<SharedState>();
    let mut folder = state.folder_path.write().await;
    *folder = path;
    Ok(())
}

#[tauri::command]
async fn start_sharing(
    app: tauri::AppHandle,
    password: String,
    port: u16,
) -> Result<serde_json::Value, String> {
    let state = app.state::<SharedState>();

    let mut folder = state.folder_path.write().await;
    if folder.is_empty() {
        return Err("No folder scanned yet. Select a folder first.".into());
    }

    let mut session = state.session.write().await;
    *session = Some(Session {
        password_hash: hash_password(&password),
        created_at: std::time::Instant::now(),
    });
    drop(session);

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    {
        let mut tx = state.shutdown_tx.write().await;
        if tx.is_some() {
            return Err("Server already running. Stop it first.".into());
        }
        *tx = Some(shutdown_tx);
    }

    let shared = state.clone();

    let app_router = Router::new()
        .route("/", get(serve_remote_ui))
        .route("/api/auth", post(api_auth))
        .route("/api/files", get(api_files))
        .route("/api/organize", post(api_organize))
        .route("/api/preview/{*path}", get(api_preview))
        .route("/api/qr", get(api_qr))
        .route("/api/info", get(api_server_info))
        .layer(middleware::from_fn_with_state(
            shared.clone(),
            auth_middleware,
        ))
        .with_state(shared);

    tokio::spawn(async move {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = TcpListener::bind(addr).await.unwrap();

        axum::serve(listener, app_router)
            .with_graceful_shutdown(async move {
                shutdown_rx.recv().await;
            })
            .await
            .unwrap();
    });

    let ip = get_local_ip().unwrap_or_else(|| "localhost".into());
    let url = format!("http://{}:{}", ip, port);

    let code = QrCode::new(url.as_bytes()).map_err(|e| e.to_string())?;
    let svg = code
        .render::<Svg>()
        .module_color(0x20291F_u8)
        .background_color(0xFBF6E9_u8)
        .quiet_zone(2)
        .build();

    Ok(serde_json::json!({
        "url": url,
        "ip": ip,
        "port": port,
        "qr": svg,
    }))
}

#[tauri::command]
async fn stop_sharing(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<SharedState>();
    let tx = state.shutdown_tx.write().await.take();
    if let Some(tx) = tx {
        let _ = tx.send(()).await;
    }
    let mut session = state.session.write().await;
    *session = None;
    Ok(())
}

#[tauri::command]
async fn get_sharing_status(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<SharedState>();
    let tx = state.shutdown_tx.read().await;
    let active = tx.is_some();
    let ip = get_local_ip().unwrap_or_else(|| "localhost".into());
    Ok(serde_json::json!({
        "active": active,
        "ip": ip,
        "port": 8080,
        "url": format!("http://{}:8080", ip),
    }))
}

// ─────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let server_state = Arc::new(ServerState {
        folder_path: RwLock::new(String::new()),
        session: RwLock::new(None),
        shutdown_tx: RwLock::new(None),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(server_state)
        .invoke_handler(tauri::generate_handler![
            scan_folder,
            organize_files,
            groq_chat,
            set_share_folder,
            start_sharing,
            stop_sharing,
            get_sharing_status,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let state = handle.state::<SharedState>();
            let state_clone = state.clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
                loop {
                    interval.tick().await;
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
