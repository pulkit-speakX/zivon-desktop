//! Claude Code session import.
//!
//! Discovers local Claude Code sessions (`~/.claude/projects/*/*.jsonl`),
//! normalizes them, and imports selected ones into Envoy as native, continuable
//! chat sessions by POSTing to the orchestrator's authenticated import endpoint
//! (through the gateway) with the user's JWT — the desktop app never holds DB
//! credentials and a user can only import into their own envoy.
//!
//! All file reads here are TIER 0 (read-only, local). The write happens entirely
//! server-side behind the user's authenticated identity.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

/// Gateway base — the desktop app is a localhost Envoy client (matches the
/// hard-locked web URL). The import path rides `/api/core/...` which the gateway
/// proxies to the orchestrator.
const GATEWAY_URL: &str = "http://localhost:9000";

const KEYRING_SERVICE: &str = "ai.zivon.envoy.desktop";
const KEYRING_SESSION: &str = "session";

// --------------------------------------------------------------------------- //
// Data model
// --------------------------------------------------------------------------- //

#[derive(Clone, serde::Serialize)]
pub struct ClaudeTurn {
    pub role: String,        // "user" | "assistant"
    pub text: String,        // content the agent sees on resume
    pub blocks: Value,       // typed blocks (array) the UI renders
    pub ts: Option<String>,  // ISO8601
}

#[derive(Clone, serde::Serialize)]
pub struct ClaudeSession {
    pub session_id: String,
    pub project: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
    pub turn_count: usize,
    pub preview: String,
    pub path: String,
    #[serde(skip)]
    pub turns: Vec<ClaudeTurn>,
}

/// Listing payload (no heavy turns).
#[derive(Clone, serde::Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub project: String,
    pub git_branch: Option<String>,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
    pub turn_count: usize,
    pub preview: String,
}

#[derive(Default)]
pub struct ImportState {
    pub sessions: Mutex<HashMap<String, ClaudeSession>>,
}

// --------------------------------------------------------------------------- //
// Discovery + parsing  (mirrors importer/parser.py)
// --------------------------------------------------------------------------- //

fn projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

fn decode_project_dir(name: &str) -> String {
    if let Some(rest) = name.strip_prefix('-') {
        format!("/{}", rest.replace('-', "/"))
    } else {
        name.replace('-', "/")
    }
}

fn text_from_content(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut parts = Vec::new();
        for b in arr {
            if b.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
        }
        return parts.join("\n");
    }
    String::new()
}

fn tool_result_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut parts = Vec::new();
        for b in arr {
            if let Some(t) = b.get("text").and_then(Value::as_str) {
                parts.push(t.to_string());
            } else if let Some(s) = b.as_str() {
                parts.push(s.to_string());
            }
        }
        return parts.join("\n");
    }
    String::new()
}

fn parse_events(path: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                out.push(v);
            }
        }
    }
    out
}

