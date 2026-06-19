use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_FILE_BYTES: usize = 256 * 1024;
const BRIDGE_TOKEN_HEADER: &str = "X-Envoy-Desktop-Bridge-Token";
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";
const TERMINAL_TIMEOUT_SECONDS: u64 = 10;

#[derive(Clone)]
pub struct DesktopBridgeState {
    inner: Arc<DesktopBridgeInner>,
}

struct DesktopBridgeInner {
    token: String,
    url: Mutex<Option<String>>,
    working_folder: Mutex<Option<PathBuf>>,
}

#[derive(Serialize)]
struct BridgeConfig {
    url: String,
    token: String,
    working_folder: Option<String>,
    allowed_roots: Vec<String>,
}

#[derive(Deserialize)]
struct WorkspaceRequest {
    path: String,
}

#[derive(Deserialize)]
struct FilePathRequest {
    path: String,
}

#[derive(Deserialize)]
struct WriteFileRequest {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct ListFilesRequest {
    path: Option<String>,
    include_hidden: Option<bool>,
}

#[derive(Deserialize)]
struct RunCommandRequest {
    command: String,
    cwd: Option<String>,
}

#[derive(Serialize)]
struct FileListingEntry {
    name: String,
    path: String,
    kind: String,
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
struct FileListing {
    path: String,
    total_entries: usize,
    file_count: usize,
    directory_count: usize,
    hidden_count: usize,
    entries: Vec<FileListingEntry>,
}

#[derive(Serialize)]
struct CommandResult {
    command: String,
    cwd: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl Default for DesktopBridgeState {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopBridgeState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DesktopBridgeInner {
                token: uuid::Uuid::new_v4().to_string(),
                url: Mutex::new(None),
                working_folder: Mutex::new(None),
            }),
        }
    }

    fn token(&self) -> String {
        self.inner.token.clone()
    }

    fn url(&self) -> Option<String> {
        self.inner.url.lock().ok().and_then(|url| url.clone())
    }

    fn set_url(&self, url: String) {
        if let Ok(mut slot) = self.inner.url.lock() {
            *slot = Some(url);
        }
    }

    fn working_folder(&self) -> Option<PathBuf> {
        self.inner
            .working_folder
            .lock()
            .ok()
            .and_then(|folder| folder.clone())
    }

    fn set_working_folder(&self, path: PathBuf) -> Result<(), String> {
        let resolved = path
            .canonicalize()
            .map_err(|e| format!("working folder does not exist: {e}"))?;
        if !resolved.is_dir() {
            return Err("working folder must be a directory".to_string());
        }
        if let Ok(mut folder) = self.inner.working_folder.lock() {
            *folder = Some(resolved);
        }
        write_bridge_config(self)
    }
}

pub fn bridge_config_path() -> Result<PathBuf, String> {
    let base = dirs::data_dir()
        .or_else(dirs::home_dir)
        .ok_or("could not resolve user data directory")?;
    Ok(base.join("Envoy").join("desktop-bridge.json"))
}

fn write_bridge_config(state: &DesktopBridgeState) -> Result<(), String> {
    let url = state.url().unwrap_or_default();
    if url.is_empty() {
        return Ok(());
    }
    let path = bridge_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("could not create bridge config dir: {e}"))?;
    }
    let config = BridgeConfig {
        url,
        token: state.token(),
        working_folder: state
            .working_folder()
            .map(|folder| folder.to_string_lossy().to_string()),
        allowed_roots: allowed_roots(state)
            .iter()
            .map(|root| root.to_string_lossy().to_string())
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&config)
        .map_err(|e| format!("could not encode bridge config: {e}"))?;
    fs::write(path, bytes).map_err(|e| format!("could not write bridge config: {e}"))
}

#[cfg(test)]
pub fn resolve_relative_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err("path must be relative to the working folder".to_string());
    }

    let mut clean = PathBuf::new();
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return Err("path must be relative to the working folder".to_string()),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err("path must name a file under the working folder".to_string());
    }
    Ok(root.join(clean))
}

fn default_local_access_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    ["Desktop", "Documents", "Downloads"]
        .iter()
        .filter_map(|name| {
            let candidate = home.join(name);
            if candidate.is_dir() {
                candidate.canonicalize().ok()
            } else {
                None
            }
        })
        .collect()
}

