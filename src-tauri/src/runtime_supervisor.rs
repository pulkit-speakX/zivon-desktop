use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde_json::{json, Value};
use tauri::AppHandle;

use crate::{KEYRING_SERVICE, KEYRING_SESSION};

const KEYRING_RUNTIME_DEVICE: &str = "runtime_device_id";
const GATEWAY_URL: &str = "http://localhost:9000";
const DEFAULT_HEARTBEAT_SECONDS: u64 = 5;

#[derive(Clone, Debug)]
struct SupervisorIdentity {
    token: String,
    refresh_token: String,
    slug: String,
    bundle_json: String,
}

#[derive(Clone, Debug)]
struct RegisterResponse {
    heartbeat_interval_seconds: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct RuntimeCommand {
    command_id: String,
    command_type: String,
    session_id: String,
    run_id: Option<String>,
    payload: Value,
}

#[derive(Clone, Debug, PartialEq)]
struct StartWorkerPayload {
    command_id: String,
    session_id: String,
    run_id: String,
    pod_name: String,
    user_email: String,
    user_id: i64,
    org_slug: String,
    core_url: String,
    action_audit_url: String,
    context_metadata: String,
}

#[derive(Clone, Debug, PartialEq)]
struct RuntimeTurn {
    turn_id: String,
    session_id: String,
    run_id: String,
    content: String,
    conversation_history: Vec<Value>,
    attachments: Vec<Value>,
    extras: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
struct RuntimeTurnResult {
    response: String,
    error: String,
    aborted: bool,
    blocks: Vec<Value>,
    todos: Vec<Value>,
    ended_in_clarify: bool,
}

struct LocalRuntimeWorker {
    command_id: String,
    session_id: String,
    run_id: String,
    port: u16,
    pid: u32,
    status: String,
    child: Option<Child>,
    stop_requested: Arc<AtomicBool>,
}

impl LocalRuntimeWorker {
    #[cfg(test)]
    fn test_worker(command_id: &str, session_id: &str, run_id: &str, pid: u32, port: u16) -> Self {
        Self {
            command_id: command_id.to_string(),
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            pid,
            port,
            status: "running".to_string(),
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl StartWorkerPayload {
    fn from_command(command: &RuntimeCommand) -> Result<Self, String> {
        if command.command_type != "start_worker" {
            return Err(format!("unsupported runtime command {}", command.command_type));
        }
        let payload = &command.payload;
        let run_id = command
            .run_id
            .clone()
            .or_else(|| required_string(payload, "run_id").ok())
            .ok_or("start_worker missing run_id")?;
        Ok(Self {
            command_id: command.command_id.clone(),
            session_id: command.session_id.clone(),
            run_id,
            pod_name: required_string(payload, "pod_name")?,
            user_email: required_string(payload, "user_email")?,
            user_id: required_i64(payload, "user_id")?,
            org_slug: required_string(payload, "org_slug")?,
            core_url: required_string(payload, "core_url")?,
            action_audit_url: required_string(payload, "action_audit_url")?,
            context_metadata: required_string(payload, "context_metadata").unwrap_or_else(|_| "{}".to_string()),
        })
    }
}

pub fn start_runtime_supervisor(app: AppHandle) {
    thread::spawn(move || supervisor_loop(app));
}

fn supervisor_loop(app: AppHandle) {
    let mut registered_key: Option<String> = None;
    let mut heartbeat_interval = Duration::from_secs(DEFAULT_HEARTBEAT_SECONDS);
    let mut workers: Vec<LocalRuntimeWorker> = Vec::new();
    let mut log_state = SupervisorLogState::default();

    loop {
        refresh_worker_statuses(&mut workers);
        match supervisor_tick(registered_key.as_deref(), &mut workers) {
            Ok(result) => {
                if should_log_registered_runtime(&mut log_state, &result.registered_key) {
                    eprintln!(
                        "Envoy runtime supervisor registered Local desktop runtime for org={}",
                        registered_key_org(&result.registered_key)
                    );
                }
                registered_key = Some(result.registered_key);
                heartbeat_interval = Duration::from_secs(result.heartbeat_interval_seconds.max(1));
            }
            Err(err) => {
                let sync_completed = should_request_app_session_sync(&err)
                    && crate::import::request_app_session_sync(&app);
                if should_request_login_window(&mut log_state, &err, sync_completed) {
                    request_login_window(&app);
                }
                let mut recovered = false;
                if should_retry_after_app_session_sync(&err, sync_completed) {
                    registered_key = None;
                    refresh_worker_statuses(&mut workers);
                    match supervisor_tick(None, &mut workers) {
                        Ok(result) => {
                            registered_key = Some(result.registered_key);
                            heartbeat_interval =
                                Duration::from_secs(result.heartbeat_interval_seconds.max(1));
                            recovered = true;
                        }
                        Err(retry_err) if !is_quiet_waiting_error(&retry_err) => {
                            eprintln!(
                                "Envoy runtime supervisor heartbeat retry after session sync failed: {retry_err}"
                            );
                        }
                        Err(retry_err) => {
                            maybe_log_waiting_error(&mut log_state, &retry_err);
                        }
                    }
                }
                if !recovered && !is_quiet_waiting_error(&err) {
                    eprintln!("Envoy runtime supervisor heartbeat skipped: {err}");
                } else if !recovered {
                    maybe_log_waiting_error(&mut log_state, &err);
                }
                if should_clear_registered_runtime(&err) {
                    registered_key = None;
                }
            }
        }
        thread::sleep(heartbeat_interval);
    }
}

#[derive(Default)]
struct SupervisorLogState {
    last_waiting_reason: Option<String>,
    last_waiting_logged_at: Option<Instant>,
    last_registered_key: Option<String>,
    login_window_requested: bool,
}

fn maybe_log_waiting_error(state: &mut SupervisorLogState, err: &str) {
    if should_log_waiting_error(state, err, Instant::now(), Duration::from_secs(60)) {
        eprintln!("Envoy runtime supervisor waiting: {err}");
    }
}

fn should_request_login_window(
    state: &mut SupervisorLogState,
    err: &str,
    sync_completed: bool,
) -> bool {
    if sync_completed || !is_quiet_waiting_error(err) || state.login_window_requested {
        return false;
    }
    state.login_window_requested = true;
    true
}

fn request_login_window(app: &AppHandle) {
    let app_for_main = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(err) = crate::show_login_window(&app_for_main) {
            eprintln!("Envoy runtime supervisor could not show login window: {err}");
        }
    });
}

fn should_log_waiting_error(
    state: &mut SupervisorLogState,
    err: &str,
    now: Instant,
    min_interval: Duration,
) -> bool {
    if !is_quiet_waiting_error(err) {
        return false;
    }
    let reason = waiting_error_reason(err).unwrap_or("waiting").to_string();
    let reason_changed = state.last_waiting_reason.as_deref() != Some(reason.as_str());
    let interval_elapsed = state
        .last_waiting_logged_at
        .map(|last| now.duration_since(last) >= min_interval)
        .unwrap_or(true);
    if reason_changed || interval_elapsed {
        state.last_waiting_reason = Some(reason);
        state.last_waiting_logged_at = Some(now);
        return true;
    }
    false
}

fn waiting_error_reason(err: &str) -> Option<&'static str> {
    if err.contains("not signed in") {
        Some("not_signed_in")
    } else if err.contains("no organization found") {
        Some("no_organization")
    } else if err.contains("session expired") {
        Some("session_expired")
    } else {
        None
    }
}

fn is_quiet_waiting_error(err: &str) -> bool {
    waiting_error_reason(err).is_some()
}

fn should_log_registered_runtime(state: &mut SupervisorLogState, registered_key: &str) -> bool {
    if state.last_registered_key.as_deref() == Some(registered_key) {
        return false;
    }
    state.last_registered_key = Some(registered_key.to_string());
    state.last_waiting_reason = None;
    state.last_waiting_logged_at = None;
    state.login_window_requested = false;
    true
}

fn registered_key_org(registered_key: &str) -> &str {
    registered_key
        .split_once(':')
        .map(|(org, _)| org)
        .filter(|org| !org.is_empty())
        .unwrap_or("unknown")
}

fn should_request_app_session_sync(err: &str) -> bool {
    err.contains("not signed in")
        || err.contains("no organization found")
        || err.contains("session expired")
}

fn should_retry_after_app_session_sync(err: &str, sync_completed: bool) -> bool {
    sync_completed && should_request_app_session_sync(err)
}

fn should_clear_registered_runtime(err: &str) -> bool {
    err.contains("not signed in")
        || err.contains("no organization found")
        || err.contains("session expired")
}

struct SupervisorTickResult {
    registered_key: String,
    heartbeat_interval_seconds: u64,
}

fn supervisor_tick(
    registered_key: Option<&str>,
    workers: &mut Vec<LocalRuntimeWorker>,
) -> Result<SupervisorTickResult, String> {
    let mut identity = load_identity()?;
    let device_id = load_or_create_device_id()?;
    let key = format!("{}:{}", identity.slug, device_id);

    let mut heartbeat_interval_seconds = DEFAULT_HEARTBEAT_SECONDS;
    if registered_key != Some(key.as_str()) {
        match register_device(&identity, &device_id) {
            Ok(resp) => heartbeat_interval_seconds = resp.heartbeat_interval_seconds,
            Err(err) if err.starts_with("unauthorized:") => {
                refresh_identity(&mut identity)?;
                heartbeat_interval_seconds = register_device(&identity, &device_id)?.heartbeat_interval_seconds;
            }
            Err(err) => return Err(err),
        }
    }

    match heartbeat_device(&identity, &device_id, workers) {
        Ok(commands) => handle_runtime_commands(&identity, &device_id, workers, commands)?,
        Err(err) if err.starts_with("unauthorized:") => {
            refresh_identity(&mut identity)?;
            let commands = heartbeat_device(&identity, &device_id, workers)?;
            handle_runtime_commands(&identity, &device_id, workers, commands)?;
        }
        Err(err) if err.starts_with("not_found:") => {
            heartbeat_interval_seconds = register_device(&identity, &device_id)?.heartbeat_interval_seconds;
        }
        Err(err) => return Err(err),
    }

    Ok(SupervisorTickResult {
        registered_key: key,
        heartbeat_interval_seconds,
    })
}

fn load_or_create_device_id() -> Result<String, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_RUNTIME_DEVICE)
        .map_err(|e| format!("keychain: {e}"))?;
    if let Ok(existing) = entry.get_password() {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let device_id = uuid::Uuid::new_v4().to_string();
    entry
        .set_password(&device_id)
        .map_err(|e| format!("keychain save runtime device: {e}"))?;
    Ok(device_id)
}

fn load_identity() -> Result<SupervisorIdentity, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_SESSION)
        .map_err(|e| format!("keychain: {e}"))?;
    let bundle = entry
        .get_password()
        .map_err(|_| "not signed in - runtime supervisor is waiting for login".to_string())?;
    let v: Value = serde_json::from_str(&bundle).map_err(|e| format!("bad session bundle: {e}"))?;

    let token = v
        .get("zivon_token")
        .and_then(Value::as_str)
        .ok_or("no auth token in session - sign in again")?
        .to_string();
    let refresh_token = v
        .get("zivon_refresh_token")
        .and_then(Value::as_str)
        .ok_or("no refresh token in session - sign out and sign in again")?
        .to_string();
    let slug = extract_org_slug(&v)
        .ok_or("no organization found in session - open Envoy once, then retry")?;

    Ok(SupervisorIdentity {
        token,
        refresh_token,
        slug,
        bundle_json: bundle,
    })
}

