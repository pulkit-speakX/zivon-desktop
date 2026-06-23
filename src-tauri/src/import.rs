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

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

/// Gateway base — the desktop app is a localhost Envoy client (matches the
/// hard-locked web URL). The import path rides `/api/core/...` which the gateway
/// proxies to the orchestrator.
const GATEWAY_URL: &str = "http://localhost:9000";

const KEYRING_SERVICE: &str = "ai.zivon.envoy.desktop";
const KEYRING_SESSION: &str = "session";
const CLAUDE_IMPORT_SESSION_LIMIT: usize = 3;
const IMPORT_COMPACTION_RECENT_TURNS: usize = 8;
const IMPORT_COMPACTION_TURN_SNIPPET_CHARS: usize = 900;
const IMPORT_COMPACTION_MAX_CHARS: usize = 12_000;
const IMPORT_RAW_SCROLLBACK_MAX_TURNS: usize = 300;

// --------------------------------------------------------------------------- //
// Data model
// --------------------------------------------------------------------------- //

#[derive(Clone, serde::Serialize)]
pub struct ClaudeTurn {
    pub role: String,        // "user" | "assistant"
    pub text: String,        // content the agent sees on resume
    pub blocks: Value,       // typed blocks (array) the UI renders
    pub ts: Option<String>,  // ISO8601
    pub history_only: bool,  // true = visible in UI scrollback, excluded from runtime context
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
    pub first_user_text: Option<String>,
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

#[derive(serde::Serialize)]
pub struct OpenImportedSessionsResult {
    pub resumed: usize,
    pub first_url: String,
}

#[derive(Default)]
pub struct ImportState {
    pub session_paths: Mutex<HashMap<String, PathBuf>>,
}

struct SessionIndexEntry {
    summary: SessionSummary,
    path: PathBuf,
    modified: Option<SystemTime>,
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

fn push_retained_turn(turns: &mut VecDeque<ClaudeTurn>, turn: ClaudeTurn) {
    turns.push_back(turn);
    while turns.len() > IMPORT_RAW_SCROLLBACK_MAX_TURNS {
        turns.pop_front();
    }
}

fn apply_tool_result_to_retained_turns(turns: &mut VecDeque<ClaudeTurn>, tool_id: &str, result: String) {
    let result: String = result.chars().take(4000).collect();
    for turn in turns.iter_mut().rev() {
        let Some(blocks) = turn.blocks.as_array_mut() else {
            continue;
        };
        for block in blocks.iter_mut() {
            let matches_tool = block.get("kind").and_then(Value::as_str) == Some("tool")
                && block.get("toolUseId").and_then(Value::as_str) == Some(tool_id);
            if !matches_tool {
                continue;
            }
            if let Some(obj) = block.as_object_mut() {
                obj.insert("status".to_string(), Value::String("done".to_string()));
                obj.insert("result".to_string(), Value::String(result));
            }
            return;
        }
    }
}

fn parse_session(path: &Path) -> Option<ClaudeSession> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut last_at: Option<String> = None;
    let mut preview: Option<String> = None;
    let mut first_user_text: Option<String> = None;
    let mut turn_count = 0usize;
    let mut turns: VecDeque<ClaudeTurn> = VecDeque::new();

    for raw in reader.lines().map_while(Result::ok) {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
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
        if t == "user" {
            if let Some(arr) = ev.pointer("/message/content").and_then(Value::as_array) {
                for b in arr {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        if let Some(tid) = b.get("tool_use_id").and_then(Value::as_str) {
                            apply_tool_result_to_retained_turns(
                                &mut turns,
                                tid,
                                tool_result_text(b.get("content").unwrap_or(&Value::Null)),
                            );
                        }
                    }
                }
            }
        }
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
            if first_user_text.is_none() {
                first_user_text = Some(text.trim().to_string());
            }
            // User turns render from `content`; no turn_payload blocks needed
            // (and the interleaved block renderer is for assistant turns only).
            turn_count += 1;
            push_retained_turn(&mut turns, ClaudeTurn {
                role: "user".into(),
                text: text.clone(),
                blocks: json!([]),
                ts,
                history_only: false,
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
                            // The matching tool_result may arrive later in the JSONL.
                            // While the turn remains in the rolling scrollback window,
                            // apply_tool_result_to_retained_turns patches the block.
                            blocks.push(json!({
                                "kind": "tool",
                                "toolUseId": tid,
                                "name": name,
                                "input": b.get("input").cloned().unwrap_or(json!({})),
                                "status": "pending",
                                "result": "",
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
            turn_count += 1;
            push_retained_turn(&mut turns, ClaudeTurn {
                role: "assistant".into(),
                text,
                blocks: Value::Array(blocks),
                ts,
                history_only: false,
            });
        }
    }

    if turn_count == 0 || turns.is_empty() {
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
        turn_count,
        preview: preview.unwrap_or_else(|| "(no text prompt)".into()),
        path: path.to_string_lossy().to_string(),
        first_user_text,
        turns: turns.into_iter().collect(),
    })
}

fn parse_session_summary(path: &Path, modified: Option<SystemTime>) -> Option<SessionIndexEntry> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut last_at: Option<String> = None;
    let mut preview: Option<String> = None;
    let mut turn_count = 0usize;

    for raw in reader.lines().map_while(Result::ok) {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
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
            turn_count += 1;
        } else if role == "assistant" {
            let mut has_renderable_content = false;
            if let Some(arr) = content.as_array() {
                has_renderable_content = arr.iter().any(|b| match b.get("type").and_then(Value::as_str) {
                    Some("text") => b.get("text").and_then(Value::as_str).is_some_and(|txt| !txt.is_empty()),
                    Some("tool_use") => true,
                    _ => false,
                });
            } else if let Some(s) = content.as_str() {
                has_renderable_content = !s.is_empty();
            }
            if has_renderable_content {
                turn_count += 1;
            }
        }
    }