fn allowed_roots(state: &DesktopBridgeState) -> Vec<PathBuf> {
    let mut roots = default_local_access_roots();
    if let Some(folder) = state.working_folder() {
        if let Ok(resolved) = folder.canonicalize() {
            roots.push(resolved);
        }
    }
    dedupe_paths(roots)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    dedupe_paths(
        roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect(),
    )
}

fn expand_home_path(requested: &str) -> PathBuf {
    if requested == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(requested));
    }
    if let Some(rest) = requested.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(requested)
}

fn clean_relative_path(rel: &Path) -> Result<PathBuf, String> {
    let mut clean = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return Err("path must stay inside the active working folder".to_string()),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err("path must name a file or folder".to_string());
    }
    Ok(clean)
}

fn candidate_path(requested: &str, working_folder: Option<&Path>) -> Result<PathBuf, String> {
    let trimmed = requested.trim();
    if trimmed.is_empty() || trimmed == "." {
        return working_folder
            .map(Path::to_path_buf)
            .ok_or_else(|| "no active working folder for relative path".to_string());
    }

    let expanded = expand_home_path(trimmed);
    if expanded
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("path must not contain parent traversal".to_string());
    }
    if expanded.is_absolute() {
        return Ok(expanded);
    }

    let root = working_folder
        .ok_or_else(|| "relative paths require an active working folder".to_string())?;
    Ok(root.join(clean_relative_path(&expanded)?))
}

fn has_sensitive_component(path: &Path) -> bool {
    let mut normal_parts: Vec<String> = Vec::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            normal_parts.push(part.to_string_lossy().to_string());
        }
    }

    for (index, part) in normal_parts.iter().enumerate() {
        if matches!(part.as_str(), ".ssh" | ".aws" | ".gnupg" | ".kube") {
            return true;
        }
        if part == "Library" && normal_parts.get(index + 1).is_some_and(|next| next == "Keychains") {
            return true;
        }
    }

    if let Some(file_name) = path.file_name().and_then(|name| name.to_str()) {
        if file_name == ".env" || file_name.starts_with(".env.") {
            return true;
        }
        if matches!(file_name, "id_rsa" | "id_ed25519" | "id_ecdsa" | "id_dsa") {
            return true;
        }
        if file_name.ends_with(".pem") || file_name.ends_with(".key") {
            return true;
        }
    }

    false
}

fn ensure_not_sensitive(path: &Path) -> Result<(), String> {
    if has_sensitive_component(path) {
        return Err("path is blocked because it looks sensitive".to_string());
    }
    Ok(())
}

