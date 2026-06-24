# Releasing Envoy Desktop

This runbook covers cutting a release, version bumping, one-time DevOps setup, updater key rotation, and dry-run validation for the Envoy desktop app (`ai.zivon.envoy.desktop`).

## 1. Version Bumping Rules

The version is the **single source of truth** — stored in **git tags** (`vMAJOR.MINOR.PATCH`). Use semantic versioning and decide the bump at tag-push time, following this table:

| Bump | When | Examples |
|---|---|---|
| **PATCH** | Fixes / invisible changes; no new capability | crash/tray fix, dependency bump, copy/icon tweaks, faster startup |
| **MINOR** | New capability, backward compatible | new tray action, new integration, bundled runtime (Phase 2), new hotkey |
| **MAJOR** | Breaks compatibility or forces user action | auth model change requiring re-login, min macOS raised, requires reinstall |

**Rule of thumb:** *"Will an auto-update silently work and behave as the user expects?"*
- Yes + nothing new → PATCH
- Yes + something new → MINOR
- No → MAJOR

Expect mostly PATCH/MINOR; MAJOR is rare and deliberate.

**Policy: manual now, automate later.** Currently the engineer picks the bump per the table above (zero setup). If release frequency grows, adopt Conventional Commits + `release-please` / `semantic-release` to propose the bump and changelog from commit history.

## 2. Cutting a Release

A release is **triggered by a git tag**. The CI pipeline automatically builds, signs, notarizes, and publishes.

### Steps

1. **Decide the version bump** using the rules in §1. The current version is in `tauri.conf.json` (`version` field).

2. **Tag and push:**
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
   The tag push automatically triggers `.github/workflows/release.yml`, which:
   - Extracts the version from the tag
   - Runs `node scripts/set-version.mjs vX.Y.Z` to update `tauri.conf.json`
   - Builds with `yarn tauri build --target universal-apple-darwin --bundles dmg,app`
   - Signs and notarizes the macOS bundle
   - Uploads to S3 and publishes `latest.json`
   - Notifies Slack