    if turn_count == 0 {
        return None;
    }

    let project = decode_project_dir(
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(""),
    );
    let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();

    Some(SessionIndexEntry {
        summary: SessionSummary {
            session_id,
            project: project.clone(),
            git_branch,
            started_at,
            last_at,
            turn_count,
            preview: preview.unwrap_or_else(|| "(no text prompt)".into()),
        },
        path: path.to_path_buf(),
        modified,
    })
}

fn discover_index_from(root: &Path) -> Vec<SessionIndexEntry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    let mut files: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
    for proj in entries.flatten() {
        let p = proj.path();
        if !p.is_dir() {
            continue;
        }
        if let Ok(project_files) = fs::read_dir(&p) {
            for f in project_files.flatten() {
                let fp = f.path();
                if fp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    let modified = f.metadata().and_then(|m| m.modified()).ok();
                    files.push((fp, modified));
                }
            }
        }
    }

    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (path, _) in files {
        let modified = path.metadata().and_then(|m| m.modified()).ok();
        if let Some(entry) = parse_session_summary(&path, modified) {
            out.push(entry);
        }
    }
    out.sort_by(|a, b| {
        b.summary
            .last_at
            .cmp(&a.summary.last_at)
            .then_with(|| b.modified.cmp(&a.modified))
    });
    out
}