fn parse_session(path: &Path) -> Option<ClaudeSession> {
    let events = parse_events(path);
    if events.is_empty() {
        return None;
    }

    // First pass: tool_use_id -> result text (from user tool_result blocks).
    let mut tool_results: HashMap<String, String> = HashMap::new();
    for ev in &events {
        if ev.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(arr) = ev.pointer("/message/content").and_then(Value::as_array) {
            for b in arr {
                if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                    if let Some(tid) = b.get("tool_use_id").and_then(Value::as_str) {
                        tool_results.insert(tid.to_string(), tool_result_text(b.get("content").unwrap_or(&Value::Null)));
                    }
                }
            }
        }
    }

    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut last_at: Option<String> = None;
    let mut preview: Option<String> = None;
    let mut turns: Vec<ClaudeTurn> = Vec::new();

    for ev in &events {
        if cwd.is_none() {
            if let Some(c) = ev.get("cwd").and_then(Value::as_str) {
                cwd = Some(c.to_string());
            }
        }
        if git_branch.is_none() {
            if let Some(g) = ev.get("gitBranch").and_then(Value::as_str) {
                if !g.is_empty() {
                    git_branch = Some(g.to_string());
                }
            }
        }
        let ts = ev.get("timestamp").and_then(Value::as_str).map(String::from);
        if let Some(ref t) = ts {
            if started_at.is_none() {
                started_at = Some(t.clone());
            }
            last_at = Some(t.clone());
        }

        let t = ev.get("type").and_then(Value::as_str).unwrap_or("");
        if t != "user" && t != "assistant" {
            continue;
        }
        if ev.get("isSidechain").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        if ev.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }

        let msg = ev.get("message").cloned().unwrap_or(Value::Null);
        let role = msg.get("role").and_then(Value::as_str).unwrap_or(t);
        let content = msg.get("content").cloned().unwrap_or(Value::Null);

        if role == "user" {
            let text = text_from_content(&content);
            let trimmed = text.trim_start();
            if text.trim().is_empty()
                || trimmed.starts_with("<local-command")
                || trimmed.starts_with("<command-")
            {
                continue;
            }
            if preview.is_none() {
                preview = Some(text.trim().chars().take(200).collect());
            }
            // User turns render from `content`; no turn_payload blocks needed
            // (and the interleaved block renderer is for assistant turns only).
            turns.push(ClaudeTurn {
                role: "user".into(),
                text: text.clone(),
                blocks: json!([]),
                ts,
            });
        } else if role == "assistant" {
            let mut blocks: Vec<Value> = Vec::new();
            let mut texts: Vec<String> = Vec::new();
            let mut tool_names: Vec<String> = Vec::new();
            if let Some(arr) = content.as_array() {
                for b in arr {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(txt) = b.get("text").and_then(Value::as_str) {
                                if !txt.is_empty() {
                                    texts.push(txt.to_string());
                                    // Render-model text block: field is `text` (turnBlocks.ts).
                                    blocks.push(json!({ "kind": "text", "text": txt }));
                                }
                            }
                        }
                        Some("tool_use") => {
                            let tid = b.get("id").and_then(Value::as_str).unwrap_or("");
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                            tool_names.push(name.to_string());
                            // Render-model tool block: a single `tool` block with the
                            // result folded in (no separate tool_result block).
                            let result: String = tool_results
                                .get(tid)
                                .map(|r| r.chars().take(4000).collect())
                                .unwrap_or_default();
                            blocks.push(json!({
                                "kind": "tool",
                                "toolUseId": tid,
                                "name": name,
                                "input": b.get("input").cloned().unwrap_or(json!({})),
                                "status": "done",
                                "result": result,
                            }));
                        }
                        _ => {}
                    }
                }
            } else if let Some(s) = content.as_str() {
                if !s.is_empty() {
                    texts.push(s.to_string());
                    blocks.push(json!({ "kind": "text", "text": s }));
                }
            }

            if blocks.is_empty() {
                continue;
            }
            let mut text = texts.join("\n").trim().to_string();
            if text.is_empty() && !tool_names.is_empty() {
                let mut seen = Vec::new();
                for n in &tool_names {
                    if !seen.contains(n) {
                        seen.push(n.clone());
                    }
                }
                text = format!("[used tools: {}]", seen.join(", "));
            }
            turns.push(ClaudeTurn {
                role: "assistant".into(),
                text,
                blocks: Value::Array(blocks),
                ts,
            });
        }
    }

    if turns.is_empty() {
        return None;
    }

    let project = decode_project_dir(
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(""),
    );
    let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();

    Some(ClaudeSession {
        session_id,
        cwd: cwd.clone().unwrap_or_else(|| project.clone()),
        project,
        git_branch,
        started_at,
        last_at,
        turn_count: turns.len(),
        preview: preview.unwrap_or_else(|| "(no text prompt)".into()),
        path: path.to_string_lossy().to_string(),
        turns,
    })
}

fn discover() -> Vec<ClaudeSession> {
    let mut out = Vec::new();
    let Some(root) = projects_dir() else {
        return out;
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return out;
    };
    for proj in entries.flatten() {
        let p = proj.path();
        if !p.is_dir() {
            continue;
        }
        if let Ok(files) = fs::read_dir(&p) {
            for f in files.flatten() {
                let fp = f.path();
                if fp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(s) = parse_session(&fp) {
                        out.push(s);
                    }
                }
            }
        }
    }
    // newest first by last activity
    out.sort_by(|a, b| b.last_at.cmp(&a.last_at));
    out
}

// --------------------------------------------------------------------------- //
// Identity (from the keychain session bundle)
// --------------------------------------------------------------------------- //

struct Identity {
    token: String,
    slug: String,
}

fn load_identity() -> Result<Identity, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_SESSION)
        .map_err(|e| format!("keychain: {e}"))?;
    let bundle = entry
        .get_password()
        .map_err(|_| "not signed in — open Envoy and sign in first".to_string())?;
    let v: Value = serde_json::from_str(&bundle).map_err(|e| format!("bad session bundle: {e}"))?;

    let token = v
        .get("zivon_token")
        .and_then(Value::as_str)
        .ok_or("no auth token in session — sign in again")?
        .to_string();

    // Prefer the last-used org; fall back to the first org in the list.
    let slug = v
        .get("zivon_last_org")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            v.get("zivon_orgs")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|orgs| {
                    orgs.as_array()
                        .and_then(|a| a.first())
                        .and_then(|o| o.get("slug").and_then(Value::as_str).map(String::from))
                })
        })
        .ok_or("no organization found in session — open Envoy once, then retry")?;

    Ok(Identity { token, slug })
}

