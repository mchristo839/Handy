//! FlowLite sync: pushes Handy transcripts and the custom dictionary to the FlowLite PocketBase
//! server and pulls records from other devices. Contract: flowlite/docs/sync-api.md.
//!
//! Kept apart from upstream Handy code. Local transcripts are captured from the existing
//! `HistoryUpdatePayload` events, and sync state lives in `flowlite_*` tables inside Handy's
//! `history.db`, created outside Handy's migration list so upstream migrations never collide.

pub mod rules;
mod update;

use anyhow::{anyhow, Result};
use log::{info, warn};
use rules::DictRow;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use specta::Type;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_specta::Event;
use tokio::sync::Notify;

use crate::managers::history::{HistoryEntry, HistoryUpdatePayload};
use crate::settings;

const COLLECTIONS: [&str; 3] = ["transcripts", "dictionary", "settings"];
const DEFAULT_SERVER: &str = "https://dictate.utomat.com";

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct SyncStatus {
    pub server_url: String,
    pub email: String,
    pub signed_in: bool,
    pub message: String,
    pub update_ready: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct SyncedTranscript {
    pub id: String,
    pub text: String,
    pub device: String,
    pub app_name: Option<String>,
    pub model: Option<String>,
    pub transcribe_ms: Option<i64>,
    pub created_at: i64,
}

/// Token and cursors. Stored in a 0600 file next to history.db.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Auth {
    server_url: String,
    email: String,
    token: Option<String>,
    user_id: Option<String>,
    last_refresh: i64,
    cursors: std::collections::HashMap<String, String>,
}

pub struct FlowLiteSync {
    app: AppHandle,
    db_path: PathBuf,
    auth_path: PathBuf,
    auth: Mutex<Auth>,
    message: Mutex<String>,
    wake: Notify,
    run_lock: tokio::sync::Mutex<()>,
    http: reqwest::Client,
    /// Final text, target app and timing of the last dictation, waiting for its history entry.
    pending: Mutex<Option<Pending>>,
    /// Verified .deb waiting for the user to install it.
    update: Mutex<Option<PathBuf>>,
    last_update_check: Mutex<Option<std::time::Instant>>,
}

