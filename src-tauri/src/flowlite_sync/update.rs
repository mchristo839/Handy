//! Self-update from the public GitHub Releases page. Each release carries flowlite-linux.json
//! (version_code, sha256, signature, tag, deb) next to the .deb. The signature is minisign, from the
//! release key held only in the release build's secrets, over "flowlite-linux:<version>:<sha256>".
//! A tampered download can neither be installed nor relabel an old signed release as newer.
//! Works without signing in to a sync server. Installed with `pkexec dpkg -i`.

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
                if fresh {
                    let _ = std::process::Command::new("notify-send")
                        .args([
                            "FlowLite update ready",
                            "Open FlowLite, Sync page, Install update",
                        ])
                        .spawn();
                }
            }
            Ok(None) => {}
            Err(e) => warn!("FlowLite: update check failed: {e}"),
        }
    }

    async fn download_update(&self) -> Result<Option<PathBuf>> {
        let resp = self
            .http
            .get(format!("{RELEASES}/latest/download/flowlite-linux.json"))
            .header("User-Agent", "FlowLite-Linux")
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
        let deb = dir.join(format!("flowlite-{version}.deb"));
        let sig = rel["signature"].as_str().unwrap_or_default();
        verify(manifest(version, sha).as_bytes(), sig)?;
        if std::fs::read(&deb).is_ok_and(|b| hex(&Sha256::digest(&b)) == sha) {
            return Ok(Some(deb));
        }
        for old in std::fs::read_dir(&dir)?.flatten() {
            let _ = std::fs::remove_file(old.path());
        }

        let safe = |v: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        let tag = rel["tag"].as_str().unwrap_or_default();
        let name = rel["deb"].as_str().unwrap_or_default();
        if !safe(tag) || !safe(name) || !name.ends_with(".deb") {
            return Err(anyhow!("update manifest is malformed"));
        }
        let url = format!("{RELEASES}/download/{tag}/{name}");
        let bytes = self
            .http
            .get(url)
            .header("User-Agent", "FlowLite-Linux")
            .timeout(Duration::from_secs(600))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        if hex(&Sha256::digest(&bytes)) != sha {
            return Err(anyhow!("update checksum mismatch"));
        }
        std::fs::write(&deb, &bytes)?;
        info!("FlowLite: update {version} downloaded");
        Ok(Some(deb))
    }

    pub fn update_ready(&self) -> bool {
        self.update.lock().unwrap().is_some()
    }

    /// Installs the downloaded update and restarts. pkexec shows the system password dialog.
    // ponytail: dpkg -i does not pull new dependencies; if a release adds one, install that release with apt by hand.
    pub fn install_update(&self) -> Result<()> {
        let deb = self
            .update
            .lock()
            .unwrap()
            .clone()
            .ok_or(anyhow!("No update downloaded"))?;
        let out = std::process::Command::new("pkexec")
            .arg("dpkg")
            .arg("-i")
            .arg(&deb)
            .output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "Install cancelled or failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let _ = std::fs::remove_file(&deb);
        self.app.restart()
    }
}

/// What the release build signs. Binding the version stops a replayed old release from passing as new.
fn manifest(version: u64, sha256: &str) -> String {
    format!("flowlite-linux:{version}:{sha256}")
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