// --------------------------------------------------------------------------- //
// Window
// --------------------------------------------------------------------------- //

/// Open (or focus) the local "Import Claude sessions" window.
pub fn open_import_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window("import") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, "import", WebviewUrl::App("import.html".into()))
        .title("Import Claude Sessions")
        .inner_size(840.0, 720.0)
        .min_inner_size(640.0, 520.0)
        .build()?;
    Ok(())
}

// --------------------------------------------------------------------------- //
// Commands
// --------------------------------------------------------------------------- //

#[tauri::command]
pub fn claude_list_sessions(state: State<ImportState>) -> Vec<SessionSummary> {
    let sessions = discover();
    let summaries: Vec<SessionSummary> = sessions
        .iter()
        .map(|s| SessionSummary {
            session_id: s.session_id.clone(),
            project: s.project.clone(),
            git_branch: s.git_branch.clone(),
            started_at: s.started_at.clone(),
            last_at: s.last_at.clone(),
            turn_count: s.turn_count,
            preview: s.preview.clone(),
        })
        .collect();

    if let Ok(mut cache) = state.sessions.lock() {
        cache.clear();
        for s in sessions {
            cache.insert(s.session_id.clone(), s);
        }
    }
    summaries
}

#[tauri::command]
pub fn claude_import(app: AppHandle, state: State<ImportState>, session_ids: Vec<String>) -> Result<(), String> {
    let ident = load_identity()?;

    // Pull the selected sessions out of the cache before spawning.
    let selected: Vec<ClaudeSession> = {
        let cache = state.sessions.lock().map_err(|_| "cache poisoned")?;
        session_ids
            .iter()
            .filter_map(|id| cache.get(id).cloned())
            .collect()
    };
    if selected.is_empty() {
        return Err("no matching sessions to import".into());
    }

    std::thread::spawn(move || {
        let total = selected.len();
        let endpoint = format!(
            "{}/api/core/api/orgs/{}/envoy/sessions/import",
            GATEWAY_URL, ident.slug
        );
        for (i, sess) in selected.iter().enumerate() {
            let title = format!(
                "[Imported] {}",
                Path::new(&sess.project)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Claude session")
            );
            let _ = app.emit(
                "import://progress",
                json!({
                    "type": "session_start",
                    "session_id": sess.session_id,
                    "index": i + 1,
                    "total": total,
                    "turns": sess.turn_count,
                    "project": sess.project,
                }),
            );

            let turns: Vec<Value> = sess
                .turns
                .iter()
                .map(|t| {
                    json!({
                        "role": t.role,
                        "content": t.text,
                        "blocks": t.blocks,
                        "ts": t.ts,
                    })
                })
                .collect();
            let body = json!({ "title": title, "turns": turns });

            let resp = ureq::post(&endpoint)
                .set("Authorization", &format!("Bearer {}", ident.token))
                .set("Content-Type", "application/json")
                .send_json(body);

            match resp {
                Ok(r) => {
                    let v: Value = r.into_json().unwrap_or(Value::Null);
                    let _ = app.emit(
                        "import://progress",
                        json!({
                            "type": "session_done",
                            "session_id": sess.session_id,
                            "url": v.get("url").and_then(Value::as_str).unwrap_or(""),
                            "turns_written": v.get("turns_written").and_then(Value::as_i64).unwrap_or(0),
                            "title": v.get("title").and_then(Value::as_str).unwrap_or(&title),
                        }),
                    );
                }
                Err(e) => {
                    let msg = match e {
                        ureq::Error::Status(code, r) => {
                            let detail = r.into_string().unwrap_or_default();
                            format!("HTTP {code}: {}", detail.chars().take(180).collect::<String>())
                        }
                        ureq::Error::Transport(t) => format!("network: {t}"),
                    };
                    let _ = app.emit(
                        "import://progress",
                        json!({
                            "type": "session_error",
                            "session_id": sess.session_id,
                            "message": msg,
                        }),
                    );
                }
            }
        }
        let _ = app.emit("import://progress", json!({ "type": "all_done", "count": total }));
    });

    Ok(())
}

/// Open an imported session in the main Envoy window (navigate it there).
#[tauri::command]
pub fn open_session_in_app(app: AppHandle, url: String) -> Result<(), String> {
    let full = if url.starts_with("http") {
        url
    } else {
        // The app window is already on the Envoy origin; navigate within it.
        format!("http://localhost:3000{url}")
    };
    if let Some(w) = app.get_webview_window("app") {
        let parsed = tauri::Url::parse(&full).map_err(|e| e.to_string())?;
        w.navigate(parsed).map_err(|e| e.to_string())?;
        let _ = w.show();
        let _ = w.set_focus();
        Ok(())
    } else {
        Err("Envoy window not open — sign in first".into())
    }
}
