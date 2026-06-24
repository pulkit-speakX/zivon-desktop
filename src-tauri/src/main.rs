// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Envoy desktop companion — Phase 1 (shell only).
//!
//! What this binary does:
//!   * Hosts the existing Envoy web UI (a configurable remote URL, default
//!     http://localhost:3000) in a native window.
//!   * Menubar/tray presence, a global hotkey (Alt+Space) to summon/hide,
//!     native notifications, single-instance.
//!   * Sign-in via the *system browser* (RFC 8252 native-app loopback): we never
//!     run Google OAuth inside the embedded webview (Google blocks that). Instead
//!     we open the real browser, the web app logs in normally, and hands the
//!     resulting session tokens back to a one-shot localhost listener. Tokens are
//!     stored in the macOS Keychain and injected into the webview's localStorage
//!     so the existing web app picks up the session unchanged.
//!
//! Local desktop actions are exposed through a token-protected localhost bridge,
//! not through remote-webview Tauri IPC. The first bridge surface is scoped
//! working-folder text file read/write; terminal / git capability remains gated
//! outside this binary.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_notification::{NotificationExt, PermissionState};
use tauri_plugin_opener::OpenerExt;

mod desktop_bridge;
mod import;
mod runtime_supervisor;

const KEYRING_SERVICE: &str = "ai.zivon.envoy.desktop";
const KEYRING_SESSION: &str = "session";

/// The one and only endpoint this app ever loads. Hard-locked — there is no
/// in-app setting or env override. This is the Envoy web UI; the desktop app is
/// an Envoy-only client by design.
const ENVOY_URL: &str = "http://localhost:3000";
const OPEN_EXTERNAL_PATH: &str = "/__envoy_desktop/open_external";
/// The remote web UI navigates here (with `title` / `body` query params) to ask
/// for a native notification — the web Notification API isn't reliable in the
/// WKWebView. We intercept, fire the OS notification, and cancel the nav.
const NOTIFY_PATH: &str = "/__envoy_desktop/notify";
const AUTH_SYNC_PATH: &str = "/auth/sync";
const AUTH_SYNC_TOKEN_HEADER: &str = "X-Envoy-Desktop-Auth-Token";

/// Window labels. The "splash" window is local shell chrome (sign-in / settings)
/// and is the ONLY window granted Tauri capabilities. The "app" window loads the
/// remote Envoy web UI and has no capability — remote content can't call back in.
const SPLASH: &str = "splash";
const APP: &str = "app";

#[derive(Clone, Debug)]
struct DesktopAuthSyncConfig {
    url: String,
    token: String,
    sync_count: Arc<AtomicU64>,
}

// ---------------------------------------------------------------------------
// Session storage — macOS Keychain.
// ---------------------------------------------------------------------------

fn save_session(bundle_json: &str) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_SESSION) {
        let _ = e.set_password(bundle_json);
    }
}

pub(crate) fn load_session() -> Option<String> {
    let e = keyring::Entry::new(KEYRING_SERVICE, KEYRING_SESSION).ok()?;
    e.get_password().ok()
}

fn clear_session() {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_SESSION) {
        let _ = e.delete_credential();
    }
}

// ---------------------------------------------------------------------------
// Windows.
// ---------------------------------------------------------------------------

