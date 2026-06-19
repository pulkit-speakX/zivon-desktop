# zivon-desktop

macOS desktop companion for **Envoy**. A thin native shell (Tauri v2) that hosts
the existing Envoy web UI in a window and adds OS integration. This is an
**addition** to the platform — it does **not** reimplement the chat, streaming,
or auth UI.

> **Current slice = shell + guarded desktop bridge.** Window + menubar/tray +
> global hotkey + native notifications + system-browser sign-in are in place.
> The local filesystem/terminal bridge now exists behind the Envoy capability
> layer, the frontend Desktop Access toggle, command allowlists, path grants,
> macOS `sandbox-exec`, approvals, and audit events.

## What it does

- Loads the live Envoy web UI from a **single hard-locked endpoint**
  (`http://localhost:3000`) in a native window. This build is an Envoy-only
  client: there is no in-app setting, env override, or "Back to Zivon" — the
  shell hides that link in the loaded UI and injects a `window.__ENVOY_DESKTOP__`
  flag so the web app can render its desktop ("Claude") skin (web users are
  unaffected).
- **Menubar/tray** icon: Show / Sign Out / Quit; left-click toggles the window.
- **Global hotkey** `⌥Space` (Option+Space) to summon/hide Envoy.
- **Native notifications**.
- **Sign-in via the system browser** (Google OAuth is blocked inside embedded
  webviews, so we never do it in-window — see below).
- **Guarded desktop bridge**: exposes local file list/read/write and terminal
  execution to Envoy only through the local bridge token written under
  `~/Library/Application Support/Envoy/desktop-bridge.json`.

## Architecture

```
src-tauri/            Rust core
  src/main.rs           window, tray, hotkey, notifications, OAuth loopback,
                        keychain token store, localStorage injection
  tauri.conf.json       app config (no windows declared; created in Rust)
  src/desktop_bridge.rs local HTTP bridge, file grants, terminal sandbox
  capabilities/         capability manifest — grants the splash window a tiny set
                        of permissions; the remote-content window gets NONE
  icons/                app + tray icons (generated from source-1024.png)
src/                  local shell chrome (NOT the chat UI)
  splash.html           sign-in screen (Claude-styled)
  index.html            redirects to splash.html
```

### Sign-in flow (RFC 8252 native-app loopback)

1. App launches → checks the macOS Keychain for a saved session.
2. No session → shows the splash window. You click **Sign in with Google**.
3. The app binds an ephemeral `127.0.0.1:<random>` listener with a one-time
   `state` nonce and opens your **real browser** at
   `…/login?desktop_cb=http://127.0.0.1:<port>/cb&state=<nonce>`.
4. You log in normally (real browser → NextAuth → gateway). No
   `disallowed_useragent` error because it's a genuine browser.
5. The web app POSTs the resulting `zivon_*` localStorage bundle back to the
   loopback (nonce-verified).
6. The app stores it in the **Keychain**, opens the Envoy window, and injects the
   bundle into the webview's `localStorage` (document-start) so the existing app
   picks up the session unchanged.

Loopback (not a custom `envoy://` scheme) so no other app can intercept the
token, and only our local listener — which holds the nonce — accepts it.

### Security posture

- The remote-content window (`app`) is **not** granted any Tauri capability, so
  page content (or an injected script) cannot invoke commands or plugins.
- Only the local `splash` window can call the handful of commands
  (`cmd_start_login`, `cmd_notify_test`).
- Filesystem and terminal operations are exposed only through the local desktop
  bridge. Envoy must send the bridge token, and the Python agent still passes
  through capability scopes, approvals, and audit.
- File paths must resolve inside allowed local roots. Current default roots are
  Desktop, Documents, and Downloads.
- Terminal commands must pass both an Envoy-side allowlist and the Rust bridge
  allowlist, then run under macOS `sandbox-exec`.

### Local terminal policy

Allowed command shapes in this slice:

- `pwd`
- `ls`
- `ls -la`
- `git status`
- `git status --short`
- `git diff --stat`
- `git branch --show-current`
- `cat /private/tmp/envoy-sandbox-demo/allowed/visible.txt`
- `cat /private/tmp/envoy-sandbox-demo/blocked/secret.txt`

The final `cat` command is intentionally allowlisted so users can visibly test
the OS sandbox: the command passes Envoy and bridge allowlists, then macOS
`sandbox-exec` denies reading the blocked demo path and Envoy labels the result
as `macos_sandbox`.

Blocked examples:

- `ls && cat ~/.ssh/id_rsa`: shell control operators are rejected before the OS
  sandbox runs.
- `cat ~/.ssh/id_rsa`: not allowlisted, and `~/.ssh` is a sensitive path.
- `python`, `node`, `bash`, `sh`, `zsh`: interpreters are not allowlisted.
- Any terminal `cwd` outside allowed local roots.

## The one frontend hook (called out explicitly)

This is the **only** change outside `zivon-desktop/`. In
[`zivon-frontend/src/app/login/page.tsx`](../zivon-frontend/src/app/login/page.tsx):
if `desktop_cb` + `state` query params are present, the login success path POSTs
the `zivon_*` localStorage bundle to that loopback instead of routing into the
web UI, then shows a "return to the app" screen. It is **entirely gated by the
presence of those params** — zero effect for normal web users, and reads no new
environment variable.

## Prerequisites

- Rust (stable) — `curl https://sh.rustup.rs -sSf | sh`
- Node 20+ and Yarn
- Xcode Command Line Tools

## Run it (dev)

```bash
# 1) Start the Envoy web UI (from the repo root):
make web            # serves http://localhost:3000

# 2) Run the desktop app:
cd zivon-desktop
yarn install
yarn tauri dev
```

The window opens on the splash screen → **Sign in with Google** (or use the web
app's Dev Bypass in the browser that opens) → the Envoy window appears,
signed in. Press `⌥Space` to summon/hide; use the menubar icon to Show / Sign
Out / Quit.

### Endpoint

The endpoint is **hard-locked** to `http://localhost:3000` in
[`src-tauri/src/main.rs`](src-tauri/src/main.rs) (`ENVOY_URL`). There is no in-app
setting or env override — change the constant and rebuild to repoint.

## Regenerating icons

```bash
yarn tauri icon src-tauri/icons/source-1024.png
```

## Not in this slice

General-purpose shell, arbitrary filesystem access, disconnect/revoke UI,
signing / notarization / auto-update.