fn ensure_inside_allowed_roots(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    if roots.iter().any(|root| path.starts_with(root)) {
        return Ok(());
    }
    Err("path resolves outside allowed local access roots".to_string())
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path.parent();
    while let Some(candidate) = current {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

fn resolve_allowed_path(
    roots: &[PathBuf],
    working_folder: Option<&Path>,
    requested: &str,
    must_exist: bool,
) -> Result<PathBuf, String> {
    let roots = canonical_roots(roots);
    if roots.is_empty() {
        return Err("no local access roots are available".to_string());
    }

    let candidate = candidate_path(requested, working_folder)?;
    ensure_not_sensitive(&candidate)?;

    let resolved = if must_exist || candidate.exists() {
        candidate
            .canonicalize()
            .map_err(|e| format!("could not resolve local path: {e}"))?
    } else {
        ensure_inside_allowed_roots(&candidate, &roots)?;
        let existing_parent = nearest_existing_ancestor(&candidate)
            .ok_or_else(|| "could not find an existing parent directory".to_string())?;
        let resolved_existing_parent = existing_parent
            .canonicalize()
            .map_err(|e| format!("could not resolve parent directory: {e}"))?;
        ensure_inside_allowed_roots(&resolved_existing_parent, &roots)?;

        let parent = candidate
            .parent()
            .ok_or_else(|| "path must name a file under an allowed root".to_string())?;
        fs::create_dir_all(parent).map_err(|e| format!("could not create parent directories: {e}"))?;
        let resolved_parent = parent
            .canonicalize()
            .map_err(|e| format!("could not resolve parent directory: {e}"))?;
        ensure_inside_allowed_roots(&resolved_parent, &roots)?;
        resolved_parent.join(
            candidate
                .file_name()
                .ok_or_else(|| "path must name a file under an allowed root".to_string())?,
        )
    };

    ensure_not_sensitive(&resolved)?;
    ensure_inside_allowed_roots(&resolved, &roots)?;
    Ok(resolved)
}

fn list_local_files_at(
    roots: &[PathBuf],
    working_folder: Option<&Path>,
    requested: &str,
    include_hidden: bool,
) -> Result<FileListing, String> {
    let path = resolve_allowed_path(roots, working_folder, requested, true)?;
    let metadata = fs::metadata(&path).map_err(|e| format!("could not read folder metadata: {e}"))?;
    if !metadata.is_dir() {
        return Err("path is not a folder".to_string());
    }

    let mut entries: Vec<FileListingEntry> = Vec::new();
    let mut file_count = 0;
    let mut directory_count = 0;
    let mut hidden_count = 0;

    for item in fs::read_dir(&path).map_err(|e| format!("could not list folder: {e}"))? {
        let item = item.map_err(|e| format!("could not read folder entry: {e}"))?;
        let item_path = item.path();
        let name = item.file_name().to_string_lossy().to_string();
        let is_hidden = name.starts_with('.');
        if is_hidden {
            hidden_count += 1;
        }
        if is_hidden && !include_hidden {
            continue;
        }

        let item_type = item
            .file_type()
            .map_err(|e| format!("could not read entry type: {e}"))?;
        let kind = if item_type.is_dir() {
            directory_count += 1;
            "directory"
        } else if item_type.is_file() {
            file_count += 1;
            "file"
        } else if item_type.is_symlink() {
            "symlink"
        } else {
            "other"
        };
        let size_bytes = if item_type.is_file() {
            item.metadata().ok().map(|metadata| metadata.len())
        } else {
            None
        };
        entries.push(FileListingEntry {
            name,
            path: item_path.to_string_lossy().to_string(),
            kind: kind.to_string(),
            size_bytes,
        });
    }

    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(FileListing {
        path: path.to_string_lossy().to_string(),
        total_entries: entries.len(),
        file_count,
        directory_count,
        hidden_count,
        entries,
    })
}

#[cfg(test)]
fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    root.canonicalize()
        .map_err(|e| format!("could not resolve working folder: {e}"))
}

#[cfg(test)]
fn ensure_existing_path_inside_root(root: &Path, path: &Path) -> Result<(), String> {
    let root = canonical_root(root)?;
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("could not resolve file path: {e}"))?;
    if !resolved.starts_with(root) {
        return Err("path resolves outside the working folder".to_string());
    }
    Ok(())
}

pub fn validate_file_size(content: &str) -> Result<(), String> {
    if content.len() > MAX_FILE_BYTES {
        return Err(format!("file content is too large; max {MAX_FILE_BYTES} bytes"));
    }
    Ok(())
}

fn validate_terminal_command(command: &str) -> Result<Vec<String>, String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("command must not be empty".to_string());
    }

    for token in ["&&", "||", ";", "|", ">", "<", "$(", "`", "\n"] {
        if trimmed.contains(token) {
            return Err("shell control operators are not allowed".to_string());
        }
    }

    let argv = match trimmed {
        "pwd" => vec!["pwd"],
        "ls" => vec!["ls"],
        "ls -la" => vec!["ls", "-la"],
        "git status" => vec!["git", "status"],
        "git status --short" => vec!["git", "status", "--short"],
        "git diff --stat" => vec!["git", "diff", "--stat"],
        "git branch --show-current" => vec!["git", "branch", "--show-current"],
        _ => return Err("command is not allowed by the local terminal policy".to_string()),
    };

    Ok(argv.into_iter().map(str::to_string).collect())
}

fn resolve_terminal_cwd(
    roots: &[PathBuf],
    working_folder: Option<&Path>,
    requested: Option<&str>,
) -> Result<PathBuf, String> {
    let requested = requested
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(".");
    let cwd = resolve_allowed_path(roots, working_folder, requested, true)?;
    let metadata = fs::metadata(&cwd).map_err(|e| format!("could not read command cwd metadata: {e}"))?;
    if !metadata.is_dir() {
        return Err("terminal cwd must be a folder".to_string());
    }
    Ok(cwd)
}