/// Build the document-start script that replays the saved localStorage bundle
/// into the remote origin before the web app's own JS runs, so the existing app
/// reads `zivon_token` etc. exactly as if the user had logged in in-page.
fn build_inject_script(bundle_json: &str, auth_sync: &DesktopAuthSyncConfig) -> String {
    // 1) Replay the saved localStorage bundle into the remote origin before the
    //    web app's own JS runs, so it reads `zivon_token` etc. exactly as if the
    //    user had logged in in-page. `bundle_json` is a JSON string->string map,
    //    which is valid JS.
    let replay = format!(
        "try{{var b={};var hasExistingAuth=!!(window.localStorage.getItem('zivon_token')&&window.localStorage.getItem('zivon_refresh_token'));for(var k in b){{if(Object.prototype.hasOwnProperty.call(b,k)){{if(hasExistingAuth&&(k==='zivon_token'||k==='zivon_refresh_token')){{continue;}}try{{window.localStorage.setItem(k,String(b[k]));}}catch(e){{}}}}}}}}catch(e){{}}",
        bundle_json
    );
    // 2) Mark the document as running inside the desktop shell so the Envoy web
    //    app can branch to its desktop ("Claude") skin (theme only). Web users
    //    never get this flag, so their UI is untouched.
    //
    //    NOTE: this build hosts the WHOLE Zivon app — we keep the desktop flag
    //    (for the Claude skin) but no longer hide "Back to Zivon" or otherwise
    //    restrict navigation. The full app (dashboard, admin, etc.) is reachable.
    let shell = r#"window.__ENVOY_DESKTOP__=true;try{document.documentElement.classList.add('envoy-desktop');}catch(e){}
try{window.localStorage.setItem('theme','claude-desktop');}catch(e){}"#;
    let sync_url =
        serde_json::to_string(&auth_sync.url).unwrap_or_else(|_| "\"\"".to_string());
    let sync_token =
        serde_json::to_string(&auth_sync.token).unwrap_or_else(|_| "\"\"".to_string());
    let sync = format!(
        r#"try{{
window.__ENVOY_DESKTOP_AUTH_SYNC__={{url:{sync_url},token:{sync_token}}};
function __envoySyncDesktopSession(){{
  try{{
    var c=window.__ENVOY_DESKTOP_AUTH_SYNC__;
    if(!c||!c.url||!c.token||typeof window.fetch!=='function'){{return;}}
    var storage={{}};
    for(var i=0;i<window.localStorage.length;i++){{
      var key=window.localStorage.key(i);
      if(key&&key.indexOf('zivon_')===0){{storage[key]=String(window.localStorage.getItem(key)||'');}}
    }}
    if(!storage.zivon_token||!storage.zivon_refresh_token){{return;}}
    window.fetch(c.url,{{
      method:'POST',
      headers:{{'Content-Type':'application/json','X-Envoy-Desktop-Auth-Token':c.token}},
      body:JSON.stringify({{storage:storage}})
    }}).catch(function(){{}});
  }}catch(e){{}}
}}
window.__ENVOY_SYNC_DESKTOP_SESSION__=__envoySyncDesktopSession;
if(window.queueMicrotask){{window.queueMicrotask(__envoySyncDesktopSession);}}else{{setTimeout(__envoySyncDesktopSession,0);}}
}}catch(e){{}}"#
    );
    format!("(function(){{{}\n{}\n{}}})();", replay, shell, sync)
}

fn is_allowed_external_oauth_url(url: &tauri::Url) -> bool {
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or_default();

    if scheme == "http"
        && matches!(host, "localhost" | "127.0.0.1" | "::1")
        && matches!(url.port_or_known_default(), Some(3003 | 3009))
    {
        return true;
    }

    if scheme == "https" && (host == "slack.com" || host.ends_with(".slack.com")) {
        return true;
    }

    if scheme == "https" && host == "connect.nango.dev" {
        return true;
    }

    if scheme == "https" && host == "redirectmeto.com" {
        let path = url.path();
        return path.starts_with("/http://localhost:3003/")
            || path.starts_with("/http://127.0.0.1:3003/");
    }

    false
}

fn auth_sync_cors_headers() -> Vec<tiny_http::Header> {
    vec![
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"POST, OPTIONS"[..])
            .unwrap(),
        tiny_http::Header::from_bytes(
            &b"Access-Control-Allow-Headers"[..],
            format!("Content-Type, {AUTH_SYNC_TOKEN_HEADER}").as_bytes(),
        )
        .unwrap(),
    ]
}

fn auth_sync_response(status: u16, body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let mut response = tiny_http::Response::from_string(body.to_string()).with_status_code(status);
    response.add_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
            .unwrap(),
    );
    for header in auth_sync_cors_headers() {
        response.add_header(header);
    }
    response
}

fn is_auth_sync_authorized(request: &tiny_http::Request, token: &str) -> bool {
    request.headers().iter().any(|header| {
        header.field.equiv(AUTH_SYNC_TOKEN_HEADER) && header.value.as_str() == token
    })
}

