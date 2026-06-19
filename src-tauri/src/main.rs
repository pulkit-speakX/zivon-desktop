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

use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

mod desktop_bridge;
mod import;

const KEYRING_SERVICE: &str = "ai.zivon.envoy.desktop";
const KEYRING_SESSION: &str = "session";

/// The one and only endpoint this app ever loads. Hard-locked — there is no
/// in-app setting or env override. This is the Envoy web UI; the desktop app is
/// an Envoy-only client by design.
const ENVOY_URL: &str = "http://localhost:3000";

/// Window labels. The "splash" window is local shell chrome (sign-in / settings)
/// and is the ONLY window granted Tauri capabilities. The "app" window loads the
/// remote Envoy web UI and has no capability — remote content can't call back in.
const SPLASH: &str = "splash";
const APP: &str = "app";

// ---------------------------------------------------------------------------
// Session storage — macOS Keychain.
// ---------------------------------------------------------------------------

fn save_session(bundle_json: &str) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_SESSION) {
        let _ = e.set_password(bundle_json);
    }
}

fn load_session() -> Option<String> {
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
fn build_inject_script(bundle_json: &str) -> String {
    // 1) Replay the saved localStorage bundle into the remote origin before the
    //    web app's own JS runs, so it reads `zivon_token` etc. exactly as if the
    //    user had logged in in-page. `bundle_json` is a JSON string->string map,
    //    which is valid JS.
    let replay = format!(
        "try{{var b={};for(var k in b){{if(Object.prototype.hasOwnProperty.call(b,k)){{try{{window.localStorage.setItem(k,String(b[k]));}}catch(e){{}}}}}}}}catch(e){{}}",
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
    format!("(function(){{{}\n{}}})();", replay, shell)
}

fn create_app_window(app: &AppHandle, bundle_json: String) -> tauri::Result<()> {
    let parsed = tauri::Url::parse(ENVOY_URL).expect("hardcoded url is valid");

    let script = build_inject_script(&bundle_json);

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

fn create_splash_window(app: &AppHandle) -> tauri::Result<()> {
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

fn start_login(app: &AppHandle) -> Result<(), String> {
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
    tauri::Builder::default()
        // Focus the existing window if a second instance is launched.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Mutex::new(())) // reserved for future shared state
        .manage(desktop_bridge::DesktopBridgeState::new())
        .manage(import::ImportState::default())
        .invoke_handler(tauri::generate_handler![
            cmd_start_login,
            cmd_notify_test,
            import::claude_list_sessions,
            import::claude_import,
            import::open_session_in_app
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let bridge = app.state::<desktop_bridge::DesktopBridgeState>().inner().clone();
            desktop_bridge::start_bridge(bridge)?;

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