struct Pending {
    text: String,
    app_name: Option<String>,
    transcribe_ms: i64,
    at: std::time::Instant,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl FlowLiteSync {
    pub fn init(app: &AppHandle) -> Result<Arc<Self>> {
        let dir = crate::portable::app_data_dir(app)?;
        let s = Arc::new(Self {
            app: app.clone(),
            db_path: dir.join("history.db"),
            auth_path: dir.join("flowlite-auth.json"),
            auth: Mutex::new(Auth::default()),
            message: Mutex::new(String::new()),
            wake: Notify::new(),
            run_lock: tokio::sync::Mutex::new(()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
            pending: Mutex::new(None),
            update: Mutex::new(None),
            last_update_check: Mutex::new(None),
        });
        s.create_tables()?;
        let mut auth: Auth = std::fs::read(&s.auth_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if auth.server_url.is_empty() {
            auth.server_url = DEFAULT_SERVER.into();
        }
        *s.message.lock().unwrap() = if auth.token.is_some() {
            "Waiting to sync".into()
        } else {
            "Not signed in".into()
        };
        *s.auth.lock().unwrap() = auth;

        let listener = s.clone();
        HistoryUpdatePayload::listen(app, move |e| {
            if let Err(err) = listener.on_history(&e.payload) {
                warn!("FlowLite: failed to record history change: {err}");
            }
        });

        let runner = s.clone();
        tauri::async_runtime::spawn(async move { runner.run_loop().await });
        Ok(s)
    }

    fn conn(&self) -> Result<Connection> {
        let c = Connection::open(&self.db_path)?;
        c.busy_timeout(Duration::from_secs(5))?;
        Ok(c)
    }

    fn create_tables(&self) -> Result<()> {
        self.conn()?.execute_batch(
            "CREATE TABLE IF NOT EXISTS flowlite_transcripts (
                id TEXT PRIMARY KEY, history_id INTEGER, text TEXT NOT NULL, raw_text TEXT NOT NULL,
                device TEXT NOT NULL, app_name TEXT, engine TEXT, model TEXT, language TEXT,
                duration_ms INTEGER, transcribe_ms INTEGER, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0, dirty INTEGER NOT NULL DEFAULT 1);
             CREATE INDEX IF NOT EXISTS flowlite_transcripts_history ON flowlite_transcripts(history_id);
             CREATE TABLE IF NOT EXISTS flowlite_dictionary (
                id TEXT PRIMARY KEY, word TEXT NOT NULL, replacement TEXT, updated_at INTEGER NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0, dirty INTEGER NOT NULL DEFAULT 1);
             CREATE TABLE IF NOT EXISTS flowlite_settings (
                id TEXT PRIMARY KEY, key TEXT NOT NULL, value_json TEXT NOT NULL, updated_at INTEGER NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0, dirty INTEGER NOT NULL DEFAULT 1);",
        )?;
        Ok(())
    }

    fn save_auth(&self) {
        let auth = self.auth.lock().unwrap().clone();
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt;
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            o.mode(0o600);
            o.open(&self.auth_path)?
                .write_all(&serde_json::to_vec(&auth)?)
        };
        if let Err(e) = write() {
            warn!("FlowLite: could not save auth: {e}");
        }
    }

    // ---- local capture -------------------------------------------------------------------

    fn on_history(&self, payload: &HistoryUpdatePayload) -> Result<()> {
        let c = self.conn()?;
        match payload {
            HistoryUpdatePayload::Added { entry } => {
                if entry.transcription_text.trim().is_empty() {
                    return Ok(());
                }
                let s = settings::get_settings(&self.app);
                let duration_ms = self.wav_duration_ms(entry);
                let created = entry.timestamp * 1000;
                let pending = self
                    .pending
                    .lock()
                    .unwrap()
                    .take()
                    .filter(|p| p.at.elapsed() < Duration::from_secs(30));
                let (text, app_name, transcribe_ms) = match pending {
                    Some(p) => (p.text, p.app_name, Some(p.transcribe_ms)),
                    None => (display_text(entry), None, None),
                };
                c.execute(
                    "INSERT INTO flowlite_transcripts (id, history_id, text, raw_text, device, app_name, engine, model, language,
                        duration_ms, transcribe_ms, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'linux', ?5, 'handy', ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![uuid::Uuid::new_v4().to_string(), entry.id, text, entry.transcription_text, app_name,
                        s.selected_model, s.selected_language, duration_ms, transcribe_ms, created, now_ms()],
                )?;
            }
            HistoryUpdatePayload::Updated { entry } => {
                c.execute(
                    "UPDATE flowlite_transcripts SET text = ?1, raw_text = ?2, updated_at = ?3, dirty = 1 WHERE history_id = ?4",
                    params![display_text(entry), entry.transcription_text, now_ms(), entry.id],
                )?;
            }
            HistoryUpdatePayload::Deleted { id } => {
                c.execute(
                    "UPDATE flowlite_transcripts SET deleted = 1, updated_at = ?1, dirty = 1 WHERE history_id = ?2",
                    params![now_ms(), id],
                )?;
            }
            HistoryUpdatePayload::Toggled { .. } => return Ok(()),
        }
        self.wake.notify_one(); // sync after each dictation
        Ok(())
    }

    fn wav_duration_ms(&self, entry: &HistoryEntry) -> Option<i64> {
        let hm = self
            .app
            .try_state::<Arc<crate::managers::history::HistoryManager>>()?;
        let r = hound::WavReader::open(hm.recordings_dir().join(&entry.file_name)).ok()?;
        Some(r.duration() as i64 * 1000 / r.spec().sample_rate as i64)
    }

    // ---- sync loop -----------------------------------------------------------------------

    async fn run_loop(self: Arc<Self>) {
        let mut failures = 0u32;
        loop {
            let wait = match self.sync_once().await {
                Ok(()) => {
                    failures = 0;
                    Duration::from_secs(300)
                }
                Err(e) => {
                    failures += 1;
                    let secs = (5u64 << (failures - 1).min(6)).min(300);
                    if self.auth.lock().unwrap().token.is_some() {
                        warn!("FlowLite sync failed ({failures}), retry in {secs}s: {e}");
                        self.set_message(format!("Offline, retrying in {secs}s"));
                    }
                    Duration::from_secs(secs)
                }
            };
            let _ = tokio::time::timeout(wait, self.wake.notified()).await;
        }
    }

    fn set_message(&self, m: String) {
        *self.message.lock().unwrap() = m;
        let _ = self.app.emit("flowlite-sync-status", ());
    }

    pub fn status(&self) -> SyncStatus {
        let a = self.auth.lock().unwrap();
        SyncStatus {
            server_url: a.server_url.clone(),
            email: a.email.clone(),
            signed_in: a.token.is_some(),
            message: self.message.lock().unwrap().clone(),
            update_ready: self.update_ready(),
        }
    }

    pub async fn sync_once(&self) -> Result<()> {
        let _guard = self.run_lock.lock().await;
        if let Err(e) = self.purge_old() {
            warn!("FlowLite: history cleanup failed: {e}");
        }
        let (base, token, owner) = {
            let a = self.auth.lock().unwrap();
            match (&a.token, &a.user_id) {
                (Some(t), Some(u)) => (a.server_url.clone(), t.clone(), u.clone()),
                _ => return Ok(()),
            }
        };
        let result = async {
            self.refresh_token(&base, &token).await?;
            let token = self.auth.lock().unwrap().token.clone().unwrap_or(token);
            self.record_local_changes()?;
            self.push(&base, &token, &owner).await?;
            self.pull(&base, &token).await?;
            if self.purge_old()? {
                self.push(&base, &token, &owner).await?; // old rows pulled from other devices
            }
            self.apply_to_handy()
        }
        .await;
        match &result {
            Ok(()) => {
                self.set_message(format!("Synced {}", chrono::Local::now().format("%H:%M")));
                let token = self.auth.lock().unwrap().token.clone();
                if let Some(token) = token {
                    self.check_update(&base, &token).await;
                }
            }
            Err(e) if e.to_string().contains("Sign in again") => {
                self.auth.lock().unwrap().token = None;
                self.save_auth();
                self.set_message("Sign in again".into());
                return Ok(());
            }
            Err(_) => {}
        }
        result
    }

    async fn refresh_token(&self, base: &str, token: &str) -> Result<()> {
        if now_ms() - self.auth.lock().unwrap().last_refresh < 86_400_000 {
            return Ok(());
        }
        let r = self
            .http
            .post(format!("{base}/api/collections/users/auth-refresh"))
            .header("Authorization", token)
            .send()
            .await?;
        check_auth(r.status())?;
        if r.status().is_success() {
            let v: Value = r.json().await?;
            let mut a = self.auth.lock().unwrap();
            a.token = v["token"].as_str().map(String::from).or(a.token.take());
            a.last_refresh = now_ms();
            drop(a);
            self.save_auth();
        }
        Ok(())
    }

    /// Compares Handy's own settings (custom words, language, filler removal) with the synced
    /// rows and queues whatever changed locally since the last sync.
    fn record_local_changes(&self) -> Result<()> {
        let s = settings::get_settings(&self.app);
        let c = self.conn()?;
        for r in rules::local_dictionary_changes(&s.custom_words, &self.dict_rows(&c)?, now_ms()) {
            c.execute(
                "INSERT INTO flowlite_dictionary (id, word, replacement, updated_at, deleted, dirty) VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT(id) DO UPDATE SET word = ?2, replacement = ?3, updated_at = ?4, deleted = ?5, dirty = 1",
                params![r.id, r.word, r.replacement, r.updated_at, r.deleted],
            )?;
        }
        for (key, local) in [
            ("language", json!(s.selected_language)),
            ("remove_filler_words", json!(s.filler_word_removal_enabled)),
        ] {
            let id = rules::setting_id(key);
            let stored: Option<String> = c
                .query_row(
                    "SELECT value_json FROM flowlite_settings WHERE id = ?1",
                    [&id],
                    |r| r.get(0),
                )
                .optional()?;
            if stored
                .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                .as_ref()
                != Some(&local)
            {
                c.execute(
                    "INSERT INTO flowlite_settings (id, key, value_json, updated_at, deleted, dirty) VALUES (?1, ?2, ?3, ?4, 0, 1)
                     ON CONFLICT(id) DO UPDATE SET value_json = ?3, updated_at = ?4, deleted = 0, dirty = 1",
                    params![id, key, local.to_string(), now_ms()],
                )?;
            }
        }
        Ok(())
    }

    /// Applies the synced "history_days" setting: soft-deletes old rows and blanks their text.
    fn purge_old(&self) -> Result<bool> {
        let c = self.conn()?;
        let days: Option<String> = c
            .query_row(
                "SELECT value_json FROM flowlite_settings WHERE id = ?1 AND deleted = 0",
                [rules::setting_id("history_days")],
                |r| r.get(0),
            )
            .optional()?;
        let Some(cutoff) = rules::retention_cutoff(
            days.and_then(|v| v.trim().parse::<f64>().ok())
                .map(|d| d as i64),
            now_ms(),
        ) else {
            return Ok(false);
        };
        let n = c.execute(
            "UPDATE flowlite_transcripts SET deleted = 1, text = '', raw_text = '', dirty = 1, updated_at = ?1
             WHERE deleted = 0 AND created_at < ?2",
            params![now_ms(), cutoff],
        )?;
        Ok(n > 0)
    }

    fn dict_rows(&self, c: &Connection) -> Result<Vec<DictRow>> {
        let mut st = c.prepare(
            "SELECT id, word, replacement, updated_at, deleted FROM flowlite_dictionary",
        )?;
        let rows = st.query_map([], |r| {
            Ok(DictRow {
                id: r.get(0)?,
                word: r.get(1)?,
                replacement: r.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty()),
                updated_at: r.get(3)?,
                deleted: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Writes pulled dictionary words and shared settings back into Handy's settings store.
    fn apply_to_handy(&self) -> Result<()> {
        let c = self.conn()?;
        let mut s = settings::get_settings(&self.app);
        let mut changed = false;
        let words = rules::words_from_rows(&s.custom_words, &self.dict_rows(&c)?);
        if words != s.custom_words {
            s.custom_words = words;
            changed = true;
        }
        let get = |key: &str| -> Result<Option<Value>> {
            let v: Option<String> = c
                .query_row(
                    "SELECT value_json FROM flowlite_settings WHERE id = ?1 AND deleted = 0",
                    [rules::setting_id(key)],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(v.and_then(|v| serde_json::from_str(&v).ok()))
        };
        if let Some(Value::String(lang)) = get("language")? {
            if lang != s.selected_language {
                s.selected_language = lang;
                changed = true;
            }
        }
        if let Some(Value::Bool(b)) = get("remove_filler_words")? {
            if b != s.filler_word_removal_enabled {
                s.filler_word_removal_enabled = b;
                changed = true;
            }
        }
        if changed {
            settings::write_settings(&self.app, s);
            let _ = self
                .app
                .emit("settings-changed", json!({ "setting": "flowlite_sync" }));
        }
        Ok(())
    }

    async fn push(&self, base: &str, token: &str, owner: &str) -> Result<()> {
        let dirty: Vec<(String, &str, Value, i64)> = {
            let c = self.conn()?;
            let mut out = Vec::new();
            let mut st = c.prepare(
                "SELECT id, text, raw_text, device, app_name, engine, model, language, duration_ms, transcribe_ms, created_at, updated_at, deleted
                 FROM flowlite_transcripts WHERE dirty = 1",
            )?;
            for r in st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, json!({
                    "text": r.get::<_, String>(1)?, "raw_text": r.get::<_, String>(2)?, "device": r.get::<_, String>(3)?,
                    "app_name": r.get::<_, Option<String>>(4)?.unwrap_or_default(), "engine": r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    "model": r.get::<_, Option<String>>(6)?.unwrap_or_default(), "language": r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                    "duration_ms": r.get::<_, Option<i64>>(8)?, "transcribe_ms": r.get::<_, Option<i64>>(9)?,
                    "created_at": r.get::<_, i64>(10)?, "updated_at": r.get::<_, i64>(11)?, "deleted": r.get::<_, bool>(12)?,
                }), r.get::<_, i64>(11)?))
            })? {
                let (id, body, at) = r?;
                out.push((id, "transcripts", body, at));
            }
            let mut st = c.prepare("SELECT id, word, replacement, updated_at, deleted FROM flowlite_dictionary WHERE dirty = 1")?;
            for r in st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, json!({
                    "word": r.get::<_, String>(1)?, "replacement": r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    "updated_at": r.get::<_, i64>(3)?, "deleted": r.get::<_, bool>(4)?,
                }), r.get::<_, i64>(3)?))
            })? {
                let (id, body, at) = r?;
                out.push((id, "dictionary", body, at));
            }
            let mut st = c.prepare("SELECT id, key, value_json, updated_at, deleted FROM flowlite_settings WHERE dirty = 1")?;
            for r in st.query_map([], |r| {
                let v: String = r.get(2)?;
                Ok((r.get::<_, String>(0)?, json!({
                    "key": r.get::<_, String>(1)?, "value_json": serde_json::from_str::<Value>(&v).unwrap_or(Value::Null),
                    "updated_at": r.get::<_, i64>(3)?, "deleted": r.get::<_, bool>(4)?,
                }), r.get::<_, i64>(3)?))
            })? {
                let (id, body, at) = r?;
                out.push((id, "settings", body, at));
            }
            out
        };

        for (id, col, mut body, updated_at) in dirty {
            body["owner"] = json!(owner);
            let url = format!("{base}/api/collections/{col}/records");
            let mut r = self
                .http
                .patch(format!("{url}/{id}"))
                .header("Authorization", token)
                .json(&body)
                .send()
                .await?;
            if r.status().as_u16() == 404 {
                body["id"] = json!(id);
                r = self
                    .http
                    .post(&url)
                    .header("Authorization", token)
                    .json(&body)
                    .send()
                    .await?;
            }
            let code = r.status().as_u16();
            check_auth(r.status())?;
            match code {
                200 | 409 => {}
                400 => warn!(
                    "FlowLite: server rejected {col}/{id}: {}",
                    r.text().await.unwrap_or_default()
                ),
                _ => return Err(anyhow!("push {col} failed: HTTP {code}")),
            }
            // Only clear dirty if the row did not change while we were pushing.
            self.conn()?.execute(
                &format!("UPDATE flowlite_{col} SET dirty = 0 WHERE id = ?1 AND updated_at = ?2"),
                params![id, updated_at],
            )?;
        }
        Ok(())
    }

    async fn pull(&self, base: &str, token: &str) -> Result<()> {
        for col in COLLECTIONS {
            let mut cursor = self
                .auth
                .lock()
                .unwrap()
                .cursors
                .get(col)
                .cloned()
                .unwrap_or_default();
            let from = rules::pull_from(&cursor);
            let mut page = 1;
            loop {
                let r = self
                    .http
                    .get(format!("{base}/api/collections/{col}/records"))
                    .query(&[
                        ("filter", format!("(synced_at>=\"{from}\")")),
                        ("sort", "synced_at,id".into()),
                        ("perPage", "500".into()),
                        ("page", page.to_string()),
                        ("skipTotal", "1".into()),
                    ])
                    .header("Authorization", token)
                    .send()
                    .await?;
                check_auth(r.status())?;
                if !r.status().is_success() {
                    return Err(anyhow!("pull {col} failed: HTTP {}", r.status()));
                }
                let v: Value = r.json().await?;
                let items = v["items"].as_array().cloned().unwrap_or_default();
                let c = self.conn()?;
                for it in &items {
                    self.apply_remote(&c, col, it)?;
                    if let Some(s) = it["synced_at"].as_str() {
                        if s > cursor.as_str() {
                            cursor = s.to_string();
                        }
                    }
                }
                if items.len() < 500 {
                    break;
                }
                page += 1;
            }
            self.auth
                .lock()
                .unwrap()
                .cursors
                .insert(col.to_string(), cursor);
            self.save_auth();
        }
        Ok(())
    }

    fn apply_remote(&self, c: &Connection, col: &str, it: &Value) -> Result<()> {
        let id = it["id"]
            .as_str()
            .ok_or_else(|| anyhow!("record without id"))?;
        let remote_at = it["updated_at"].as_i64().unwrap_or(0);
        let local_at: Option<i64> = c
            .query_row(
                &format!("SELECT updated_at FROM flowlite_{col} WHERE id = ?1"),
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if !rules::remote_wins(local_at, remote_at) {
            return Ok(());
        }
        let s = |k: &str| it[k].as_str().unwrap_or_default().to_string();
        let deleted = it["deleted"].as_bool().unwrap_or(false);
        match col {
            "transcripts" => c.execute(
                "INSERT INTO flowlite_transcripts (id, text, raw_text, device, app_name, engine, model, language, duration_ms,
                    transcribe_ms, created_at, updated_at, deleted, dirty) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,0)
                 ON CONFLICT(id) DO UPDATE SET text=?2, raw_text=?3, app_name=?5, engine=?6, model=?7, language=?8,
                    duration_ms=?9, transcribe_ms=?10, updated_at=?12, deleted=?13, dirty=0",
                params![id, s("text"), s("raw_text"), s("device"), s("app_name"), s("engine"), s("model"), s("language"),
                    it["duration_ms"].as_i64(), it["transcribe_ms"].as_i64(), it["created_at"].as_i64().unwrap_or(0), remote_at, deleted],
            )?,
            "dictionary" => c.execute(
                "INSERT INTO flowlite_dictionary (id, word, replacement, updated_at, deleted, dirty) VALUES (?1,?2,?3,?4,?5,0)
                 ON CONFLICT(id) DO UPDATE SET word=?2, replacement=?3, updated_at=?4, deleted=?5, dirty=0",
                params![id, s("word"), Some(s("replacement")).filter(|r| !r.is_empty()), remote_at, deleted],
            )?,
            _ => c.execute(
                "INSERT INTO flowlite_settings (id, key, value_json, updated_at, deleted, dirty) VALUES (?1,?2,?3,?4,?5,0)
                 ON CONFLICT(id) DO UPDATE SET value_json=?3, updated_at=?4, deleted=?5, dirty=0",
                params![id, s("key"), it["value_json"].to_string(), remote_at, deleted],
            )?,
        };
        Ok(())
    }

    // ---- commands ------------------------------------------------------------------------

    pub async fn login(&self, server_url: String, email: String, password: String) -> Result<()> {
        let base = server_url.trim().trim_end_matches('/').to_string();
        let local = ["http://localhost", "http://127.0.0.1"].iter().any(|p| base.starts_with(p));
        if !base.starts_with("https://") && !local {
            return Err(anyhow!("Server URL must start with https://"));
        }
        let r = self
            .http
            .post(format!("{base}/api/collections/users/auth-with-password"))
            .json(&json!({ "identity": email.trim(), "password": password }))
            .send()
            .await?;
        if !r.status().is_success() {
            let v: Value = r.json().await.unwrap_or_default();
            return Err(anyhow!(
                "{}",
                v["message"].as_str().unwrap_or("Sign in failed")
            ));
        }
        let v: Value = r.json().await?;
        {
            let mut a = self.auth.lock().unwrap();
            if a.server_url != base || a.email != email.trim() {
                a.cursors.clear();
            }
            a.server_url = base;
            a.email = email.trim().to_string();
            a.token = v["token"].as_str().map(String::from);
            a.user_id = v["record"]["id"].as_str().map(String::from);
            a.last_refresh = now_ms();
        }
        self.save_auth();
        info!("FlowLite: signed in");
        self.set_message("Signed in".into());
        self.wake.notify_one();
        Ok(())
    }

    pub fn logout(&self) {
        let mut a = self.auth.lock().unwrap();
        a.token = None;
        a.user_id = None;
        drop(a);
        self.save_auth();
        self.set_message("Not signed in".into());
    }

    pub fn list(&self, query: &str) -> Result<Vec<SyncedTranscript>> {
        let c = self.conn()?;
        let mut st = c.prepare(
            "SELECT id, text, device, app_name, model, transcribe_ms, created_at FROM flowlite_transcripts
             WHERE deleted = 0 AND text LIKE '%' || ?1 || '%' ORDER BY created_at DESC LIMIT 500",
        )?;
        let rows = st.query_map([query], |r| {
            Ok(SyncedTranscript {
                id: r.get(0)?,
                text: r.get(1)?,
                device: r.get(2)?,
                app_name: r.get::<_, Option<String>>(3)?.filter(|s| !s.is_empty()),
                model: r.get(4)?,
                transcribe_ms: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Called from Handy's transcription flow (actions.rs) before the text is saved and pasted.
/// Applies synced dictionary replacements and remembers the target app and timing for the history row.
pub(crate) fn on_transcribed(
    app: &AppHandle,
    mut processed: crate::actions::ProcessedTranscription,
    elapsed: Duration,
) -> crate::actions::ProcessedTranscription {
    let Some(sync) = app.try_state::<Arc<FlowLiteSync>>() else {
        return processed;
    };
    match sync.conn().and_then(|c| sync.dict_rows(&c)) {
        Ok(rows) => processed.final_text = rules::apply_replacements(&processed.final_text, &rows),
        Err(e) => warn!("FlowLite: dictionary replacements skipped: {e}"),
    }
    *sync.pending.lock().unwrap() = Some(Pending {
        text: processed.final_text.clone(),
        app_name: active_window_class(),
        transcribe_ms: elapsed.as_millis() as i64,
        at: std::time::Instant::now(),
    });
    processed
}

/// Window class of the focused X11 window, e.g. "firefox". None on Wayland or without xdotool.
/// Process name of the focused window. Older xdotool (Ubuntu 24.04) has no getwindowclassname.
fn active_window_class() -> Option<String> {
    let out = std::process::Command::new("xdotool")
        .args(["getactivewindow", "getwindowpid"])
        .output()
        .ok()?;
    let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(name.trim().to_string()).filter(|n| !n.is_empty())
}

fn display_text(e: &HistoryEntry) -> String {
    e.post_processed_text
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| e.transcription_text.clone())
}

fn check_auth(code: reqwest::StatusCode) -> Result<()> {
    if code.as_u16() == 401 || code.as_u16() == 403 {
        return Err(anyhow!("Sign in again"));
    }
    Ok(())
}

type State<'a> = tauri::State<'a, Arc<FlowLiteSync>>;

#[tauri::command]
#[specta::specta]
pub fn flowlite_status(sync: State<'_>) -> SyncStatus {
    sync.status()
}

#[tauri::command]
#[specta::specta]
pub async fn flowlite_login(
    sync: State<'_>,
    server_url: String,
    email: String,
    password: String,
) -> Result<SyncStatus, String> {
    sync.login(server_url, email, password)
        .await
        .map_err(|e| e.to_string())?;
    Ok(sync.status())
}

#[tauri::command]
#[specta::specta]
pub fn flowlite_logout(sync: State<'_>) -> SyncStatus {
    sync.logout();
    sync.status()
}

#[tauri::command]
#[specta::specta]
pub async fn flowlite_sync_now(sync: State<'_>) -> Result<SyncStatus, String> {
    if let Err(e) = sync.sync_once().await {
        sync.set_message(format!("Sync failed: {e}"));
    }
    Ok(sync.status())
}

#[tauri::command]
#[specta::specta]
pub fn flowlite_transcripts(
    sync: State<'_>,
    query: String,
) -> Result<Vec<SyncedTranscript>, String> {
    sync.list(&query).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn flowlite_install_update(sync: State<'_>) -> Result<(), String> {
    sync.install_update().map_err(|e| e.to_string())
}