fn parse_auth_sync_payload(body: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid json body: {e}"))?;
    let storage = v
        .get("storage")
        .and_then(|value| value.as_object())
        .ok_or("missing storage object")?;
    if storage
        .get("zivon_token")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
    {
        return Err("missing zivon_token".to_string());
    }
    if storage
        .get("zivon_refresh_token")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
    {
        return Err("missing zivon_refresh_token".to_string());
    }

    let mut bundle = serde_json::Map::new();
    for (key, value) in storage {
        if !key.starts_with("zivon_") {
            continue;
        }
        match value {
            serde_json::Value::String(value) => {
                bundle.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
            serde_json::Value::Null => {}
            other => {
                bundle.insert(key.clone(), serde_json::Value::String(other.to_string()));
            }
        }
    }

    Ok(serde_json::Value::Object(bundle).to_string())
}

fn merge_auth_sync_bundle(existing_json: Option<&str>, incoming_json: &str) -> String {
    let mut merged = existing_json
        .and_then(|existing| serde_json::from_str::<serde_json::Value>(existing).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();

    if let Ok(incoming) = serde_json::from_str::<serde_json::Value>(incoming_json) {
        if let Some(incoming) = incoming.as_object() {
            for (key, value) in incoming {
                if key.starts_with("zivon_") {
                    merged.insert(key.clone(), value.clone());
                }
            }
        }
    }

    serde_json::Value::Object(merged).to_string()
}

fn start_auth_sync_server() -> Result<DesktopAuthSyncConfig, String> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| format!("could not start desktop auth sync listener: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or("desktop auth sync listener did not bind to an IP address")?
        .port();
    let token = uuid::Uuid::new_v4().to_string();
    let sync_count = Arc::new(AtomicU64::new(0));
    let config = DesktopAuthSyncConfig {
        url: format!("http://127.0.0.1:{port}{AUTH_SYNC_PATH}"),
        token,
        sync_count,
    };
    let expected_token = config.token.clone();
    let sync_count = config.sync_count.clone();

    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            if request.method() == &tiny_http::Method::Options {
                let mut response = tiny_http::Response::empty(204);
                for header in auth_sync_cors_headers() {
                    response.add_header(header);
                }
                let _ = request.respond(response);
                continue;
            }

            let path = request.url().split('?').next().unwrap_or(request.url()).to_string();
            if request.method() != &tiny_http::Method::Post || path != AUTH_SYNC_PATH {
                let _ = request.respond(auth_sync_response(404, "not found"));
                continue;
            }
            if !is_auth_sync_authorized(&request, &expected_token) {
                let _ = request.respond(auth_sync_response(401, "unauthorized"));
                continue;
            }

            let mut body = String::new();
            if request
                .as_reader()
                .take(128 * 1024)
                .read_to_string(&mut body)
                .is_err()
            {
                let _ = request.respond(auth_sync_response(400, "invalid body"));
                continue;
            }
            match parse_auth_sync_payload(&body) {
                Ok(bundle) => {
                    let merged = merge_auth_sync_bundle(load_session().as_deref(), &bundle);
                    save_session(&merged);
                    sync_count.fetch_add(1, Ordering::SeqCst);
                    let _ = request.respond(auth_sync_response(200, "ok"));
                }
                Err(err) => {
                    let _ = request.respond(auth_sync_response(400, &err));
                }
            }
        }
    });

    Ok(config)
}

