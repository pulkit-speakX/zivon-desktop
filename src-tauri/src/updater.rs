//! Auto-update: checks the signed `latest.json` feed and, when a newer version is
//! available, downloads + installs it and relaunches. Signature verification is
//! enforced by the plugin (minisign pubkey in tauri.conf.json).

use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

/// Check for an update. When `interactive` is true (user clicked "Check for
/// Updates…"), always notify the result; otherwise stay silent unless an update
/// is found. Runs the download/install and relaunches on success.
pub fn check_and_notify(app: &AppHandle, interactive: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let updater = match app.updater() {
            Ok(u) => u,
            Err(e) => {
                if interactive {
                    notify(&app, "Update check failed", &e.to_string());
                }
                return;
            }
        };
        match updater.check().await {
            Ok(Some(update)) => {
                let version = update.version.clone();
                notify(&app, "Updating Envoy", &format!("Downloading {version}…"));
                let result = update
                    .download_and_install(|_chunk, _total| {}, || {})
                    .await;
                match result {
                    Ok(_) => {
                        notify(&app, "Update ready", "Envoy will restart to finish updating.");
                        let _ = app.restart();
                    }
                    Err(e) => notify(&app, "Update failed", &e.to_string()),
                }
            }
            Ok(None) => {
                if interactive {
                    notify(&app, "You're up to date", "Envoy is running the latest version.");
                }
            }
            Err(e) => {
                if interactive {
                    notify(&app, "Update check failed", &e.to_string());
                }
            }
        }
    });
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
}