fn extract_org_slug(v: &Value) -> Option<String> {
    v.get("zivon_last_org")
        .and_then(Value::as_str)
        .filter(|slug| !slug.trim().is_empty())
        .map(|slug| slug.trim().to_string())
        .or_else(|| {
            v.get("zivon_orgs")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|orgs| {
                    orgs.as_array()
                        .and_then(|a| a.first())
                        .and_then(|o| o.get("slug").and_then(Value::as_str))
                        .map(|slug| slug.trim().to_string())
                })
        })
}

fn refresh_identity(identity: &mut SupervisorIdentity) -> Result<(), String> {
    let resp = ureq::post(&format!("{GATEWAY_URL}/auth/refresh"))
        .set("Content-Type", "application/json")
        .send_json(json!({ "refresh_token": identity.refresh_token }));

    let v: Value = match resp {
        Ok(r) => r
            .into_json()
            .map_err(|e| format!("could not decode refreshed session: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            if code == 401 {
                return Err("session expired - runtime supervisor is waiting for fresh login".to_string());
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
    let next_bundle = update_session_bundle_tokens(&identity.bundle_json, access, refresh)?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_SESSION)
        .map_err(|e| format!("keychain: {e}"))?;
    entry
        .set_password(&next_bundle)
        .map_err(|e| format!("keychain save refreshed session: {e}"))?;

    identity.token = access.to_string();
    identity.refresh_token = refresh.to_string();
    identity.bundle_json = next_bundle;
    Ok(())
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

fn register_device(identity: &SupervisorIdentity, device_id: &str) -> Result<RegisterResponse, String> {
    let url = format!(
        "{GATEWAY_URL}/api/core/api/orgs/{}/runtime/devices/register",
        urlencoding::encode(&identity.slug)
    );
    let body = json!({
        "device_id": device_id,
        "device_name": desktop_device_name(),
        "platform": std::env::consts::OS,
        "app_version": env!("CARGO_PKG_VERSION"),
        "supervisor_protocol_version": 1,
        "capabilities": [
            "desktop.files",
            "desktop.terminal",
            "desktop.runtime.worker"
        ]
    });
    let value = send_runtime_json(&url, &identity.token, body)?;
    Ok(RegisterResponse {
        heartbeat_interval_seconds: value
            .get("heartbeat_interval_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_HEARTBEAT_SECONDS),
    })
}

fn heartbeat_device(
    identity: &SupervisorIdentity,
    device_id: &str,
    workers: &[LocalRuntimeWorker],
) -> Result<Vec<RuntimeCommand>, String> {
    let url = format!(
        "{GATEWAY_URL}/api/core/api/orgs/{}/runtime/devices/{}/heartbeat",
        urlencoding::encode(&identity.slug),
        urlencoding::encode(device_id)
    );
    let body = json!({
        "status": "online",
        "active_workers": active_worker_payload(workers),
        "resources": {}
    });
    let value = send_runtime_json(&url, &identity.token, body)?;
    Ok(parse_runtime_commands(&value))
}

fn parse_runtime_commands(value: &Value) -> Vec<RuntimeCommand> {
    value
        .get("commands")
        .and_then(Value::as_array)
        .map(|commands| {
            commands
                .iter()
                .filter_map(|command| {
                    let command_id = command.get("command_id")?.as_str()?.trim().to_string();
                    let command_type = command.get("type")?.as_str()?.trim().to_string();
                    let session_id = command.get("session_id")?.as_str()?.trim().to_string();
                    if command_id.is_empty() || command_type.is_empty() || session_id.is_empty() {
                        return None;
                    }
                    let run_id = command
                        .get("run_id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(ToString::to_string);
                    Some(RuntimeCommand {
                        command_id,
                        command_type,
                        session_id,
                        run_id,
                        payload: command.get("payload").cloned().unwrap_or_else(|| json!({})),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn handle_runtime_commands(
    identity: &SupervisorIdentity,
    device_id: &str,
    workers: &mut Vec<LocalRuntimeWorker>,
    commands: Vec<RuntimeCommand>,
) -> Result<(), String> {
    for command in commands {
        if command.command_type == "start_worker" {
            if worker_already_started(workers, &command) {
                let existing = workers
                    .iter()
                    .find(|worker| {
                        worker.command_id == command.command_id
                            || (worker.session_id == command.session_id
                                && command.run_id.as_deref() == Some(worker.run_id.as_str()))
                    });
                ack_runtime_command(
                    identity,
                    device_id,
                    &command,
                    "accepted",
                    "desktop worker is already running",
                    existing,
                )?;
                continue;
            }
            match StartWorkerPayload::from_command(&command).and_then(|payload| {
                let worker = start_local_envoy_worker(&payload)?;
                start_turn_loop(
                    identity.clone(),
                    worker.session_id.clone(),
                    worker.run_id.clone(),
                    worker.port,
                    worker.stop_requested.clone(),
                );
                Ok(worker)
            }) {
                Ok(worker) => {
                    ack_runtime_command(
                        identity,
                        device_id,
                        &command,
                        "accepted",
                        "desktop worker started and passed health check",
                        Some(&worker),
                    )?;
                    workers.push(worker);
                }
                Err(err) => {
                    ack_runtime_command(
                        identity,
                        device_id,
                        &command,
                        "failed",
                        &format!("desktop worker failed to start: {err}"),
                        None,
                    )?;
                }
            }
        } else if command.command_type == "stop_worker" {
            match stop_runtime_worker(workers, &command) {
                Ok(worker) => {
                    ack_runtime_command(
                        identity,
                        device_id,
                        &command,
                        "accepted",
                        "desktop worker stopped",
                        worker,
                    )?;
                }
                Err(err) => {
                    ack_runtime_command(
                        identity,
                        device_id,
                        &command,
                        "failed",
                        &format!("desktop worker failed to stop: {err}"),
                        None,
                    )?;
                }
            }
        } else {
            ack_runtime_command(
                identity,
                device_id,
                &command,
                "ignored",
                "desktop supervisor ignored unsupported runtime command",
                None,
            )?;
        }
    }
    Ok(())
}

fn ack_runtime_command(
    identity: &SupervisorIdentity,
    device_id: &str,
    command: &RuntimeCommand,
    status: &str,
    message: &str,
    worker: Option<&LocalRuntimeWorker>,
) -> Result<(), String> {
    let url = format!(
        "{GATEWAY_URL}/api/core/api/orgs/{}/runtime/commands/{}/ack",
        urlencoding::encode(&identity.slug),
        urlencoding::encode(&command.command_id)
    );
    let worker_payload = worker
        .map(worker_ref_payload)
        .unwrap_or_else(|| {
            let worker_status = if command.command_type == "stop_worker" {
                "stopped"
            } else if status == "accepted" {
                "starting"
            } else {
                "failed"
            };
            json!({
                "session_id": command.session_id,
                "run_id": command.run_id,
                "status": worker_status
            })
        });
    let body = json!({
        "device_id": device_id,
        "status": status,
        "message": message,
        "worker": worker_payload
    });
    send_runtime_json(&url, &identity.token, body).map(|_| ())
}

fn required_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("start_worker payload missing {key}"))
}

fn required_i64(value: &Value, key: &str) -> Result<i64, String> {
    if let Some(n) = value.get(key).and_then(Value::as_i64) {
        return Ok(n);
    }
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|s| s.trim().parse::<i64>().ok())
        .ok_or_else(|| format!("start_worker payload missing {key}"))
}

fn worker_already_started(workers: &[LocalRuntimeWorker], command: &RuntimeCommand) -> bool {
    workers.iter().any(|worker| {
        worker.command_id == command.command_id
            || (worker.session_id == command.session_id
                && command.run_id.as_deref() == Some(worker.run_id.as_str()))
    })
}

fn active_worker_payload(workers: &[LocalRuntimeWorker]) -> Vec<Value> {
    workers
        .iter()
        .filter(|worker| matches!(worker.status.as_str(), "running" | "starting"))
        .map(worker_ref_payload)
        .collect()
}

fn worker_ref_payload(worker: &LocalRuntimeWorker) -> Value {
    json!({
        "session_id": worker.session_id,
        "run_id": worker.run_id,
        "status": worker.status,
        "pid": worker.pid,
        "port": worker.port,
    })
}

fn refresh_worker_statuses(workers: &mut [LocalRuntimeWorker]) {
    for worker in workers {
        if worker.status != "running" && worker.status != "starting" {
            continue;
        }
        let Some(child) = worker.child.as_mut() else {
            continue;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                worker.status = if status.success() {
                    "stopped".to_string()
                } else {
                    "failed".to_string()
                };
            }
            Ok(None) => {}
            Err(err) => {
                worker.status = format!("failed: {err}");
            }
        }
    }
}

fn start_local_envoy_worker(payload: &StartWorkerPayload) -> Result<LocalRuntimeWorker, String> {
    let port = free_loopback_port()?;
    let envoy_repo = discover_envoy_repo_dir()?;
    let python = python_for_envoy_repo(&envoy_repo);

    let mut cmd = Command::new(&python);
    cmd.arg("-m")
        .arg("envoy2.main")
        .current_dir(&envoy_repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let python_path = build_python_path(&envoy_repo);
    cmd.env("PORT", port.to_string())
        .env("USER_EMAIL", &payload.user_email)
        .env("POD_NAME", &payload.pod_name)
        .env("ORG_SLUG", &payload.org_slug)
        .env("CORE_URL", &payload.core_url)
        .env("ACTION_AUDIT_URL", &payload.action_audit_url)
        .env("USER_ID", payload.user_id.to_string())
        .env("CONTEXT_METADATA", &payload.context_metadata)
        .env("EXECUTION_HOST", "local")
        .env("RUNTIME_BACKEND", "desktop_worker")
        .env("PYTHONPATH", python_path);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("start envoy2 process with {python}: {e}"))?;
    let pid = child.id();

    if let Err(err) = wait_for_worker_health(port, Duration::from_secs(90)) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    Ok(LocalRuntimeWorker {
        command_id: payload.command_id.clone(),
        session_id: payload.session_id.clone(),
        run_id: payload.run_id.clone(),
        port,
        pid,
        status: "running".to_string(),
        child: Some(child),
        stop_requested: Arc::new(AtomicBool::new(false)),
    })
}

fn stop_runtime_worker<'a>(
    workers: &'a mut [LocalRuntimeWorker],
    command: &RuntimeCommand,
) -> Result<Option<&'a LocalRuntimeWorker>, String> {
    let Some(worker) = workers.iter_mut().find(|worker| {
        worker.session_id == command.session_id
            && command
                .run_id
                .as_deref()
                .map(|run_id| run_id == worker.run_id)
                .unwrap_or(true)
    }) else {
        return Ok(None);
    };

    worker.stop_requested.store(true, Ordering::SeqCst);
    if worker.status == "stopped" {
        return Ok(Some(worker));
    }

    let pid = worker.pid;
    if let Some(child) = worker.child.as_mut() {
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => terminate_worker_process(pid, child)?,
            Err(err) => return Err(format!("check worker process before stop: {err}")),
        }
    }
    worker.status = "stopped".to_string();
    Ok(Some(worker))
}

#[cfg(unix)]
fn terminate_worker_process(pid: u32, child: &mut Child) -> Result<(), String> {
    let group = format!("-{pid}");
    let _ = Command::new("kill").arg("-TERM").arg(&group).status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(err) => return Err(format!("wait for worker process after TERM: {err}")),
        }
    }
    let _ = Command::new("kill").arg("-KILL").arg(&group).status();
    child
        .wait()
        .map(|_| ())
        .map_err(|e| format!("wait for worker process after KILL: {e}"))
}

#[cfg(not(unix))]
fn terminate_worker_process(_pid: u32, child: &mut Child) -> Result<(), String> {
    child.kill().map_err(|e| format!("kill worker process: {e}"))?;
    child
        .wait()
        .map(|_| ())
        .map_err(|e| format!("wait for worker process after kill: {e}"))
}

fn free_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("find free local port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("read free local port: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

fn discover_envoy_repo_dir() -> Result<PathBuf, String> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        roots.extend(exe.ancestors().map(Path::to_path_buf));
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.extend(cwd.ancestors().map(Path::to_path_buf));
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Desktop").join("speakx").join("zivon-v2"));
    }

    for root in roots {
        for candidate in [
            root.join("zivon-envoy"),
            root.parent()
                .map(|parent| parent.join("zivon-envoy"))
                .unwrap_or_else(|| root.join("__missing__")),
        ] {
            if candidate.join("envoy2").join("main.py").is_file() {
                return Ok(candidate);
            }
        }
    }
    Err("could not locate sibling zivon-envoy/envoy2 runtime package".to_string())
}

fn python_for_envoy_repo(envoy_repo: &Path) -> String {
    for candidate in [
        envoy_repo.join(".venv").join("bin").join("python"),
        envoy_repo
            .parent()
            .unwrap_or(envoy_repo)
            .join(".venv")
            .join("bin")
            .join("python"),
    ] {
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    "python3".to_string()
}

fn build_python_path(envoy_repo: &Path) -> String {
    let mut parts = vec![envoy_repo.to_string_lossy().to_string()];
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        if !existing.trim().is_empty() {
            parts.push(existing);
        }
    }
    parts.join(":")
}

fn wait_for_worker_health(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("http://127.0.0.1:{port}/health");
    let mut last_err = String::new();
    while Instant::now() < deadline {
        match ureq::get(&url).timeout(Duration::from_secs(1)).call() {
            Ok(resp) => {
                let status = resp.status();
                match resp.into_json::<Value>() {
                    Ok(v)
                        if status < 500
                            && v.get("agent_ready").and_then(Value::as_bool).unwrap_or(false) =>
                    {
                        return Ok(())
                    }
                    Ok(v) => last_err = format!("HTTP {status}: {v}"),
                    Err(err) => last_err = format!("HTTP {status}: {err}"),
                }
            }
            Err(ureq::Error::Status(code, resp)) => {
                last_err = format!("HTTP {code}: {}", resp.into_string().unwrap_or_default());
            }
            Err(ureq::Error::Transport(err)) => {
                last_err = err.to_string();
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!("envoy2 worker on port {port} did not become healthy: {last_err}"))
}

fn start_turn_loop(
    identity: SupervisorIdentity,
    session_id: String,
    run_id: String,
    port: u16,
    stop_requested: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        if stop_requested.load(Ordering::SeqCst) {
            break;
        }
        match poll_runtime_turn(&identity, &run_id) {
            Ok(Some(turn)) => {
                if stop_requested.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(err) = process_runtime_turn(&identity, port, &turn) {
                    let _ = complete_runtime_turn(
                        &identity,
                        &turn,
                        &RuntimeTurnResult {
                            response: String::new(),
                            error: err,
                            aborted: false,
                            blocks: vec![],
                            todos: vec![],
                            ended_in_clarify: false,
                        },
                    );
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!(
                    "Envoy runtime worker turn poll failed for session={session_id} run={run_id}: {err}"
                );
                thread::sleep(Duration::from_secs(2));
            }
        }
    });
}

fn poll_runtime_turn(identity: &SupervisorIdentity, run_id: &str) -> Result<Option<RuntimeTurn>, String> {
    let url = format!(
        "{GATEWAY_URL}/api/core/api/orgs/{}/runtime/workers/{}/turns/poll",
        urlencoding::encode(&identity.slug),
        urlencoding::encode(run_id)
    );
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", identity.token))
        .send_json(json!({ "wait_seconds": 20 }));
    match resp {
        Ok(r) if r.status() == 204 => Ok(None),
        Ok(r) => {
            let value: Value = r
                .into_json()
                .map_err(|e| format!("could not decode runtime turn poll response: {e}"))?;
            value
                .get("turn")
                .filter(|turn| !turn.is_null())
                .map(runtime_turn_from_value)
                .transpose()
        }
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            Err(format!(
                "runtime_turn_poll: HTTP {code}: {}",
                detail.chars().take(180).collect::<String>()
            ))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("runtime_turn_poll network: {t}")),
    }
}

fn runtime_turn_from_value(value: &Value) -> Result<RuntimeTurn, String> {
    Ok(RuntimeTurn {
        turn_id: required_runtime_turn_string(value, "turn_id")?,
        session_id: required_runtime_turn_string(value, "session_id")?,
        run_id: required_runtime_turn_string(value, "run_id")?,
        content: required_runtime_turn_string(value, "content")?,
        conversation_history: value
            .get("conversation_history")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        attachments: value
            .get("attachments")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        extras: value
            .get("extras")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    })
}

fn required_runtime_turn_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .map(ToString::to_string)
        .ok_or_else(|| format!("runtime turn missing {key}"))
}

fn process_runtime_turn(identity: &SupervisorIdentity, port: u16, turn: &RuntimeTurn) -> Result<(), String> {
    let thread_id = format!("{}:{}", identity.slug, turn.session_id);
    let url = format!(
        "http://127.0.0.1:{port}/v3/{}/stream",
        urlencoding::encode(&thread_id)
    );
    let body = build_envoy2_turn_body(identity, turn);
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(Value::Object(body));

    match resp {
        Ok(r) => {
            let result = relay_envoy2_sse(identity, turn, r)?;
            complete_runtime_turn(identity, turn, &result)
        }
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            Err(format!(
                "local envoy2 stream returned HTTP {code}: {}",
                detail.chars().take(240).collect::<String>()
            ))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("local envoy2 stream network: {t}")),
    }
}