pub(crate) fn create_app_window(app: &AppHandle, bundle_json: String) -> tauri::Result<()> {
    let parsed = tauri::Url::parse(ENVOY_URL).expect("hardcoded url is valid");

    let auth_sync = app.state::<DesktopAuthSyncConfig>().inner().clone();
    let script = build_inject_script(&bundle_json, &auth_sync);

    if let Some(existing) = app.get_webview_window(APP) {
        let _ = existing.eval(&script);
        let _ = existing.navigate(parsed);
        let _ = existing.show();
        let _ = existing.set_focus();
        if let Some(splash) = app.get_webview_window(SPLASH) {
            let _ = splash.close();
        }
        return Ok(());
    }

    // The web UI's "Import Claude Sessions" sidebar item navigates to this
    // sentinel path. We intercept it here, cancel the navigation, and open the
    // native import window. Remote page content can therefore only *request*
    // the import UI — it can never invoke the file-read/import commands itself
    // (those live behind the capability-scoped "import" window).
    let nav_app = app.clone();
    let win = WebviewWindowBuilder::new(app, APP, WebviewUrl::External(parsed))
        .title("Envoy")
        .inner_size(1200.0, 820.0)
        .min_inner_size(820.0, 600.0)
        // Warm Claude-dark canvas, so there is no white flash before the web UI
        // paints. Matches the in-app --background (#262624).
        .background_color(tauri::webview::Color(0x26, 0x26, 0x24, 0xff))
        .initialization_script(&script)
        .on_navigation(move |url| {
            if url.path() == "/__envoy_desktop/import" {
                let _ = import::open_import_window(&nav_app);
                return false; // cancel — don't actually navigate
            }
            if url.path() == OPEN_EXTERNAL_PATH {
                if let Some((_, target)) = url.query_pairs().find(|(key, _)| key == "url") {
                    if let Ok(target_url) = tauri::Url::parse(&target) {
                        if is_allowed_external_oauth_url(&target_url) {
                            let _ = nav_app.opener().open_url(target_url.as_str(), None::<&str>);
                        }
                    }
                }
                return false;
            }
            if url.path() == NOTIFY_PATH {
                // Suppress only when the user is already looking at the chat.
                // Use the authoritative *native* window-focus state — the web
                // UI can't gate on this reliably from inside the WKWebView.
                // is_focused() is false when the window is minimized, hidden,
                // or another app is frontmost, which is exactly when we want to
                // notify. Default to "not focused" (notify) if focus is unknown.
                let focused = nav_app
                    .get_webview_window(APP)
                    .and_then(|w| w.is_focused().ok())
                    .unwrap_or(false);
                if !focused {
                    let mut title = "Envoy".to_string();
                    let mut body = String::new();
                    for (key, value) in url.query_pairs() {
                        match key.as_ref() {
                            "title" if !value.is_empty() => title = value.into_owned(),
                            "body" => body = value.into_owned(),
                            _ => {}
                        }
                    }
                    let mut builder = nav_app.notification().builder().title(title);
                    if !body.is_empty() {
                        builder = builder.body(body);
                    }
                    let _ = builder.show();
                }
                return false; // cancel — don't actually navigate
            }
            true
        })
        .build()?;
    let _ = win.set_focus();

    if let Some(splash) = app.get_webview_window(SPLASH) {
        let _ = splash.close();
    }

    let _ = app
        .notification()
        .builder()
        .title("Envoy")
        .body("You're signed in. Press ⌥Space anytime to summon Envoy.")
        .show();

    Ok(())
}

pub(crate) fn create_splash_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window(SPLASH) {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, SPLASH, WebviewUrl::App("splash.html".into()))
        .title("Envoy")
        .inner_size(480.0, 640.0)
        .min_inner_size(420.0, 560.0)
        .resizable(true)
        .build()?;
    Ok(())
}

pub(crate) fn show_login_window(app: &AppHandle) -> tauri::Result<()> {
    clear_session();
    if let Some(app_window) = app.get_webview_window(APP) {
        let _ = app_window.close();
    }
    create_splash_window(app)
}

/// Whichever content window currently exists (prefer the app window).
fn primary_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window(APP)
        .or_else(|| app.get_webview_window(SPLASH))
}