fn sandbox_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn sandbox_profile(roots: &[PathBuf]) -> String {
    let mut read_paths = vec![
        "/bin".to_string(),
        "/sbin".to_string(),
        "/usr".to_string(),
        "/System".to_string(),
        "/Library".to_string(),
        "/private/etc".to_string(),
        "/etc".to_string(),
        "/dev/null".to_string(),
        "/dev/urandom".to_string(),
    ];
    read_paths.extend(roots.iter().map(|root| root.to_string_lossy().to_string()));

    let mut write_paths = vec!["/tmp".to_string(), "/private/tmp".to_string()];
    let temp_dir = std::env::temp_dir().to_string_lossy().to_string();
    if !write_paths.iter().any(|path| path == &temp_dir) {
        write_paths.push(temp_dir);
    }

    let read_rules = read_paths
        .iter()
        .map(|path| format!("  (subpath \"{}\")", sandbox_string(path)))
        .collect::<Vec<_>>()
        .join("\n");
    let write_rules = write_paths
        .iter()
        .map(|path| format!("  (subpath \"{}\")", sandbox_string(path)))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process*)\n\
         (allow file-read-metadata)\n\
         (allow file-read*\n{read_rules})\n\
         (allow file-write*\n{write_rules})\n\
         (deny network*)\n"
    )
}

fn sandboxed_command_argv(argv: &[String], profile: &str) -> Vec<String> {
    let mut sandboxed = vec![
        SANDBOX_EXEC_PATH.to_string(),
        "-p".to_string(),
        profile.to_string(),
    ];
    sandboxed.extend(argv.iter().cloned());
    sandboxed
}

fn limited_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_COMMAND_OUTPUT_BYTES {
        return text.into_owned();
    }

    let mut truncated = text.chars().take(MAX_COMMAND_OUTPUT_BYTES).collect::<String>();
    truncated.push_str("\n[output truncated]");
    truncated
}

fn run_sandboxed_process(argv: &[String], cwd: &Path, profile: &str) -> Result<(i32, String, String), String> {
    if !Path::new(SANDBOX_EXEC_PATH).is_file() {
        return Err("terminal sandbox is unavailable on this machine".to_string());
    }

    let sandboxed_argv = sandboxed_command_argv(argv, profile);
    let mut child = Command::new(&sandboxed_argv[0])
        .args(&sandboxed_argv[1..])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start sandboxed command: {e}"))?;

    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .map_err(|e| format!("could not poll sandboxed command: {e}"))?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .map_err(|e| format!("could not collect sandboxed command output: {e}"))?;
            let exit_code = output.status.code().unwrap_or(-1);
            return Ok((exit_code, limited_output(&output.stdout), limited_output(&output.stderr)));
        }

        if started.elapsed() > Duration::from_secs(TERMINAL_TIMEOUT_SECONDS) {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .map_err(|e| format!("could not collect timed-out command output: {e}"))?;
            let mut stderr = limited_output(&output.stderr);
            if !stderr.is_empty() {
                stderr.push('\n');
            }
            stderr.push_str(&format!("command timed out after {TERMINAL_TIMEOUT_SECONDS} seconds"));
            return Ok((-1, limited_output(&output.stdout), stderr));
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

fn json_response(status: u16, body: Value) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let mut response = tiny_http::Response::from_string(body.to_string()).with_status_code(status);
    response.add_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
    );
    response
}

fn read_body(request: &mut tiny_http::Request) -> Result<Value, String> {
    let mut body = String::new();
    request
        .as_reader()
        .take((MAX_FILE_BYTES + 4096) as u64)
        .read_to_string(&mut body)
        .map_err(|e| format!("could not read request body: {e}"))?;
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&body).map_err(|e| format!("invalid json body: {e}"))
}

fn is_authorized(request: &tiny_http::Request, state: &DesktopBridgeState) -> bool {
    request.headers().iter().any(|header| {
        header.field.equiv(BRIDGE_TOKEN_HEADER) && header.value.as_str() == state.token()
    })
}

fn working_folder_ref(state: &DesktopBridgeState) -> Option<PathBuf> {
    state.working_folder()
}