fn discover_index() -> Vec<SessionIndexEntry> {
    let Some(root) = projects_dir() else {
        return Vec::new();
    };
    discover_index_from(&root)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn compact_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn snippet(text: &str) -> String {
    truncate_chars(&compact_whitespace(text), IMPORT_COMPACTION_TURN_SNIPPET_CHARS)
}

fn tool_names_from_blocks(blocks: &Value) -> Vec<String> {
    let mut names = Vec::new();
    let Some(arr) = blocks.as_array() else {
        return names;
    };
    for block in arr {
        if block.get("kind").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(name) = block.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn session_topic(sess: &ClaudeSession) -> String {
    let topic = compact_whitespace(&sess.preview);
    if !topic.is_empty() && topic != "(no text prompt)" {
        return truncate_chars(&topic, 82);
    }
    Path::new(&sess.project)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(|name| truncate_chars(name, 82))
        .unwrap_or_else(|| "Claude session".to_string())
}

fn import_title_for_session(session: &ClaudeSession) -> String {
    truncate_chars(&format!("Claude: {}", session_topic(session)), 96)
}

fn session_handoff_lines(sess: &ClaudeSession, heading: &str, recent_limit: usize, include_guidance: bool) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(heading.to_string());
    lines.push(format!("Project: {}", sess.project));
    lines.push(format!("Working directory: {}", sess.cwd));
    if let Some(branch) = &sess.git_branch {
        lines.push(format!("Git branch: {branch}"));
    }
    if let Some(started_at) = &sess.started_at {
        lines.push(format!("Started at: {started_at}"));
    }
    if let Some(last_at) = &sess.last_at {
        lines.push(format!("Last activity: {last_at}"));
    }
    lines.push(format!("Original Claude session id: {}", sess.session_id));
    lines.push(format!("Original turns: {}", sess.turn_count));
    let visible_raw_turns = sess
        .turns
        .iter()
        .filter(|turn| !turn.text.trim().is_empty())
        .count()
        .min(IMPORT_RAW_SCROLLBACK_MAX_TURNS);
    lines.push(format!(
        "Visible raw scrollback retained in Envoy: latest {visible_raw_turns} text turns"
    ));
    let omitted_raw_turns = sess.turn_count.saturating_sub(visible_raw_turns);
    if omitted_raw_turns > 0 {
        lines.push(format!("Older raw scrollback omitted from Envoy DB: {omitted_raw_turns} turns"));
    }

    let first_user_text = sess.first_user_text.as_deref().or_else(|| {
        sess.turns
            .iter()
            .find(|turn| turn.role == "user" && !turn.text.trim().is_empty())
            .map(|turn| turn.text.as_str())
    });
    if let Some(first_user_text) = first_user_text {
        lines.push(String::new());
        lines.push("Original first request:".to_string());
        lines.push(snippet(first_user_text));
    }

    let mut recent: Vec<&ClaudeTurn> = sess
        .turns
        .iter()
        .filter(|turn| !turn.text.trim().is_empty())
        .rev()
        .take(recent_limit)
        .collect();
    recent.reverse();
    if !recent.is_empty() {
        lines.push(String::new());
        lines.push("Recent conversation highlights:".to_string());
        for turn in recent {
            let role = if turn.role == "user" { "User" } else { "Assistant" };
            let mut line = format!("- {role}: {}", snippet(&turn.text));
            let tools = tool_names_from_blocks(&turn.blocks);
            if !tools.is_empty() {
                line.push_str(&format!(" [tools: {}]", tools.join(", ")));
            }
            lines.push(line);
        }
    }

    if include_guidance {
        lines.push(String::new());
        lines.push("Continuation guidance: continue from this compact handoff. Treat omitted raw turns as intentionally compacted; ask the user before relying on details that are not present here.".to_string());
    }

    lines
}

fn compact_session_turn(sess: &ClaudeSession) -> ClaudeTurn {
    let lines = session_handoff_lines(
        sess,
        "Imported Claude Code handoff",
        IMPORT_COMPACTION_RECENT_TURNS,
        true,
    );

    let text = truncate_chars(&lines.join("\n"), IMPORT_COMPACTION_MAX_CHARS);
    ClaudeTurn {
        role: "assistant".into(),
        text: text.clone(),
        blocks: json!([{ "kind": "text", "text": text }]),
        ts: sess.last_at.clone(),
        history_only: false,
    }
}

fn import_turns_for_session(sess: &ClaudeSession) -> Vec<ClaudeTurn> {
    let mut turns = vec![compact_session_turn(sess)];
    let raw: Vec<&ClaudeTurn> = sess
        .turns
        .iter()
        .filter(|turn| !turn.text.trim().is_empty())
        .collect();
    let start = raw.len().saturating_sub(IMPORT_RAW_SCROLLBACK_MAX_TURNS);
    turns.extend(raw.into_iter().skip(start).cloned().map(|mut turn| {
        turn.history_only = true;
        turn
    }));
    turns
}

// --------------------------------------------------------------------------- //
// Identity (from the keychain session bundle)
// --------------------------------------------------------------------------- //

struct Identity {
    token: String,
    refresh_token: String,
    slug: String,
    bundle_json: String,
}

fn save_session_bundle(bundle_json: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_SESSION)
        .map_err(|e| format!("keychain: {e}"))?;
    entry
        .set_password(bundle_json)
        .map_err(|e| format!("keychain save: {e}"))
}

fn update_session_bundle_tokens(bundle_json: &str, access_token: &str, refresh_token: &str) -> Result<String, String> {
    let mut v: Value = serde_json::from_str(bundle_json).map_err(|e| format!("bad session bundle: {e}"))?;
    let Some(obj) = v.as_object_mut() else {
        return Err("bad session bundle: expected object".to_string());
    };
    obj.insert("zivon_token".to_string(), Value::String(access_token.to_string()));
    obj.insert(
        "zivon_refresh_token".to_string(),
        Value::String(refresh_token.to_string()),
    );
    Ok(v.to_string())
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

    let refresh_token = v
        .get("zivon_refresh_token")
        .and_then(Value::as_str)
        .ok_or("no refresh token in session — sign out and sign in again")?
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

    Ok(Identity {
        token,
        refresh_token,
        slug,
        bundle_json: bundle,
    })
}

fn refresh_identity(ident: &mut Identity) -> Result<(), String> {
    let resp = ureq::post(&format!("{}/auth/refresh", GATEWAY_URL))
        .set("Content-Type", "application/json")
        .send_json(json!({ "refresh_token": ident.refresh_token }));

    let v: Value = match resp {
        Ok(r) => r
            .into_json()
            .map_err(|e| format!("could not decode refreshed session: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            if code == 401 {
                return Err("desktop session expired — refresh the desktop session, then retry import".to_string());
            }
            return Err(format!(
                "session refresh failed: HTTP {code}: {}",
                detail.chars().take(180).collect::<String>()
            ));
        }
        Err(ureq::Error::Transport(t)) => return Err(format!("session refresh failed: network: {t}")),
    };

    let access = v
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("session refresh response missing access token")?;
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or("session refresh response missing refresh token")?;
    let next_bundle = update_session_bundle_tokens(&ident.bundle_json, access, refresh)?;
    save_session_bundle(&next_bundle)?;

    ident.token = access.to_string();
    ident.refresh_token = refresh.to_string();
    ident.bundle_json = next_bundle;
    Ok(())
}

fn post_import(endpoint: &str, token: &str, body: Value) -> Result<ureq::Response, ureq::Error> {
    ureq::post(endpoint)
        .set("Authorization", &format!("Bearer {}", token))
        .set("X-Envoy-Client-Surface", "desktop")
        .set("Content-Type", "application/json")
        .send_json(body)
}

fn post_resume(endpoint: &str, token: &str) -> Result<ureq::Response, ureq::Error> {
    ureq::post(endpoint)
        .set("Authorization", &format!("Bearer {}", token))
        .set("X-Envoy-Client-Surface", "desktop")
        .set("Content-Type", "application/json")
        .send_json(json!({}))
}

fn should_abort_import_batch_after_refresh_error(message: &str) -> bool {
    !message.trim().is_empty()
}

fn build_app_session_sync_script(config: &crate::DesktopAuthSyncConfig) -> String {
    let url = serde_json::to_string(&config.url).unwrap_or_else(|_| "\"\"".to_string());
    let token = serde_json::to_string(&config.token).unwrap_or_else(|_| "\"\"".to_string());
    let gateway = serde_json::to_string(GATEWAY_URL).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(function(){{
function collectZivonStorage(){{
  var storage={{}};
  try{{
    for(var i=0;i<window.localStorage.length;i++){{
      var key=window.localStorage.key(i);
      if(key&&key.indexOf('zivon_')===0){{storage[key]=String(window.localStorage.getItem(key)||'');}}
    }}
  }}catch(e){{}}
  return storage;
}}
function writeLoginData(data){{
  try{{
    if(data.access_token){{window.localStorage.setItem('zivon_token',String(data.access_token));}}
    if(data.refresh_token){{window.localStorage.setItem('zivon_refresh_token',String(data.refresh_token));}}
    if(data.role){{window.localStorage.setItem('zivon_role',String(data.role));}}
    if(data.email){{window.localStorage.setItem('zivon_email',String(data.email));}}
    if(data.name){{window.localStorage.setItem('zivon_name',String(data.name));}}
    if(data.orgs){{window.localStorage.setItem('zivon_orgs',JSON.stringify(data.orgs||[]));}}
    if(data.pending_joins){{window.localStorage.setItem('zivon_pending_joins',JSON.stringify(data.pending_joins||[]));}}
    if(data.permissions){{window.localStorage.setItem('zivon_permissions',JSON.stringify(data.permissions||[]));}}
    if(data.finance_role){{window.localStorage.setItem('zivon_finance_role',String(data.finance_role));}}
    if(data.hr_role){{window.localStorage.setItem('zivon_hr_role',String(data.hr_role));}}
    if(data.orgs&&data.orgs.length&&!window.localStorage.getItem('zivon_last_org')){{
      window.localStorage.setItem('zivon_last_org',String(data.orgs[0].slug||''));
    }}
  }}catch(e){{}}
}}
async function tryRefreshWithZivonToken(){{
  var refresh=window.localStorage.getItem('zivon_refresh_token');
  if(!refresh){{return false;}}
  try{{
    var res=await window.fetch({gateway}+'/auth/refresh',{{
      method:'POST',
      headers:{{'Content-Type':'application/json'}},
      body:JSON.stringify({{refresh_token:refresh}})
    }});
    if(!res.ok){{return false;}}
    var data=await res.json();
    writeLoginData(data);
    return !!(data.access_token&&data.refresh_token);
  }}catch(e){{return false;}}
}}
async function tryLoginWithNextAuthSession(){{
  try{{
    var sessionRes=await window.fetch('/api/auth/session');
    if(!sessionRes.ok){{return false;}}
    var session=await sessionRes.json();
    if(!session||!session.id_token){{return false;}}
    var loginRes=await window.fetch({gateway}+'/auth/login',{{
      method:'POST',
      headers:{{'Content-Type':'application/json'}},
      body:JSON.stringify({{google_token:session.id_token}})
    }});
    if(!loginRes.ok){{return false;}}
    var data=await loginRes.json();
    writeLoginData(data);
    return !!(data.access_token&&data.refresh_token);
  }}catch(e){{return false;}}
}}
async function tryValidateCurrentAccessToken(){{
  var access=window.localStorage.getItem('zivon_token');
  if(!access){{return false;}}
  try{{
    var res=await window.fetch({gateway}+'/auth/me',{{
      method:'GET',
      headers:{{'Authorization':'Bearer '+access}}
    }});
    return !!res.ok;
  }}catch(e){{return false;}}
}}
function canUseLocalDevBypassLogin(){{
  try{{return window.localStorage.getItem('zivon_dev_bypass_login')==='true';}}catch(e){{return false;}}
}}
async function tryLocalDevBypassLogin(){{
  if(!canUseLocalDevBypassLogin()){{return false;}}
  try{{
    var res=await window.fetch({gateway}+'/auth/bypass-login',{{method:'POST',headers:{{'Content-Type':'application/json'}}}});
    if(!res.ok){{return false;}}
    var data=await res.json();
    writeLoginData(data);
    window.localStorage.setItem('zivon_dev_bypass_login','true');
    return !!(data.access_token&&data.refresh_token);
  }}catch(e){{return false;}}
}}
async function syncToNative(){{
  try{{
    var syncReady=await tryRefreshWithZivonToken()
      ||await tryLoginWithNextAuthSession()
      ||await tryValidateCurrentAccessToken()
      ||await tryLocalDevBypassLogin();
    var storage=collectZivonStorage();
    if(!syncReady||!storage.zivon_token||!storage.zivon_refresh_token){{return;}}
    await window.fetch({url},{{
      method:'POST',
      headers:{{'Content-Type':'application/json','X-Envoy-Desktop-Auth-Token':{token}}},
      body:JSON.stringify({{storage:storage}})
    }});
  }}catch(e){{}}
}}
syncToNative();
}})();"#
    )
}