fn toggle_main(app: &AppHandle) {
    if let Some(w) = primary_window(app) {
        let visible = w.is_visible().unwrap_or(false);
        let focused = w.is_focused().unwrap_or(false);
        if visible && focused {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = primary_window(app) {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn sign_out(app: &AppHandle) -> tauri::Result<()> {
    clear_session();
    if let Some(w) = app.get_webview_window(APP) {
        let _ = w.close();
    }
    create_splash_window(app)
}

// ---------------------------------------------------------------------------
// System-browser OAuth (loopback).
// ---------------------------------------------------------------------------

fn cors_headers() -> Vec<tiny_http::Header> {
    vec![
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"POST, OPTIONS"[..])
            .unwrap(),
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"Content-Type"[..])
            .unwrap(),
    ]
}

/// Parse the callback body and, if the one-time `state` nonce matches, return the
/// localStorage bundle (serialized) to persist + inject.
fn parse_callback(body: &str, expected_nonce: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let state = v.get("state")?.as_str()?;
    if state != expected_nonce {
        return None;
    }
    let storage = v.get("storage")?;
    if !storage.is_object() {
        return None;
    }
    Some(storage.to_string())
}

pub(crate) fn start_login(app: &AppHandle) -> Result<(), String> {
    let nonce = uuid::Uuid::new_v4().to_string();

    // Bind an ephemeral loopback listener. Only this local process (and the
    // browser the user controls, which holds the nonce) can talk to it.
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| format!("could not start local listener: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or("no loopback address")?
        .port();

    let app_for_thread = app.clone();
    let nonce_for_thread = nonce.clone();

    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            // CORS preflight.
            if request.method() == &tiny_http::Method::Options {
                let mut resp = tiny_http::Response::empty(204);
                for h in cors_headers() {
                    resp.add_header(h);
                }
                let _ = request.respond(resp);
                continue;
            }

            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);

            let bundle = parse_callback(&body, &nonce_for_thread);
            let ok = bundle.is_some();

            let mut resp = tiny_http::Response::from_string(if ok { "ok" } else { "error" })
                .with_status_code(if ok { 200 } else { 400 });
            for h in cors_headers() {
                resp.add_header(h);
            }
            let _ = request.respond(resp);

            if let Some(bundle_json) = bundle {
                save_session(&bundle_json);
                let app_main = app_for_thread.clone();
                let _ = app_for_thread.run_on_main_thread(move || {
                    let _ = create_app_window(&app_main, bundle_json);
                });
                break; // one-shot: stop the listener after a successful handoff.
            }
        }
    });

    let cb = format!("http://127.0.0.1:{port}/cb");
    let login_url = format!(
        "{}/login?desktop_cb={}&state={}",
        ENVOY_URL.trim_end_matches('/'),
        urlencoding::encode(&cb),
        urlencoding::encode(&nonce),
    );

    app.opener()
        .open_url(login_url, None::<&str>)
        .map_err(|e| format!("could not open browser: {e}"))
}

// ---------------------------------------------------------------------------
// Tauri commands (callable only from the splash window per capabilities).
// ---------------------------------------------------------------------------

#[tauri::command]
fn cmd_start_login(app: AppHandle) -> Result<(), String> {
    start_login(&app)
}

