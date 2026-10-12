//! Pure sync rules shared with the Android client (see flowlite/docs/sync-api.md).
//! No Tauri types here so the logic is unit tested in isolation.

use std::collections::HashSet;

/// Last write wins. A tie keeps the local copy (identical or already queued).
pub fn remote_wins(local_updated_at: Option<i64>, remote_updated_at: i64) -> bool {
    local_updated_at.map_or(true, |l| remote_updated_at > l)
}

pub fn dictionary_id(word: &str) -> String {
    uuid5(&format!(
        "flowlite:dictionary:{}",
        word.trim().to_lowercase()
    ))
}

pub fn setting_id(key: &str) -> String {
    uuid5(&format!("flowlite:settings:{key}"))
}

fn uuid5(name: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, name.as_bytes()).to_string()
}

/// Pull filter value: the stored cursor minus a 2 second overlap, or "" for a full pull.
pub fn pull_from(cursor: &str) -> String {
    const FMT: &str = "%Y-%m-%d %H:%M:%S%.3fZ";
    chrono::NaiveDateTime::parse_from_str(cursor, FMT)
        .map(|t| (t - chrono::Duration::seconds(2)).format(FMT).to_string())
        .unwrap_or_default()
}

/// A dictionary row as stored locally.
#[derive(Clone, Debug, PartialEq)]
pub struct DictRow {
    pub id: String,
    pub word: String,
    pub replacement: Option<String>,
    pub updated_at: i64,
    pub deleted: bool,
}

/// Handy's `custom_words` is the editable store on Linux. Compares it with the synced rows and
/// returns the rows that changed locally (added, revived or removed), stamped with `now`.
/// Rows with a replacement are not part of `custom_words` and are left alone.
pub fn local_dictionary_changes(
    custom_words: &[String],
    rows: &[DictRow],
    now: i64,
) -> Vec<DictRow> {
    let mut changes = Vec::new();
    let mut seen = HashSet::new();
    for w in custom_words {
        let id = dictionary_id(w);
        if !seen.insert(id.clone()) {
            continue;
        }
        match rows.iter().find(|r| r.id == id) {
            Some(r) if !r.deleted => {}
            _ => changes.push(DictRow {
                id,
                word: w.trim().to_string(),
                replacement: None,
                updated_at: now,
                deleted: false,
            }),
        }
    }
    for r in rows {
        if !r.deleted && r.replacement.is_none() && !seen.contains(&r.id) {
            changes.push(DictRow {
                updated_at: now,
                deleted: true,
                ..r.clone()
            });
        }
    }
    changes
}