fn handle_request(mut request: tiny_http::Request, state: DesktopBridgeState) {
    if !is_authorized(&request, &state) && request.url() != "/health" {
        let _ = request.respond(json_response(401, json!({ "error": "unauthorized" })));
        return;
    }

    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or(request.url()).to_string();
    let response = match (method, path.as_str()) {
        (tiny_http::Method::Get, "/health") => json_response(200, json!({ "ok": true })),
        (tiny_http::Method::Get, "/workspace") => {
            let working_folder = state
                .working_folder()
                .map(|folder| folder.to_string_lossy().to_string());
            json_response(200, json!({ "working_folder": working_folder }))
        }
        (tiny_http::Method::Post, "/workspace") => {
            let body = match read_body(&mut request) {
                Ok(body) => body,
                Err(e) => {
                    let _ = request.respond(json_response(400, json!({ "error": e })));
                    return;
                }
            };
            let parsed: Result<WorkspaceRequest, _> = serde_json::from_value(body);
            match parsed {
                Ok(payload) => match state.set_working_folder(PathBuf::from(payload.path)) {
                    Ok(()) => json_response(
                        200,
                        json!({ "working_folder": state.working_folder().map(|p| p.to_string_lossy().to_string()) }),
                    ),
                    Err(e) => json_response(400, json!({ "error": e })),
                },
                Err(e) => json_response(400, json!({ "error": format!("invalid workspace request: {e}") })),
            }
        }
        (tiny_http::Method::Post, "/read-file") => {
            let body = match read_body(&mut request) {
                Ok(body) => body,
                Err(e) => {
                    let _ = request.respond(json_response(400, json!({ "error": e })));
                    return;
                }
            };
            let parsed: Result<FilePathRequest, _> = serde_json::from_value(body);
            match parsed {
                Ok(payload) => match read_local_file(&state, &payload.path) {
                    Ok(content) => json_response(200, json!({ "path": payload.path, "content": content })),
                    Err(e) => json_response(400, json!({ "error": e })),
                },
                Err(e) => json_response(400, json!({ "error": format!("invalid read request: {e}") })),
            }
        }
        (tiny_http::Method::Post, "/list-files") => {
            let body = match read_body(&mut request) {
                Ok(body) => body,
                Err(e) => {
                    let _ = request.respond(json_response(400, json!({ "error": e })));
                    return;
                }
            };
            let parsed: Result<ListFilesRequest, _> = serde_json::from_value(body);
            match parsed {
                Ok(payload) => {
                    let path = payload.path.unwrap_or_else(|| ".".to_string());
                    let include_hidden = payload.include_hidden.unwrap_or(true);
                    match list_local_files(&state, &path, include_hidden) {
                        Ok(listing) => json_response(200, json!(listing)),
                        Err(e) => json_response(400, json!({ "error": e })),
                    }
                }
                Err(e) => json_response(400, json!({ "error": format!("invalid list request: {e}") })),
            }
        }
        (tiny_http::Method::Post, "/write-file") => {
            let body = match read_body(&mut request) {
                Ok(body) => body,
                Err(e) => {
                    let _ = request.respond(json_response(400, json!({ "error": e })));
                    return;
                }
            };
            let parsed: Result<WriteFileRequest, _> = serde_json::from_value(body);
            match parsed {
                Ok(payload) => match write_local_file(&state, &payload.path, &payload.content) {
                    Ok(bytes_written) => json_response(
                        200,
                        json!({ "path": payload.path, "bytes_written": bytes_written }),
                    ),
                    Err(e) => json_response(400, json!({ "error": e })),
                },
                Err(e) => json_response(400, json!({ "error": format!("invalid write request: {e}") })),
            }
        }
        (tiny_http::Method::Post, "/run-command") => {
            let body = match read_body(&mut request) {
                Ok(body) => body,
                Err(e) => {
                    let _ = request.respond(json_response(400, json!({ "error": e })));
                    return;
                }
            };
            let parsed: Result<RunCommandRequest, _> = serde_json::from_value(body);
            match parsed {
                Ok(payload) => match run_terminal_command(&state, payload) {
                    Ok(result) => json_response(200, json!(result)),
                    Err(e) => json_response(400, json!({ "error": e })),
                },
                Err(e) => json_response(400, json!({ "error": format!("invalid command request: {e}") })),
            }
        }
        _ => json_response(404, json!({ "error": "not found" })),
    };
    let _ = request.respond(response);
}

fn read_local_file(state: &DesktopBridgeState, rel_path: &str) -> Result<String, String> {
    let working_folder = working_folder_ref(state);
    let roots = allowed_roots(state);
    let path = resolve_allowed_path(&roots, working_folder.as_deref(), rel_path, true)?;
    let metadata = fs::metadata(&path).map_err(|e| format!("could not read file metadata: {e}"))?;
    if !metadata.is_file() {
        return Err("path is not a file".to_string());
    }
    if metadata.len() as usize > MAX_FILE_BYTES {
        return Err(format!("file is too large; max {MAX_FILE_BYTES} bytes"));
    }
    fs::read_to_string(path).map_err(|e| format!("could not read file as utf-8 text: {e}"))
}

