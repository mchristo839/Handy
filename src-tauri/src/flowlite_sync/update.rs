//! Self-update from the public GitHub Releases page. Each release carries flowlite-<os>.json
//! (version_code, sha256, signature, tag, file) next to the installer for that system. The signature
//! is minisign, from the release key held only in the release build's secrets, over
//! "flowlite-<os>:<version>:<sha256>". A tampered download can neither be installed nor relabel an
//! old signed release as newer. Works without signing in to a sync server.
//! Linux installs with `pkexec dpkg -i`. Windows runs the setup program. macOS opens the disk image.

use anyhow::{anyhow, Result};
use log::{info, warn};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

use super::FlowLiteSync;

const CHECK_EVERY: Duration = Duration::from_secs(3600);
const RELEASES: &str = "https://github.com/mchristo839/flowlite/releases";
/// Public half of flowlite/keys/release.key, base64 as written by `tauri signer generate`.
const RELEASE_PUBKEY: &str = include_str!("release.pub");

#[cfg(target_os = "windows")]
const OS: &str = "windows";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const OS: &str = "macos";
#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
const OS: &str = "macos-x64"; // Intel Macs have their own build and update file
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const OS: &str = "linux";

/// Installer type each system accepts.
#[cfg(target_os = "windows")]
const EXT: &str = ".exe";
#[cfg(target_os = "macos")]
const EXT: &str = ".dmg";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const EXT: &str = ".deb";

/// Build number baked in by the release build. Local builds are 0, so any published release replaces them.
pub fn build() -> u64 {
    option_env!("FLOWLITE_BUILD")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

impl FlowLiteSync {
    /// Called from the sync loop. Checks at most hourly; never fails the sync.
    pub(super) async fn check_update(&self) {
        {
            let mut last = self.last_update_check.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < CHECK_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        match self.download_update().await {
            Ok(Some(path)) => {
                let fresh = self.update.lock().unwrap().replace(path).is_none();
                let _ = self.app.emit("flowlite-sync-status", ());
                #[cfg(target_os = "linux")]
                if fresh {
                    let _ = std::process::Command::new("notify-send")
                        .args([
                            "FlowLite update ready",
                            "Open FlowLite, Sync page, Install update",
                        ])
                        .spawn();
                }
                #[cfg(not(target_os = "linux"))]
                let _ = fresh;
            }
            Ok(None) => {}
            Err(e) => warn!("FlowLite: update check failed: {e}"),
        }
    }

    async fn download_update(&self) -> Result<Option<PathBuf>> {
        let resp = self
            .http
            .get(format!("{RELEASES}/latest/download/flowlite-{OS}.json"))
            .header("User-Agent", "FlowLite-Desktop")
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None); // no release published yet
        }
        let rel: Value = resp.error_for_status()?.json().await?;
        let version = rel["version_code"].as_u64().unwrap_or(0);
        if version <= build() {
            return Ok(None);
        }
        let sha = rel["sha256"]
            .as_str()
            .ok_or(anyhow!("release has no checksum"))?;
        let dir = self.app.path().app_cache_dir()?.join("updates");
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("flowlite-{version}{EXT}"));
        let sig = rel["signature"].as_str().unwrap_or_default();
        verify(manifest(version, sha).as_bytes(), sig)?;
        if std::fs::read(&file).is_ok_and(|b| hex(&Sha256::digest(&b)) == sha) {
            return Ok(Some(file));
        }
        for old in std::fs::read_dir(&dir)?.flatten() {
            let _ = std::fs::remove_file(old.path());
        }

        let safe = |v: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        let tag = rel["tag"].as_str().unwrap_or_default();
        // Linux manifests from 0.2.x call the field "deb".
        let name = rel["file"].as_str().or(rel["deb"].as_str()).unwrap_or_default();
        if !safe(tag) || !safe(name) || !name.ends_with(EXT) {
            return Err(anyhow!("update manifest is malformed"));
        }
        let url = format!("{RELEASES}/download/{tag}/{name}");
        let bytes = self
            .http
            .get(url)
            .header("User-Agent", "FlowLite-Desktop")
            .timeout(Duration::from_secs(600))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        if hex(&Sha256::digest(&bytes)) != sha {
            return Err(anyhow!("update checksum mismatch"));
        }
        std::fs::write(&file, &bytes)?;
        info!("FlowLite: update {version} downloaded");
        Ok(Some(file))
    }

    pub fn update_ready(&self) -> bool {
        self.update.lock().unwrap().is_some()
    }

    /// Installs the downloaded update. Linux: pkexec shows the system password dialog, then
    /// FlowLite restarts. Windows: the setup program takes over and FlowLite quits so it can
    /// replace the files. macOS: the disk image opens for the usual drag to Applications.
    // ponytail: dpkg -i does not pull new dependencies; if a release adds one, install that release with apt by hand.
    pub fn install_update(&self) -> Result<()> {
        let file = self
            .update
            .lock()
            .unwrap()
            .clone()
            .ok_or(anyhow!("No update downloaded"))?;
        #[cfg(target_os = "linux")]
        {
            let out = std::process::Command::new("pkexec")
                .arg("dpkg")
                .arg("-i")
                .arg(&file)
                .output()?;
            if !out.status.success() {
                return Err(anyhow!(
                    "Install cancelled or failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let _ = std::fs::remove_file(&file);
            self.app.restart()
        }
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new(&file).spawn()?;
            self.app.exit(0);
            Ok(())
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(&file).spawn()?;
            Ok(())
        }
    }
}

/// What the release build signs. Binding the version stops a replayed old release from passing as new.
fn manifest(version: u64, sha256: &str) -> String {
    format!("flowlite-{OS}:{version}:{sha256}")
}

fn verify(data: &[u8], signature_b64: &str) -> Result<()> {
    use base64::Engine;
    let text = |b64: &str| -> Result<String> {
        Ok(String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64.trim())?)?)
    };
    let key = minisign_verify::PublicKey::decode(&text(RELEASE_PUBKEY)?)?;
    let sig = minisign_verify::Signature::decode(&text(signature_b64)?)?;
    key.verify(data, &sig, true)
        .map_err(|e| anyhow!("update signature invalid: {e}"))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_unsigned_and_garbage() {
        assert!(super::verify(b"deb", "").is_err());
        assert!(super::verify(b"deb", "bm90IGEgc2lnbmF0dXJl").is_err());
    }

    #[test]
    fn accepts_release_key_signature_only_for_signed_bytes() {
        let sig = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVSV3F6ZFhFTGpuUDVsQ1RhRGtFK3N3YTc5MmNKOEtudDRjS1hQMkttYlJwZHlLZTRwZHVZVFNOUk9idzFkTTUvL0srZm4rYlN3TXAyTmVicnRlTWd3SWlLZlM4Z2ZuR3dFPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkxODc5ODIzCWZpbGU6c2FtcGxlLmJpbgpKZllhcnY0T3I4QnF3Zllhck9TVE1KR3VvZklwYkN3V3RQU3lvVzdXbzJoQ09lczIzUkhtUklrMk9FZFhnc1lpM0VXZFRGdFBtTm5lOUQ3QlZ3cnVDdz09Cg==";
        assert!(super::verify(b"flowlite signed sample", sig).is_ok());
        assert!(super::verify(b"flowlite signed sample!", sig).is_err());
    }
}