fn build_envoy2_turn_body(identity: &SupervisorIdentity, turn: &RuntimeTurn) -> serde_json::Map<String, Value> {
    let mut body = serde_json::Map::new();
    body.insert("content".to_string(), Value::String(turn.content.clone()));
    body.insert("user_email".to_string(), Value::String(identity_user_email(identity)));
    body.insert("org_slug".to_string(), Value::String(identity.slug.clone()));
    body.insert(
        "conversation_history".to_string(),
        Value::Array(turn.conversation_history.clone()),
    );
    body.insert("attachments".to_string(), Value::Array(turn.attachments.clone()));

    for extra in &turn.extras {
        if let Some(obj) = extra.as_object() {
            for (key, value) in obj {
                if matches!(key.as_str(), "content" | "user_email" | "org_slug" | "conversation_history") {
                    continue;
                }
                body.insert(key.clone(), value.clone());
            }
        }
    }
    body
}

fn identity_user_email(identity: &SupervisorIdentity) -> String {
    serde_json::from_str::<Value>(&identity.bundle_json)
        .ok()
        .and_then(|v| {
            v.get("zivon_user")
                .and_then(Value::as_str)
                .or_else(|| v.get("zivon_email").and_then(Value::as_str))
                .or_else(|| v.get("email").and_then(Value::as_str))
                .map(ToString::to_string)
        })
        .unwrap_or_default()
}