pub(crate) fn request_app_session_sync(app: &AppHandle) -> bool {
    let Some(app_window) = app.get_webview_window("app") else {
        return false;
    };
    let config = app.state::<crate::DesktopAuthSyncConfig>().inner().clone();
    let previous_sync_count = config.sync_count.load(Ordering::SeqCst);
    let script = build_app_session_sync_script(&config);
    let _ = app_window.eval(&script);
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(5_000) {
        if config.sync_count.load(Ordering::SeqCst) > previous_sync_count {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
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
    let entries = discover_index();
    let summaries: Vec<SessionSummary> = entries
        .iter()
        .map(|entry| entry.summary.clone())
        .collect();

    if let Ok(mut cache) = state.session_paths.lock() {
        cache.clear();
        for entry in entries {
            cache.insert(entry.summary.session_id, entry.path);
        }
    }
    summaries
}

#[tauri::command]
pub fn claude_import(app: AppHandle, state: State<ImportState>, session_ids: Vec<String>) -> Result<(), String> {
    request_app_session_sync(&app);
    let ident = load_identity()?;
    if session_ids.len() > CLAUDE_IMPORT_SESSION_LIMIT {
        return Err(format!(
            "Import at most {CLAUDE_IMPORT_SESSION_LIMIT} Claude sessions at a time."
        ));
    }

    // Pull the selected sessions out of the cache before spawning.
    let selected: Vec<ClaudeSession> = {
        let cache = state.session_paths.lock().map_err(|_| "cache poisoned")?;
        session_ids
            .iter()
            .filter_map(|id| cache.get(id).and_then(|path| parse_session(path)))
            .collect()
    };
    if selected.is_empty() {
        return Err("no matching sessions to import".into());
    }

    std::thread::spawn(move || {
        let mut ident = ident;
        let total = selected.len();
        let endpoint = format!(
            "{}/api/core/api/orgs/{}/envoy/sessions/import",
            GATEWAY_URL, ident.slug
        );
        let mut aborted = false;
        let mut imported_urls: Vec<String> = Vec::new();
        for (i, sess) in selected.iter().enumerate() {
            let title = import_title_for_session(sess);
            let _ = app.emit(
                "import://progress",
                json!({
                    "type": "session_start",
                    "session_id": sess.session_id,
                    "index": i + 1,
                    "total": total,
                    "turns": sess.turn_count,
                    "compacted": true,
                    "project": sess.project,
                }),
            );

            let turns: Vec<Value> = import_turns_for_session(sess)
                .into_iter()
                .map(|turn| {
                    json!({
                        "role": turn.role,
                        "content": turn.text,
                        "blocks": turn.blocks,
                        "ts": turn.ts,
                        "history_only": turn.history_only,
                    })
                })
                .collect();
            let body = json!({
                "title": title,
                "turns": turns,
                "compacted": true,
                "original_turns": sess.turn_count,
            });

            let mut resp = post_import(&endpoint, &ident.token, body.clone());
            if matches!(resp, Err(ureq::Error::Status(401, _))) {
                match refresh_identity(&mut ident) {
                    Ok(()) => {
                        resp = post_import(&endpoint, &ident.token, body);
                    }
                    Err(e) => {
                        let should_abort = should_abort_import_batch_after_refresh_error(&e);
                        let _ = app.emit(
                            "import://progress",
                            json!({
                                "type": "session_error",
                                "session_id": sess.session_id,
                                "message": e,
                            }),
                        );
                        if should_abort {
                            aborted = true;
                            break;
                        }
                        continue;
                    }
                }
            }

            match resp {
                Ok(r) => {
                    let v: Value = r.into_json().unwrap_or(Value::Null);
                    if let Some(url) = v.get("url").and_then(Value::as_str) {
                        if !url.is_empty() {
                            imported_urls.push(url.to_string());
                        }
                    }
                    let _ = app.emit(
                        "import://progress",
                        json!({
                            "type": "session_done",
                            "session_id": sess.session_id,
                            "url": v.get("url").and_then(Value::as_str).unwrap_or(""),
                            "turns_written": v.get("turns_written").and_then(Value::as_i64).unwrap_or(0),
                            "original_turns": v.get("original_turns").and_then(Value::as_i64).unwrap_or(sess.turn_count as i64),
                            "raw_omitted": v.get("raw_omitted").and_then(Value::as_i64).unwrap_or(0),
                            "compacted": v.get("compacted").and_then(Value::as_bool).unwrap_or(true),
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
        let _ = app.emit(
            "import://progress",
            json!({ "type": "all_done", "count": if aborted { 0 } else { imported_urls.len() }, "urls": imported_urls }),
        );
    });

    Ok(())
}

#[tauri::command]
pub fn repair_desktop_session(app: AppHandle) -> Result<(), String> {
    crate::start_login(&app)
}

fn full_app_url(url: &str, strip_query: bool) -> Result<tauri::Url, String> {
    let full = if url.starts_with("http") {
        url.to_string()
    } else {
        format!("http://localhost:3000{url}")
    };
    let mut parsed = tauri::Url::parse(&full).map_err(|e| e.to_string())?;
    if strip_query {
        parsed.set_query(None);
    }
    Ok(parsed)
}

fn session_id_from_import_url(url: &str) -> Option<String> {
    let parsed = full_app_url(url, false).ok()?;
    let mut segments = parsed.path_segments()?;
    while let Some(segment) = segments.next() {
        if segment == "agent" {
            return segments.next().map(ToString::to_string);
        }
    }
    None
}

fn resume_imported_session(ident: &mut Identity, session_id: &str) -> Result<(), String> {
    let endpoint = format!(
        "{}/api/core/api/orgs/{}/pods/sessions/{}/resume",
        GATEWAY_URL, ident.slug, session_id
    );
    let mut resp = post_resume(&endpoint, &ident.token);
    if matches!(resp, Err(ureq::Error::Status(401, _))) {
        refresh_identity(ident)?;
        resp = post_resume(&endpoint, &ident.token);
    }
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(409, r)) => {
            let detail = r.into_string().unwrap_or_default();
            if detail.contains("already active") {
                Ok(())
            } else {
                Err(format!("resume failed for {session_id}: HTTP 409: {}", detail.chars().take(180).collect::<String>()))
            }
        }
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            Err(format!(
                "resume failed for {session_id}: HTTP {code}: {}",
                detail.chars().take(180).collect::<String>()
            ))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("resume failed for {session_id}: network: {t}")),
    }
}

/// Open an imported session in the main Envoy window (navigate it there).
#[tauri::command]
pub fn open_session_in_app(app: AppHandle, url: String) -> Result<(), String> {
    if app.get_webview_window("app").is_none() {
        if let Some(bundle) = crate::load_session() {
            crate::create_app_window(&app, bundle).map_err(|e| e.to_string())?;
        }
    }
    if let Some(w) = app.get_webview_window("app") {
        let parsed = full_app_url(&url, false)?;
        w.navigate(parsed).map_err(|e| e.to_string())?;
        let _ = w.show();
        let _ = w.set_focus();
        Ok(())
    } else {
        Err("Envoy window not open — sign in first".into())
    }
}

/// Resume a batch of imported desktop sessions, then show the first one.
#[tauri::command]
pub fn open_sessions_in_app(app: AppHandle, urls: Vec<String>) -> Result<OpenImportedSessionsResult, String> {
    if urls.is_empty() {
        return Err("No imported sessions to open".to_string());
    }
    request_app_session_sync(&app);
    let mut ident = load_identity()?;
    for url in &urls {
        let Some(session_id) = session_id_from_import_url(url) else {
            return Err("Imported session URL is invalid".to_string());
        };
        resume_imported_session(&mut ident, &session_id)?;
    }

    if app.get_webview_window("app").is_none() {
        if let Some(bundle) = crate::load_session() {
            crate::create_app_window(&app, bundle).map_err(|e| e.to_string())?;
        }
    }
    if let Some(w) = app.get_webview_window("app") {
        let parsed = full_app_url(&urls[0], true)?;
        w.navigate(parsed).map_err(|e| e.to_string())?;
        let _ = w.show();
        let _ = w.set_focus();
        let first_url = urls[0].clone();
        let resumed = urls.len();
        let app_for_close = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(350));
            if let Some(import_window) = app_for_close.get_webview_window("import") {
                let _ = import_window.close();
            }
        });
        Ok(OpenImportedSessionsResult { resumed, first_url })
    } else {
        Err("Envoy window not open — sign in first".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn temp_projects_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "envoy-import-{name}-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("temp projects root");
        dir
    }

    fn write_jsonl_session(root: &Path, project: &str, session_id: &str, timestamp: &str, prompt: &str) {
        let project_dir = root.join(project);
        fs::create_dir_all(&project_dir).expect("project dir");
        let path = project_dir.join(format!("{session_id}.jsonl"));
        let line = json!({
            "type": "user",
            "timestamp": timestamp,
            "cwd": decode_project_dir(project),
            "message": {
                "role": "user",
                "content": prompt,
            },
        });
        fs::write(path, format!("{line}\n")).expect("jsonl session");
    }

    #[test]
    fn discovery_returns_all_sessions_for_listing() {
        let root = temp_projects_root("limit");
        for i in 0..5 {
            write_jsonl_session(
                &root,
                "-Users-pulkitnagpal-Desktop-speakx-zivon-v2",
                &format!("session-{i}"),
                &format!("2026-06-22T04:2{i}:00Z"),
                &format!("prompt {i}"),
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        let sessions = discover_index_from(&root);

        assert_eq!(sessions.len(), 5);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_session_streams_large_history_into_recent_window() {
        let root = temp_projects_root("stream-cap");
        let project = "-Users-pulkitnagpal-Desktop-speakx-zivon-v2";
        let project_dir = root.join(project);
        fs::create_dir_all(&project_dir).expect("project dir");
        let path = project_dir.join("large-session.jsonl");
        let total = IMPORT_RAW_SCROLLBACK_MAX_TURNS + 5;
        let mut content = String::new();
        for i in 0..total {
            let line = json!({
                "type": "user",
                "timestamp": format!("2026-06-22T04:{:02}:00Z", i % 60),
                "cwd": decode_project_dir(project),
                "message": {
                    "role": "user",
                    "content": format!("prompt {i}"),
                },
            });
            content.push_str(&format!("{line}\n"));
        }
        fs::write(&path, content).expect("large jsonl session");

        let parsed = parse_session(&path).expect("parsed session");

        assert_eq!(parsed.turn_count, total);
        assert_eq!(parsed.turns.len(), IMPORT_RAW_SCROLLBACK_MAX_TURNS);
        assert_eq!(parsed.first_user_text.as_deref(), Some("prompt 0"));
        assert_eq!(parsed.turns[0].text, "prompt 5");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn compact_session_turn_creates_single_handoff_with_original_context() {
        let session = ClaudeSession {
            session_id: "claude-123".to_string(),
            project: "/Users/pulkitnagpal/Desktop/speakx/zivon-v2".to_string(),
            cwd: "/Users/pulkitnagpal/Desktop/speakx/zivon-v2".to_string(),
            git_branch: Some("envoy-cowork".to_string()),
            started_at: Some("2026-06-22T04:20:00Z".to_string()),
            last_at: Some("2026-06-22T04:30:00Z".to_string()),
            turn_count: 4,
            preview: "build cowork import".to_string(),
            path: "/tmp/claude-123.jsonl".to_string(),
            first_user_text: Some("Build the Cowork-style import flow.".to_string()),
            turns: vec![
                ClaudeTurn {
                    role: "user".to_string(),
                    text: "Build the Cowork-style import flow.".to_string(),
                    blocks: json!([]),
                    ts: None,
                    history_only: false,
                },
                ClaudeTurn {
                    role: "assistant".to_string(),
                    text: "I inspected the importer and found every session selected by default.".to_string(),
                    blocks: json!([{ "kind": "text", "text": "I inspected the importer." }]),
                    ts: None,
                    history_only: false,
                },
                ClaudeTurn {
                    role: "user".to_string(),
                    text: "Make it production-like and avoid huge histories.".to_string(),
                    blocks: json!([]),
                    ts: None,
                    history_only: false,
                },
                ClaudeTurn {
                    role: "assistant".to_string(),
                    text: "I will compact the imported transcript into a handoff.".to_string(),
                    blocks: json!([]),
                    ts: None,
                    history_only: false,
                },
            ],
        };

        let turn = compact_session_turn(&session);

        assert_eq!(turn.role, "assistant");
        assert!(turn.text.contains("Imported Claude Code handoff"));
        assert!(turn.text.contains("Original turns: 4"));
        assert!(turn.text.contains("Build the Cowork-style import flow."));
        assert!(turn.text.len() <= IMPORT_COMPACTION_MAX_CHARS + 3);
    }

    #[test]
    fn import_turns_include_handoff_plus_capped_recent_history_scrollback() {
        let total = IMPORT_RAW_SCROLLBACK_MAX_TURNS + 5;
        let turns: Vec<ClaudeTurn> = (0..total)
            .map(|i| ClaudeTurn {
                role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                text: format!("turn-{i}"),
                blocks: json!([]),
                ts: None,
                history_only: false,
            })
            .collect();
        let session = ClaudeSession {
            session_id: "claude-456".to_string(),
            project: "/tmp/project".to_string(),
            cwd: "/tmp/project".to_string(),
            git_branch: None,
            started_at: None,
            last_at: None,
            turn_count: turns.len(),
            preview: "turn-0".to_string(),
            path: "/tmp/claude-456.jsonl".to_string(),
            first_user_text: Some("turn-0".to_string()),
            turns,
        };

        let import_turns = import_turns_for_session(&session);

        assert_eq!(import_turns.len(), IMPORT_RAW_SCROLLBACK_MAX_TURNS + 1);
        assert!(import_turns[0].text.contains("Imported Claude Code handoff"));
        assert!(!import_turns[0].history_only);
        assert_eq!(import_turns[1].text, "turn-5");
        let expected_last = format!("turn-{}", total - 1);
        assert_eq!(
            import_turns.last().map(|turn| turn.text.as_str()),
            Some(expected_last.as_str())
        );
        assert!(import_turns[1..].iter().all(|turn| turn.history_only));
    }

    #[test]
    fn import_title_uses_conversation_topic_not_project_folder() {
        let session = ClaudeSession {
            session_id: "claude-a".to_string(),
            project: "/Users/pulkitnagpal/Desktop/speakx/zivon-v2".to_string(),
            cwd: "/Users/pulkitnagpal/Desktop/speakx/zivon-v2".to_string(),
            git_branch: None,
            started_at: None,
            last_at: None,
            turn_count: 1,
            preview: "Fix Claude import auto resume in desktop worker".to_string(),
            path: "/tmp/claude-a.jsonl".to_string(),
            first_user_text: None,
            turns: vec![],
        };

        let title = import_title_for_session(&session);

        assert!(title.contains("Fix Claude import auto resume"));
        assert!(!title.contains("zivon-v2"));
    }

    #[test]
    fn import_url_parsing_extracts_session_id() {
        let id = session_id_from_import_url(
            "/org/speakx-dev/agent/64e7f4f74f4f4f4f4f4f4f4f?auto_resume=1&source=claude_import",
        );

        assert_eq!(id.as_deref(), Some("64e7f4f74f4f4f4f4f4f4f4f"));
    }

    #[test]
    fn session_bundle_token_update_replaces_rotated_tokens() {
        let bundle = json!({
            "zivon_token": "old-access",
            "zivon_refresh_token": "old-refresh",
            "zivon_last_org": "speakx-dev",
            "zivon_orgs": "[]"
        })
        .to_string();

        let updated = update_session_bundle_tokens(&bundle, "new-access", "new-refresh").expect("updated bundle");
        let parsed: Value = serde_json::from_str(&updated).expect("valid json");

        assert_eq!(parsed.get("zivon_token").and_then(Value::as_str), Some("new-access"));
        assert_eq!(
            parsed.get("zivon_refresh_token").and_then(Value::as_str),
            Some("new-refresh")
        );
        assert_eq!(parsed.get("zivon_last_org").and_then(Value::as_str), Some("speakx-dev"));
    }

    #[test]
    fn refresh_failure_aborts_import_batch_after_one_visible_error() {
        assert!(should_abort_import_batch_after_refresh_error(
            "desktop session expired — sign out and sign in again"
        ));
    }

    #[test]
    fn app_session_sync_script_collects_current_webview_storage() {
        let config = crate::DesktopAuthSyncConfig {
            url: "http://127.0.0.1:12345/auth/sync".to_string(),
            token: "native-sync-token".to_string(),
            sync_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let script = build_app_session_sync_script(&config);

        assert!(script.contains("window.localStorage.length"));
        assert!(script.contains("zivon_token"));
        assert!(script.contains("zivon_refresh_token"));
        assert!(script.contains("/auth/refresh"));
        assert!(script.contains("/auth/me"));
        assert!(script.contains("tryValidateCurrentAccessToken"));
        assert!(script.contains("syncReady"));
        assert!(script.contains("if(!syncReady||!storage.zivon_token||!storage.zivon_refresh_token)"));
        assert!(script.contains("/api/auth/session"));
        assert!(script.contains("/auth/login"));
        assert!(script.contains("/auth/bypass-login"));
        assert!(script.contains("zivon_dev_bypass_login"));
        assert!(script.contains("canUseLocalDevBypassLogin"));
        assert!(script.contains("X-Envoy-Desktop-Auth-Token"));
        assert!(script.contains("native-sync-token"));
        assert!(script.contains("http://127.0.0.1:12345/auth/sync"));
    }
}