3. **Monitor the build:** check the [Actions tab](https://github.com/pulkit-speakx/zivon-desktop/actions) to confirm `release.yml` succeeds. Look for:
   - ✅ Build, sign & notarize step completes
   - ✅ Notarization succeeds (check the step logs for `notarytool` success)
   - ✅ Publish to S3 succeeds
   - ✅ Slack notification posted

### Release Candidates (RC)

For testing or dry-runs (see §5), use pre-release tags that follow SemVer:

```bash
git tag v0.1.1-rc.1
git push origin v0.1.1-rc.1
```

Pre-release tags (e.g. `v0.1.1-rc.2`) sort below the final release (`v0.1.1`), so the auto-updater will not prompt users to "update" to an RC. Use RC tags freely for testing.

### Manual trigger (workflow_dispatch)

You can also trigger a release build without pushing a tag using `workflow_dispatch`:

```bash
gh workflow run release.yml -f tag=v0.1.1-rc.1
```

This is equivalent to `git tag v0.1.1-rc.1 && git push origin v0.1.1-rc.1` but without committing the tag to git history. Useful for testing the pipeline quickly.

## 3. One-Time DevOps Setup

The release pipeline requires infrastructure and credentials that must be set up **once by a human** before the first release. These are never committed and never repeated.

### 3.1 Apple Signing & Notarization

**Action items:**

1. **Generate a dedicated Developer ID Application certificate** (Account Holder role required).
   - This is a **distinct cert type** from iOS `match` certs — generating it does not regress iOS signing.
   - In Apple Developer Portal, create a new "Developer ID Application" cert, download the `.p12`.
   - The cert should be issued to the SpeakX account (Team ID: already known to mobile CI as `DEVELOPER_PORTAL_TEAM_ID`).

2. **Create a dedicated app-specific password** for notarization.
   - Go to [appleid.apple.com](https://appleid.apple.com) → Security → App-Specific Passwords.
   - Create a new password labeled `"Envoy Desktop Notarization"`.
   - This password is for the *same* Apple ID as the cert, but is specific to desktop and never shared with iOS.

### 3.2 AWS Storage & CDN

**Action items:**

1. **Provision a dedicated S3 bucket** (proposed name: `zivon-desktop-builds`).
   - Region: `ap-south-1` (matches existing mobile pipeline).
   - Enable versioning (optional, for safety).
   - Never use the mobile bucket (`ivykids-app-builds`).

2. **Create a dedicated CloudFront distribution** pointing to the bucket.
   - Enable cache behaviors:
     - `/latest.json` → TTL 0 (no-cache, always fresh)
     - `/*.dmg`, `/*.tar.gz`, `/*.sig` → TTL 1 year (immutable)
   - Distribution domain or custom domain: `downloads.zivon.ai` (requires ACM cert + DNS CNAME).

3. **Provision an IAM key** scoped **only** to the desktop bucket.
   - Create an IAM user or service account with S3 GetObject/PutObject/DeleteObject permissions on the desktop bucket **only**.
   - Never grant access to `ivykids-app-builds` or other mobile resources.

### 3.3 GitHub Secrets

Add the following secrets to the `zivon-desktop` GitHub repository (Settings → Secrets and variables → Actions):

**From Apple (see §3.1):**
- `APPLE_CERTIFICATE` — base64-encoded `.p12` file from the Developer ID Application cert
  ```bash
  base64 -i DevID.p12 | pbcopy
  ```
- `APPLE_CERTIFICATE_PASSWORD` — password used when exporting the `.p12`
- `APPLE_SIGNING_IDENTITY` — e.g. `Developer ID Application: SpeakX (TEAMID)` (visible in Keychain)
- `APPLE_ID` — Apple ID email (e.g. `notary@example.com`)
- `APPLE_PASSWORD` — the dedicated app-specific password from §3.1
- `APPLE_TEAM_ID` — Apple Team ID (copy from `DEVELOPER_PORTAL_TEAM_ID` in `spx-codebase`)

**Updater signing (see §4):**
- `TAURI_SIGNING_PRIVATE_KEY` — minisign private key (generated via `tauri signer generate`)
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — password protecting the private key (recommended: a strong random string)

**AWS (see §3.2):**
- `AWS_ACCESS_KEY_ID` — IAM access key ID
- `AWS_SECRET_ACCESS_KEY` — IAM secret access key
- `AWS_REGION` — `ap-south-1`
- `DESKTOP_S3_BUCKET` — bucket name (e.g. `zivon-desktop-builds`)
- `DESKTOP_CLOUDFRONT_DISTRIBUTION_ID` — CloudFront distribution ID

**Slack notifications:**
- `DESKTOP_SLACK_WEBHOOK_URL` — webhook URL for a dedicated desktop build channel (never the mobile channel)

**Optional:**
- `ZIVON_ENVOY_URL` — environment variable (GitHub Actions "Variable", not Secret) that specifies the Envoy endpoint at runtime (e.g. production URL for Phase 2). Defaults to `http://localhost:3000` if unset.

### 3.4 Isolation Rules (Do's and Don'ts)

To protect the existing iOS pipeline, follow these rules **always**:

- ✅ **DO** use GitHub-hosted `macos-latest` runners for desktop CI (defined in `.github/workflows/release.yml`).
- ✅ **DO** create dedicated credentials (cert, app-specific password, S3 bucket, CloudFront distribution, Slack webhook).
- ✅ **DO** copy **identifiers only** from the mobile side (Apple Team ID value).

- ❌ **NEVER** use the self-hosted `app-builder` runner fleet (that is for iOS only).
- ❌ **NEVER** share the mobile bucket `ivykids-app-builds`.
- ❌ **NEVER** post build notifications to the mobile Slack channel.
- ❌ **NEVER** reuse the iOS app-specific password (even though it's the same Apple ID, each app/purpose needs its own password).

## 4. Updater Key Rotation

The updater uses a **Tauri minisign keypair** (separate from Apple signing). The public key is embedded in `tauri.conf.json`; the private key is stored in GitHub Secrets.

### Initial Setup (Phase 0)

A minisign keypair must be generated once (by DevOps or on the release machine):

```bash
yarn tauri signer generate
```

This prompts for a **password** to protect the private key. You must provide this password.

> ⚠️ **Current state:** The updater key has an **empty password** (private key at `~/.zivon/envoy-updater.key`). Before going to production, we recommend setting a password on the key. See the "migrate to a password-protected key" section below.

Output:
- Public key → copy to `tauri.conf.json` under `plugins.updater.pubkey`.
- Private key → save for GitHub Secret entry (do not commit).

### Rotating the Key (when needed)

1. **Generate a new minisign keypair:**
   ```bash
   yarn tauri signer generate --output ~/.zivon/envoy-updater-new.key
   ```

2. **Update `tauri.conf.json`** with the new public key.

3. **Cut a normal release** (not an RC) with the new public key embedded.
   - Clients running the old version (with the old public key) will **not** be able to verify updates signed with the new key.
   - By shipping a normal (non-RC) release first, all clients auto-update to the new version and can then receive updates signed with the new key.

4. **Update GitHub Secrets** `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` with the new private key and its password.

5. **Retire the old private key** (delete from `~/.zivon/` and any notes).

### Migrate to a Password-Protected Key

Currently the private key has an empty password. To add a password:

1. Generate a new keypair **with** a password:
   ```bash
   yarn tauri signer generate --output ~/.zivon/envoy-updater-pw.key
   ```
   When prompted, enter a strong password (e.g., 32-char random string).

2. Proceed with key rotation steps above (update `tauri.conf.json`, cut a release, update secrets).

## 5. Dry-Run Procedure (Release Candidate Test)

Before shipping the first production release, validate the entire pipeline with RC builds.

### Step 1: Build and Install RC1

1. **Tag and push RC1:**
   ```bash
   git tag v0.1.1-rc.1
   git push origin v0.1.1-rc.1
   ```

2. **Monitor the `release.yml` build.** Verify all steps succeed, especially:
   - ✅ Notarization succeeds (`notarytool` logs show success)
   - ✅ S3 upload succeeds
   - ✅ CloudFront invalidation succeeds
   - ✅ Slack notification posted

3. **Download the `.dmg`** from the Slack notification or from `https://downloads.zivon.ai/Envoy_0.1.1-rc.1_universal.dmg`.

4. **Install on a clean Mac** (no development environment, no local repo, no Gatekeeper issues).
   - Double-click the `.dmg` and drag Envoy to Applications.
   - Confirm no Gatekeeper warning (it should open with no friction).
   - Launch Envoy and confirm it runs (assuming Phase 1: it connects to a gateway / localhost).

### Step 2: Test Auto-Update (RC2)

1. **With RC1 still running,** tag and push RC2:
   ```bash
   git tag v0.1.1-rc.2
   git push origin v0.1.1-rc.2
   ```

2. **RC1 should detect the update:**
   - Within ~6 hours, the app's background updater will call `check()` and see RC2 is available.
   - Or, from the tray menu, click "Check for Updates…" to trigger immediately.
   - A native prompt appears: *"Envoy 0.1.1-rc.2 is available. Install?"*

3. **Confirm the prompt** and installation:
   - Click "Install" → the app downloads the new bundle and relaunches as RC2.
   - Confirm the running app now reports version `0.1.1-rc.2` (visible in the About dialog or tray tooltip).

### Step 3: Clean Up RC Tags (optional)

After validation, you may delete the RC tags to keep the git history clean:

```bash
git tag -d v0.1.1-rc.1 v0.1.1-rc.2
git push --delete origin v0.1.1-rc.1 v0.1.1-rc.2
```

Or keep them in history for reference.

---

## References

- **Design spec:** [2026-06-24-zivon-desktop-release-pipeline-design.md](docs/superpowers/specs/2026-06-24-zivon-desktop-release-pipeline-design.md)
- **Release workflow:** [.github/workflows/release.yml](.github/workflows/release.yml)
- **Version script:** `scripts/set-version.mjs`
- **Apple Developer:** https://developer.apple.com
- **Tauri docs (updater):** https://tauri.app/v2/features/updater/