fn list_local_files(state: &DesktopBridgeState, path: &str, include_hidden: bool) -> Result<FileListing, String> {
    let working_folder = working_folder_ref(state);
    let roots = allowed_roots(state);
    list_local_files_at(&roots, working_folder.as_deref(), path, include_hidden)
}

fn write_local_file(state: &DesktopBridgeState, rel_path: &str, content: &str) -> Result<usize, String> {
    validate_file_size(content)?;
    let working_folder = working_folder_ref(state);
    let roots = allowed_roots(state);
    let path = resolve_allowed_path(&roots, working_folder.as_deref(), rel_path, false)?;
    fs::write(&path, content).map_err(|e| format!("could not write file: {e}"))?;
    Ok(content.len())
}

fn run_terminal_command(state: &DesktopBridgeState, payload: RunCommandRequest) -> Result<CommandResult, String> {
    let argv = validate_terminal_command(&payload.command)?;
    let working_folder = working_folder_ref(state);
    let roots = allowed_roots(state);
    let cwd = resolve_terminal_cwd(&roots, working_folder.as_deref(), payload.cwd.as_deref())?;
    let profile = sandbox_profile(&canonical_roots(&roots));
    let (exit_code, stdout, stderr) = run_sandboxed_process(&argv, &cwd, &profile)?;

    Ok(CommandResult {
        command: payload.command.trim().to_string(),
        cwd: cwd.to_string_lossy().to_string(),
        exit_code,
        stdout,
        stderr,
    })
}