fn relay_envoy2_sse(
    identity: &SupervisorIdentity,
    turn: &RuntimeTurn,
    response: ureq::Response,
) -> Result<RuntimeTurnResult, String> {
    let mut reader = BufReader::new(response.into_reader());
    let mut line = String::new();
    let mut result = RuntimeTurnResult {
        response: String::new(),
        error: String::new(),
        aborted: false,
        blocks: vec![],
        todos: vec![],
        ended_in_clarify: false,
    };

    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| format!("read local envoy2 SSE: {e}"))?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        let Some(raw) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let frame: Value = serde_json::from_str(raw)
            .map_err(|e| format!("decode local envoy2 SSE frame: {e}: {raw}"))?;
        apply_runtime_frame(&mut result, &frame);
        post_runtime_turn_event(identity, turn, &frame)?;
        if frame.get("type").and_then(Value::as_str) == Some("done") || !result.error.is_empty() {
            break;
        }
    }

    Ok(result)
}

fn apply_runtime_frame(result: &mut RuntimeTurnResult, frame: &Value) {
    match frame.get("type").and_then(Value::as_str).unwrap_or_default() {
        "text" => {
            if let Some(content) = frame.get("content").and_then(Value::as_str) {
                result.response.push_str(content);
            }
        }
        "turn_blocks" => {
            let partial = frame.get("partial").and_then(Value::as_bool).unwrap_or(false);
            if !partial {
                result.blocks = frame
                    .get("blocks")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
            }
        }
        "todos" => {
            result.todos = frame
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        }
        "clarify_request" => {
            result.ended_in_clarify = true;
        }
        "error" => {
            result.error = frame
                .get("message")
                .or_else(|| frame.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("local envoy2 runtime error")
                .to_string();
        }
        _ => {}
    }
}

fn post_runtime_turn_event(identity: &SupervisorIdentity, turn: &RuntimeTurn, frame: &Value) -> Result<(), String> {
    let url = format!(
        "{GATEWAY_URL}/api/core/api/orgs/{}/runtime/turns/{}/events",
        urlencoding::encode(&identity.slug),
        urlencoding::encode(&turn.turn_id)
    );
    post_runtime_json_empty_ok(
        &url,
        &identity.token,
        json!({
            "session_id": turn.session_id,
            "run_id": turn.run_id,
            "frame": frame,
        }),
    )
}

fn complete_runtime_turn(
    identity: &SupervisorIdentity,
    turn: &RuntimeTurn,
    result: &RuntimeTurnResult,
) -> Result<(), String> {
    let url = format!(
        "{GATEWAY_URL}/api/core/api/orgs/{}/runtime/turns/{}/complete",
        urlencoding::encode(&identity.slug),
        urlencoding::encode(&turn.turn_id)
    );
    post_runtime_json_empty_ok(
        &url,
        &identity.token,
        json!({
            "session_id": turn.session_id,
            "run_id": turn.run_id,
            "response": result.response,
            "error": result.error,
            "aborted": result.aborted,
            "blocks": result.blocks,
            "todos": result.todos,
            "ended_in_clarify": result.ended_in_clarify,
        }),
    )
}

fn post_runtime_json_empty_ok(url: &str, token: &str, body: Value) -> Result<(), String> {
    let resp = ureq::post(url)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body);
    match resp {
        Ok(r) if (200..300).contains(&r.status()) => Ok(()),
        Ok(r) => Err(format!("runtime_api: HTTP {}", r.status())),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            Err(format!(
                "runtime_api: HTTP {code}: {}",
                detail.chars().take(180).collect::<String>()
            ))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("runtime_api network: {t}")),
    }
}

