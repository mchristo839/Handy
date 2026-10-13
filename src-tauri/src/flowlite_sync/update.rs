//! Self-update from the FlowLite server's linux_releases collection (uploaded by flowlite/linux/publish.sh).
//! The .deb must carry a minisign signature from the release key, which never leaves the publishing
//! machine, so a compromised server cannot push its own package. Installed with `pkexec dpkg -i`.

use anyhow::{anyhow, Result};
use log::{info, warn};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

use super::FlowLiteSync;

const CHECK_EVERY: Duration = Duration::from_secs(3600);
/// Public half of flowlite/keys/release.key, base64 as written by `tauri signer generate`.
const RELEASE_PUBKEY: &str = include_str!("release.pub");

/// Build number baked in by publish.sh. Local builds are 0, so any published release replaces them.
pub fn build() -> u64 {
    option_env!("FLOWLITE_BUILD")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

impl FlowLiteSync {
    /// Called after each successful sync. Checks at most hourly; never fails the sync.
    pub(super) async fn check_update(&self, base: &str, token: &str) {
        {
            let mut last = self.last_update_check.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < CHECK_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        match self.download_update(base, token).await {
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

    async fn download_update(&self, base: &str, token: &str) -> Result<Option<PathBuf>> {
        let v: Value = self
            .http
            .get(format!("{base}/api/collections/linux_releases/records?sort=-version_code&perPage=1&skipTotal=1"))
            .header("Authorization", token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let Some(rel) = v["items"].get(0) else {
            return Ok(None);
        };
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
        if std::fs::read(&deb).is_ok_and(|b| hex(&Sha256::digest(&b)) == sha && verify(&b, sig).is_ok()) {
            return Ok(Some(deb));
        }
        for old in std::fs::read_dir(&dir)?.flatten() {
            let _ = std::fs::remove_file(old.path());
        }

        let ft: Value = self
            .http
            .post(format!("{base}/api/files/token"))
            .header("Authorization", token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let url = reqwest::Url::parse_with_params(
            &format!(
                "{base}/api/files/linux_releases/{}/{}",
                rel["id"].as_str().unwrap_or_default(),
                rel["deb"].as_str().unwrap_or_default()
            ),
            &[("token", ft["token"].as_str().unwrap_or_default())],
        )?;
        let bytes = self
            .http
            .get(url)
            .timeout(Duration::from_secs(600))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        if hex(&Sha256::digest(&bytes)) != sha {
            return Err(anyhow!("update checksum mismatch"));
        }
        verify(&bytes, sig)?;
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