/// Rebuilds `custom_words` from the synced rows, keeping the user's order for words that stay.
pub fn words_from_rows(custom_words: &[String], rows: &[DictRow]) -> Vec<String> {
    let active: Vec<&DictRow> = rows
        .iter()
        .filter(|r| !r.deleted && r.replacement.is_none())
        .collect();
    let active_ids: HashSet<&str> = active.iter().map(|r| r.id.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    let mut kept = HashSet::new();
    for w in custom_words {
        let id = dictionary_id(w);
        if active_ids.contains(id.as_str()) && kept.insert(id) {
            out.push(w.clone());
        }
    }
    for r in active {
        if kept.insert(r.id.clone()) {
            out.push(r.word.clone());
        }
    }
    out
}

/// Whole-word, case-insensitive dictionary replacements, longest word first.
/// Same rule as Android's `Cleanup.applyDictionary`, so both devices produce the same text.
/// Longest spoken trigger and expansion accepted, as in Wispr Flow. Rows pulled from the server
/// are untrusted, so longer ones are ignored instead of being pasted into the focused window.
pub const MAX_WORD: usize = 60;
pub const MAX_REPLACEMENT: usize = 4000;

/// Drops control characters other than newline and tab, so a replacement cannot send key codes.
pub fn clean_replacement(r: &str) -> String {
    r.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

pub fn apply_replacements(text: &str, rows: &[DictRow]) -> String {
    let mut active: Vec<&DictRow> = rows
        .iter()
        .filter(|r| {
            !r.deleted && !r.word.trim().is_empty() && r.word.trim().chars().count() <= MAX_WORD
        })
        .filter(|r| {
            r.replacement
                .as_ref()
                .map_or(true, |x| x.chars().count() <= MAX_REPLACEMENT)
        })
        .collect();
    if active.is_empty() {
        return text.to_string();
    }
    // Longest first so the longest phrase wins at a position.
    active.sort_by_key(|r| std::cmp::Reverse(r.word.trim().chars().count()));
    let targets: std::collections::HashMap<String, String> = active
        .iter()
        .map(|r| {
            let word = r.word.trim();
            let target = r
                .replacement
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(word);
            (word.to_lowercase(), clean_replacement(target))
        })
        .collect();
    let pattern = active
        .iter()
        .map(|r| regex::escape(r.word.trim()))
        .collect::<Vec<_>>()
        .join("|");
    let Ok(re) = regex::Regex::new(&format!("(?i)(?:{pattern})")) else {
        return text.to_string();
    };
    // One pass over the original text: inserted text is never expanded again.
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in re.find_iter(text) {
        let inside_word = text[..m.start()]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
            || text[m.end()..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric);
        if let (false, Some(target)) = (inside_word, targets.get(&m.as_str().to_lowercase())) {
            out.push_str(&text[last..m.start()]);
            out.push_str(target);
            last = m.end();
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Cutoff for the synced "history_days" setting. None keeps everything.
pub fn retention_cutoff(days: Option<i64>, now: i64) -> Option<i64> {
    days.filter(|d| *d > 0).map(|d| now - d * 86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(word: &str, at: i64, deleted: bool) -> DictRow {
        DictRow {
            id: dictionary_id(word),
            word: word.into(),
            replacement: None,
            updated_at: at,
            deleted,
        }
    }

    #[test]
    fn last_write_wins() {
        assert!(remote_wins(None, 5));
        assert!(remote_wins(Some(4), 5));
        assert!(!remote_wins(Some(6), 5));
        assert!(!remote_wins(Some(5), 5), "tie keeps local");
    }

    #[test]
    fn ids_match_python_and_android() {
        // python3 -c "import uuid;print(uuid.uuid5(uuid.NAMESPACE_URL,'flowlite:dictionary:kubernetes'))"
        assert_eq!(
            dictionary_id("  Kubernetes "),
            "55ac65ef-9da4-55c6-a4ce-1df859da6111"
        );
    }

    #[test]
    fn cursor_overlap() {
        assert_eq!(pull_from(""), "");
        assert_eq!(
            pull_from("2026-10-12 10:33:11.123Z"),
            "2026-10-12 10:33:09.123Z"
        );
    }

    #[test]
    fn detects_local_adds_removes_and_revives() {
        let rows = vec![
            row("Kubernetes", 1, false),
            row("Tauri", 1, false),
            row("Rust", 1, true),
        ];
        let words = vec![
            "kubernetes".to_string(),
            "Rust".to_string(),
            "Zed".to_string(),
        ];
        let ch = local_dictionary_changes(&words, &rows, 9);
        let summary: Vec<(String, bool)> = ch.iter().map(|r| (r.word.clone(), r.deleted)).collect();
        assert_eq!(
            summary,
            vec![
                ("Rust".into(), false),
                ("Zed".into(), false),
                ("Tauri".into(), true)
            ]
        );
        assert!(ch.iter().all(|r| r.updated_at == 9));
    }

    #[test]
    fn replacement_rows_are_not_custom_words() {
        let mut r = row("gonna", 1, false);
        r.replacement = Some("going to".into());
        assert!(local_dictionary_changes(&[], &[r.clone()], 9).is_empty());
        assert!(words_from_rows(&[], &[r]).is_empty());
    }

    #[test]
    fn applies_remote_rows_to_words() {
        let rows = vec![
            row("Kubernetes", 2, false),
            row("Tauri", 2, true),
            row("Greek", 2, false),
        ];
        let words = vec!["Tauri".to_string(), "Kubernetes".to_string()];
        assert_eq!(
            words_from_rows(&words, &rows),
            vec!["Kubernetes".to_string(), "Greek".to_string()]
        );
    }

    #[test]
    fn replacements_match_whole_words_only() {
        let mut email = row("my email", 1, false);
        email.replacement = Some("me@example.com".into());
        let mut gonna = row("gonna", 1, false);
        gonna.replacement = Some("going to".into());
        let rows = vec![
            email,
            gonna,
            row("Kubernetes", 1, false),
            row("old", 1, true),
        ];
        assert_eq!(
            apply_replacements(
                "Send it to My Email, I'm gonna deploy kubernetes. Gonnabe old",
                &rows
            ),
            "Send it to me@example.com, I'm going to deploy Kubernetes. Gonnabe old"
        );
    }

    #[test]
    fn retention() {
        assert_eq!(retention_cutoff(None, 100), None);
        assert_eq!(retention_cutoff(Some(0), 100), None);
        assert_eq!(retention_cutoff(Some(1), 86_400_100), Some(100));
    }

    #[test]
    fn untrusted_replacements_are_bounded_and_cleaned() {
        let mut esc = row("deploy", 1, false);
        esc.replacement = Some("ok\u{1b}[31m\r\nline two\tend".into());
        let mut huge = row("hello", 1, false);
        huge.replacement = Some("x".repeat(MAX_REPLACEMENT + 1));
        let long = row(&"w".repeat(MAX_WORD + 1), 1, false);
        assert_eq!(
            apply_replacements("deploy hello", &[esc, huge, long]),
            "ok[31m\nline two\tend hello"
        );
    }

    #[test]
    fn replacements_do_not_cascade() {
        let mut a = row("alpha", 1, false);
        a.replacement = Some("beta beta".into());
        let mut b = row("beta", 1, false);
        b.replacement = Some("gamma".into());
        assert_eq!(
            apply_replacements("alpha and beta", &[a, b]),
            "beta beta and gamma"
        );
    }
}