fn send_runtime_json(url: &str, token: &str, body: Value) -> Result<Value, String> {
    let resp = ureq::post(url)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body);
    match resp {
        Ok(r) => r
            .into_json()
            .map_err(|e| format!("could not decode runtime response: {e}")),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            let prefix = match code {
                401 => "unauthorized",
                404 => "not_found",
                _ => "runtime_api",
            };
            Err(format!(
                "{prefix}: HTTP {code}: {}",
                detail.chars().take(180).collect::<String>()
            ))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("runtime_api network: {t}")),
    }
}

fn desktop_device_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|user| format!("{user}'s Mac"))
        })
        .unwrap_or_else(|| "Envoy Desktop".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_last_org_before_org_list() {
        let v = json!({
            "zivon_last_org": "speakx-dev",
            "zivon_orgs": "[{\"slug\":\"fallback\"}]"
        });

        assert_eq!(extract_org_slug(&v), Some("speakx-dev".to_string()));
    }

    #[test]
    fn extracts_first_org_from_stringified_org_list() {
        let v = json!({
            "zivon_orgs": "[{\"slug\":\"speakx-dev\"}]"
        });

        assert_eq!(extract_org_slug(&v), Some("speakx-dev".to_string()));
    }

    #[test]
    fn updates_session_bundle_tokens_without_dropping_metadata() {
        let updated = update_session_bundle_tokens(
            r#"{"zivon_token":"old","zivon_refresh_token":"old-r","zivon_last_org":"speakx-dev"}"#,
            "new",
            "new-r",
        )
        .unwrap();
        let v: Value = serde_json::from_str(&updated).unwrap();

        assert_eq!(v.get("zivon_token").and_then(Value::as_str), Some("new"));
        assert_eq!(v.get("zivon_refresh_token").and_then(Value::as_str), Some("new-r"));
        assert_eq!(v.get("zivon_last_org").and_then(Value::as_str), Some("speakx-dev"));
    }

    #[test]
    fn parses_runtime_commands_from_heartbeat_response() {
        let value = json!({
            "commands": [
                {
                    "command_id": "cmd-1",
                    "type": "start_worker",
                    "session_id": "sess-1",
                    "run_id": "run-1",
                    "payload": {"ignored": true}
                },
                {"command_id": "", "type": "start_worker", "session_id": "bad"}
            ]
        });

        assert_eq!(
            parse_runtime_commands(&value),
            vec![RuntimeCommand {
                command_id: "cmd-1".to_string(),
                command_type: "start_worker".to_string(),
                session_id: "sess-1".to_string(),
                run_id: Some("run-1".to_string()),
                payload: json!({"ignored": true}),
            }]
        );
    }

    #[test]
    fn classifies_identity_errors_as_sync_recoverable() {
        assert!(should_request_app_session_sync(
            "not signed in - runtime supervisor is waiting for login"
        ));
        assert!(should_request_app_session_sync(
            "no organization found in session - open Envoy once, then retry"
        ));
        assert!(should_request_app_session_sync(
            "session expired - runtime supervisor is waiting for fresh login"
        ));
        assert!(!should_request_app_session_sync("runtime_api: HTTP 502"));
    }

    #[test]
    fn session_expiry_waiting_state_is_quiet() {
        assert!(is_quiet_waiting_error(
            "session expired - runtime supervisor is waiting for fresh login"
        ));
    }

    #[test]
    fn waiting_supervisor_errors_log_once_per_reason_or_interval() {
        let mut state = SupervisorLogState::default();
        let now = Instant::now();

        assert!(should_log_waiting_error(
            &mut state,
            "not signed in - runtime supervisor is waiting for login",
            now,
            Duration::from_secs(60)
        ));
        assert!(!should_log_waiting_error(
            &mut state,
            "not signed in - runtime supervisor is waiting for login",
            now + Duration::from_secs(10),
            Duration::from_secs(60)
        ));
        assert!(should_log_waiting_error(
            &mut state,
            "session expired - runtime supervisor is waiting for fresh login",
            now + Duration::from_secs(20),
            Duration::from_secs(60)
        ));
        assert!(should_log_waiting_error(
            &mut state,
            "session expired - runtime supervisor is waiting for fresh login",
            now + Duration::from_secs(90),
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn registered_runtime_log_resets_waiting_state() {
        let mut state = SupervisorLogState::default();
        let now = Instant::now();
        assert!(should_log_waiting_error(
            &mut state,
            "session expired - runtime supervisor is waiting for fresh login",
            now,
            Duration::from_secs(60)
        ));

        assert!(should_log_registered_runtime(&mut state, "speakx-dev:mac-1"));
        assert!(!should_log_registered_runtime(&mut state, "speakx-dev:mac-1"));
        assert!(should_log_waiting_error(
            &mut state,
            "session expired - runtime supervisor is waiting for fresh login",
            now + Duration::from_secs(1),
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn login_window_is_requested_once_when_auth_sync_cannot_recover() {
        let mut state = SupervisorLogState::default();

        assert!(should_request_login_window(
            &mut state,
            "session expired - runtime supervisor is waiting for fresh login",
            false
        ));
        assert!(!should_request_login_window(
            &mut state,
            "session expired - runtime supervisor is waiting for fresh login",
            false
        ));
        assert!(!should_request_login_window(
            &mut state,
            "runtime_api network: connection refused",
            false
        ));
    }

    #[test]
    fn login_window_is_not_requested_when_auth_sync_recovers() {
        let mut state = SupervisorLogState::default();

        assert!(!should_request_login_window(
            &mut state,
            "session expired - runtime supervisor is waiting for fresh login",
            true
        ));
    }

    #[test]
    fn successful_registration_allows_future_login_window_request() {
        let mut state = SupervisorLogState::default();

        assert!(should_request_login_window(
            &mut state,
            "not signed in - runtime supervisor is waiting for login",
            false
        ));
        assert!(should_log_registered_runtime(&mut state, "speakx-dev:mac-1"));
        assert!(should_request_login_window(
            &mut state,
            "not signed in - runtime supervisor is waiting for login",
            false
        ));
    }

    #[test]
    fn registered_runtime_log_uses_org_only() {
        assert_eq!(registered_key_org("speakx-dev:device-id"), "speakx-dev");
        assert_eq!(registered_key_org(""), "unknown");
    }

    #[test]
    fn retries_tick_immediately_only_after_completed_session_sync() {
        assert!(should_retry_after_app_session_sync(
            "session expired - runtime supervisor is waiting for fresh login",
            true
        ));
        assert!(!should_retry_after_app_session_sync(
            "session expired - runtime supervisor is waiting for fresh login",
            false
        ));
        assert!(!should_retry_after_app_session_sync("runtime_api: HTTP 502", true));
    }

    #[test]
    fn parses_start_worker_payload_from_command() {
        let command = RuntimeCommand {
            command_id: "cmd-1".to_string(),
            command_type: "start_worker".to_string(),
            session_id: "sess-1".to_string(),
            run_id: Some("run-1".to_string()),
            payload: json!({
                "pod_name": "pulkit-envoy2-1",
                "user_email": "pulkit@example.com",
                "user_id": 42,
                "org_slug": "acme",
                "core_url": "http://localhost:8000",
                "action_audit_url": "http://localhost:8001",
                "context_metadata": "{\"session_id\":\"sess-1\"}"
            }),
        };

        let payload = StartWorkerPayload::from_command(&command).expect("payload should parse");

        assert_eq!(payload.session_id, "sess-1");
        assert_eq!(payload.run_id, "run-1");
        assert_eq!(payload.pod_name, "pulkit-envoy2-1");
        assert_eq!(payload.user_email, "pulkit@example.com");
        assert_eq!(payload.org_slug, "acme");
    }

    #[test]
    fn duplicate_start_worker_command_is_not_launched_twice() {
        let workers = vec![LocalRuntimeWorker::test_worker(
            "cmd-1", "sess-1", "run-1", 12345, 49152,
        )];
        let command = RuntimeCommand {
            command_id: "cmd-1".to_string(),
            command_type: "start_worker".to_string(),
            session_id: "sess-1".to_string(),
            run_id: Some("run-1".to_string()),
            payload: json!({}),
        };

        assert!(worker_already_started(&workers, &command));
    }

    #[test]
    fn active_worker_payload_reports_running_pid_and_port() {
        let workers = vec![LocalRuntimeWorker::test_worker(
            "cmd-1", "sess-1", "run-1", 12345, 49152,
        )];

        let refs = active_worker_payload(&workers);

        assert_eq!(
            refs[0].get("session_id").and_then(Value::as_str),
            Some("sess-1")
        );
        assert_eq!(refs[0].get("run_id").and_then(Value::as_str), Some("run-1"));
        assert_eq!(refs[0].get("status").and_then(Value::as_str), Some("running"));
        assert_eq!(refs[0].get("pid").and_then(Value::as_i64), Some(12345));
        assert_eq!(refs[0].get("port").and_then(Value::as_i64), Some(49152));
    }

    #[test]
    fn stop_worker_marks_worker_stopped_and_removes_it_from_active_payload() {
        let mut workers = vec![LocalRuntimeWorker::test_worker(
            "cmd-1", "sess-1", "run-1", 12345, 49152,
        )];
        let command = RuntimeCommand {
            command_id: "stop-1".to_string(),
            command_type: "stop_worker".to_string(),
            session_id: "sess-1".to_string(),
            run_id: Some("run-1".to_string()),
            payload: json!({}),
        };

        let stopped = stop_runtime_worker(&mut workers, &command)
            .expect("stop command should succeed")
            .expect("worker should be found");

        assert_eq!(stopped.status, "stopped");
        assert!(stopped.stop_requested.load(Ordering::SeqCst));
        assert!(active_worker_payload(&workers).is_empty());
    }

    #[test]
    fn runtime_frame_accumulator_tracks_text_blocks_todos_and_clarify() {
        let mut result = RuntimeTurnResult {
            response: String::new(),
            error: String::new(),
            aborted: false,
            blocks: vec![],
            todos: vec![],
            ended_in_clarify: false,
        };

        apply_runtime_frame(&mut result, &json!({"type":"text","content":"hel"}));
        apply_runtime_frame(&mut result, &json!({"type":"text","content":"lo"}));
        apply_runtime_frame(
            &mut result,
            &json!({"type":"turn_blocks","partial":true,"blocks":[{"ignored":true}]}),
        );
        apply_runtime_frame(
            &mut result,
            &json!({"type":"turn_blocks","blocks":[{"type":"assistant","text":"hello"}]}),
        );
        apply_runtime_frame(&mut result, &json!({"type":"todos","items":[{"text":"ship"}]}));
        apply_runtime_frame(&mut result, &json!({"type":"clarify_request"}));

        assert_eq!(result.response, "hello");
        assert_eq!(result.blocks, vec![json!({"type":"assistant","text":"hello"})]);
        assert_eq!(result.todos, vec![json!({"text":"ship"})]);
        assert!(result.ended_in_clarify);
    }

    #[test]
    fn envoy2_turn_body_merges_safe_extras_without_overwriting_core_fields() {
        let identity = SupervisorIdentity {
            token: "token".to_string(),
            refresh_token: "refresh".to_string(),
            slug: "acme".to_string(),
            bundle_json: r#"{"zivon_email":"pulkit@example.com"}"#.to_string(),
        };
        let turn = RuntimeTurn {
            turn_id: "turn-1".to_string(),
            session_id: "sess-1".to_string(),
            run_id: "run-1".to_string(),
            content: "hello".to_string(),
            conversation_history: vec![json!({"role":"user","content":"prior"})],
            attachments: vec![],
            extras: vec![json!({
                "content": "malicious overwrite",
                "selected_model": "opus",
                "desktop_access_enabled": false
            })],
        };

        let body = build_envoy2_turn_body(&identity, &turn);

        assert_eq!(body.get("content").and_then(Value::as_str), Some("hello"));
        assert_eq!(body.get("user_email").and_then(Value::as_str), Some("pulkit@example.com"));
        assert_eq!(body.get("selected_model").and_then(Value::as_str), Some("opus"));
        assert_eq!(
            body.get("desktop_access_enabled").and_then(Value::as_bool),
            Some(false)
        );
    }
}