pub fn start_bridge(state: DesktopBridgeState) -> Result<(), String> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| format!("could not start desktop bridge: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or("desktop bridge did not bind to an IP address")?
        .port();
    let url = format!("http://127.0.0.1:{port}");
    state.set_url(url);
    write_bridge_config(&state)?;

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            handle_request(request, state.clone());
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("envoy-bridge-{label}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn resolves_relative_paths_under_workspace_root() {
        let root = Path::new("/tmp/envoy-bridge-root");

        let resolved = resolve_relative_path(root, "notes/readme.md")
            .expect("relative path should resolve");

        assert_eq!(resolved, root.join("notes/readme.md"));
    }

    #[test]
    fn rejects_parent_traversal_paths() {
        let root = Path::new("/tmp/envoy-bridge-root");

        let err = resolve_relative_path(root, "../secret.txt")
            .expect_err("parent traversal must be rejected");

        assert!(err.contains("relative"));
    }

    #[test]
    fn rejects_absolute_paths() {
        let root = Path::new("/tmp/envoy-bridge-root");

        let err = resolve_relative_path(root, "/tmp/secret.txt")
            .expect_err("absolute paths must be rejected");

        assert!(err.contains("relative"));
    }

    #[test]
    fn rejects_oversized_content() {
        let content = "x".repeat(MAX_FILE_BYTES + 1);

        let err = validate_file_size(&content)
            .expect_err("oversized content must be rejected");

        assert!(err.contains("too large"));
    }

    #[test]
    fn rejects_existing_symlink_escape() {
        let nonce = uuid::Uuid::new_v4().to_string();
        let base = std::env::temp_dir().join(format!("envoy-bridge-test-{nonce}"));
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&root).expect("root dir");
        fs::create_dir_all(&outside).expect("outside dir");
        let outside_file = outside.join("secret.txt");
        fs::write(&outside_file, "secret").expect("outside file");
        let link = root.join("linked-secret.txt");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, &link).expect("symlink");

        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside_file, &link).expect("symlink");

        let err = ensure_existing_path_inside_root(&root, &link)
            .expect_err("symlink escape must be rejected");

        assert!(err.contains("outside"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn resolves_absolute_path_inside_allowed_root() {
        let root = unique_temp_dir("allowed-root");
        fs::create_dir_all(&root).expect("root dir");
        let file = root.join("notes.txt");
        fs::write(&file, "hello").expect("file");

        let resolved = resolve_allowed_path(&[root.clone()], None, file.to_str().unwrap(), true)
            .expect("path inside allowed root");

        assert_eq!(resolved, file.canonicalize().unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_absolute_path_outside_allowed_roots() {
        let allowed = unique_temp_dir("allowed-root");
        let outside = unique_temp_dir("outside-root");
        fs::create_dir_all(&allowed).expect("allowed dir");
        fs::create_dir_all(&outside).expect("outside dir");
        let file = outside.join("secret.txt");
        fs::write(&file, "secret").expect("file");

        let err = resolve_allowed_path(&[allowed.clone()], None, file.to_str().unwrap(), true)
            .expect_err("outside path must be rejected");

        assert!(err.contains("outside allowed local access roots"));
        let _ = fs::remove_dir_all(allowed);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn rejects_sensitive_paths_inside_allowed_roots() {
        let root = unique_temp_dir("allowed-root");
        let sensitive = root.join(".ssh").join("id_rsa");
        fs::create_dir_all(sensitive.parent().unwrap()).expect("sensitive dir");
        fs::write(&sensitive, "key").expect("key");

        let err = resolve_allowed_path(&[root.clone()], None, sensitive.to_str().unwrap(), true)
            .expect_err("sensitive path must be rejected");

        assert!(err.contains("sensitive"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lists_immediate_files_with_counts() {
        let root = unique_temp_dir("allowed-root");
        fs::create_dir_all(root.join("folder")).expect("folder");
        fs::write(root.join("a.txt"), "a").expect("a");
        fs::write(root.join(".hidden"), "h").expect("hidden");

        let listing = list_local_files_at(&[root.clone()], None, root.to_str().unwrap(), true)
            .expect("listing");

        assert_eq!(listing.total_entries, 3);
        assert_eq!(listing.file_count, 2);
        assert_eq!(listing.directory_count, 1);
        assert_eq!(listing.hidden_count, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_new_write_path_outside_roots_without_creating_parent() {
        let allowed = unique_temp_dir("allowed-root");
        let outside = unique_temp_dir("outside-root");
        fs::create_dir_all(&allowed).expect("allowed dir");
        let target = outside.join("new-parent").join("secret.txt");

        let err = resolve_allowed_path(&[allowed.clone()], None, target.to_str().unwrap(), false)
            .expect_err("outside write path must be rejected");

        assert!(err.contains("outside allowed local access roots"));
        assert!(!outside.join("new-parent").exists());
        let _ = fs::remove_dir_all(allowed);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn terminal_policy_allows_exact_read_only_command() {
        let argv = validate_terminal_command("git status --short")
            .expect("exact allowlisted command should pass");

        assert_eq!(
            argv,
            vec!["git".to_string(), "status".to_string(), "--short".to_string()]
        );
    }

    #[test]
    fn terminal_policy_blocks_shell_control() {
        let err = validate_terminal_command("git status && cat ~/.ssh/id_rsa")
            .expect_err("shell control operators must be rejected");

        assert!(err.contains("shell control"));
    }

    #[test]
    fn terminal_policy_blocks_unlisted_interpreters() {
        let err = validate_terminal_command("python -c 'print(1)'")
            .expect_err("interpreters must not be allowed");

        assert!(err.contains("not allowed"));
    }

    #[test]
    fn terminal_cwd_must_stay_inside_allowed_roots() {
        let allowed = unique_temp_dir("terminal-allowed-root");
        let outside = unique_temp_dir("terminal-outside-root");
        fs::create_dir_all(&allowed).expect("allowed dir");
        fs::create_dir_all(&outside).expect("outside dir");

        let err = resolve_terminal_cwd(&[allowed.clone()], None, Some(outside.to_str().unwrap()))
            .expect_err("terminal cwd outside allowed roots must be rejected");

        assert!(err.contains("outside allowed local access roots"));
        let _ = fs::remove_dir_all(allowed);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn sandbox_profile_denies_default_and_network_while_allowing_roots() {
        let root = PathBuf::from("/Users/example/Documents");

        let profile = sandbox_profile(&[root]);

        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(subpath \"/Users/example/Documents\")"));
    }

    #[test]
    fn sandbox_argv_wraps_command_with_sandbox_exec() {
        let argv = vec!["git".to_string(), "status".to_string()];

        let sandboxed = sandboxed_command_argv(&argv, "(version 1)");

        assert_eq!(
            sandboxed,
            vec![
                "/usr/bin/sandbox-exec".to_string(),
                "-p".to_string(),
                "(version 1)".to_string(),
                "git".to_string(),
                "status".to_string(),
            ]
        );
    }
}