#[tauri::command]
fn cmd_notify_test(app: AppHandle) -> Result<(), String> {
    app.notification()
        .builder()
        .title("Envoy")
        .body("Notifications are working.")
        .show()
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

fn main() {
    let auth_sync = start_auth_sync_server().expect("could not start desktop auth sync listener");
    tauri::Builder::default()
        // Focus the existing window if a second instance is launched.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Mutex::new(())) // reserved for future shared state
        .manage(auth_sync)
        .manage(desktop_bridge::DesktopBridgeState::new())
        .manage(import::ImportState::default())
        .invoke_handler(tauri::generate_handler![
            cmd_start_login,
            cmd_notify_test,
            import::claude_list_sessions,
            import::claude_import,
            import::repair_desktop_session,
            import::open_session_in_app,
            import::open_sessions_in_app
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Request notification permission up front so the first real
            // notification (e.g. a clarify question while the user is in another
            // app) isn't silently dropped by macOS. No-op once granted.
            if !matches!(handle.notification().permission_state(), Ok(PermissionState::Granted)) {
                let _ = handle.notification().request_permission();
            }

            let bridge = app.state::<desktop_bridge::DesktopBridgeState>().inner().clone();
            desktop_bridge::start_bridge(bridge)?;
            runtime_supervisor::start_runtime_supervisor(handle.clone());

            // --- Tray / menubar ---
            let show_i = MenuItem::with_id(app, "show", "Show Envoy", true, None::<&str>)?;
            let import_i =
                MenuItem::with_id(app, "import_sessions", "Import Claude Sessions…", true, None::<&str>)?;
            let signout_i = MenuItem::with_id(app, "signout", "Sign Out", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Envoy", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &import_i, &signout_i, &quit_i])?;

            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("Envoy")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "import_sessions" => {
                        let _ = import::open_import_window(app);
                    }
                    "signout" => {
                        let _ = sign_out(app);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // --- Global hotkey: Alt+Space toggles the window ---
            let alt_space = Shortcut::new(Some(Modifiers::ALT), Code::Space);
            let toggle_shortcut = alt_space;
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(move |app, scut, event| {
                        if scut == &toggle_shortcut && event.state() == ShortcutState::Pressed {
                            toggle_main(app);
                        }
                    })
                    .build(),
            )?;
            app.global_shortcut().register(alt_space)?;

            // --- Initial window: app if we have a session, else splash ---
            match load_session() {
                Some(bundle) => create_app_window(&handle, bundle)?,
                None => create_splash_window(&handle)?,
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Envoy desktop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_local_nango_oauth_urls() {
        let url =
            tauri::Url::parse("http://localhost:3009/connect?session_token=abc").unwrap();
        assert!(is_allowed_external_oauth_url(&url));

        let callback =
            tauri::Url::parse("http://127.0.0.1:3003/oauth/callback").unwrap();
        assert!(is_allowed_external_oauth_url(&callback));
    }

    #[test]
    fn allows_slack_oauth_urls() {
        let url =
            tauri::Url::parse("https://slack.com/oauth/v2/authorize?client_id=abc").unwrap();
        assert!(is_allowed_external_oauth_url(&url));
    }

    #[test]
    fn rejects_non_oauth_external_urls() {
        let url = tauri::Url::parse("https://example.com/phishing").unwrap();
        assert!(!is_allowed_external_oauth_url(&url));

        let local_wrong_port = tauri::Url::parse("http://localhost:22/connect").unwrap();
        assert!(!is_allowed_external_oauth_url(&local_wrong_port));
    }

    #[test]
    fn desktop_auth_sync_accepts_only_zivon_storage_with_tokens() {
        let bundle = parse_auth_sync_payload(
            r#"{
                "storage": {
                    "zivon_token": "access",
                    "zivon_refresh_token": "refresh",
                    "zivon_last_org": "speakx-dev",
                    "theme": "claude-desktop",
                    "other": "ignored"
                }
            }"#,
        )
        .unwrap();

        let v: serde_json::Value = serde_json::from_str(&bundle).unwrap();
        assert_eq!(v.get("zivon_token").and_then(|v| v.as_str()), Some("access"));
        assert_eq!(
            v.get("zivon_refresh_token").and_then(|v| v.as_str()),
            Some("refresh")
        );
        assert_eq!(
            v.get("zivon_last_org").and_then(|v| v.as_str()),
            Some("speakx-dev")
        );
        assert!(v.get("theme").is_none());
        assert!(v.get("other").is_none());
    }

    #[test]
    fn desktop_auth_sync_rejects_payload_without_rotated_tokens() {
        let err = parse_auth_sync_payload(r#"{"storage":{"zivon_token":"access"}}"#).unwrap_err();
        assert!(err.contains("missing zivon_refresh_token"));
    }

    #[test]
    fn injection_script_does_not_replace_existing_browser_tokens() {
        let auth_sync = DesktopAuthSyncConfig {
            url: "http://127.0.0.1:12345/auth/sync".to_string(),
            token: "sync-secret".to_string(),
            sync_count: Arc::new(AtomicU64::new(0)),
        };
        let script = build_inject_script(
            r#"{"zivon_token":"stale-access","zivon_refresh_token":"stale-refresh","zivon_email":"user@example.com"}"#,
            &auth_sync,
        );

        assert!(script.contains("hasExistingAuth"));
        assert!(script.contains("k==='zivon_token'||k==='zivon_refresh_token'"));
        assert!(script.contains("__ENVOY_DESKTOP_AUTH_SYNC__"));
        assert!(script.contains("/auth/sync"));
    }

    #[test]
    fn auth_sync_merge_preserves_existing_org_metadata() {
        let existing = r#"{
            "zivon_token": "old-access",
            "zivon_refresh_token": "old-refresh",
            "zivon_last_org": "speakx-dev",
            "zivon_orgs": "[{\"slug\":\"speakx-dev\"}]"
        }"#;
        let incoming = r#"{
            "zivon_token": "new-access",
            "zivon_refresh_token": "new-refresh"
        }"#;

        let merged = merge_auth_sync_bundle(Some(existing), incoming);
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(
            parsed.get("zivon_token").and_then(|v| v.as_str()),
            Some("new-access")
        );
        assert_eq!(
            parsed.get("zivon_refresh_token").and_then(|v| v.as_str()),
            Some("new-refresh")
        );
        assert_eq!(
            parsed.get("zivon_last_org").and_then(|v| v.as_str()),
            Some("speakx-dev")
        );
        assert_eq!(
            parsed.get("zivon_orgs").and_then(|v| v.as_str()),
            Some(r#"[{"slug":"speakx-dev"}]"#)
        );
    }
}
