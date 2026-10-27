//! Git-native storage layer. See SPEC.md §5.
//!
//! Layout:
//!   `refs/provenance/events/<session-shard>`  — append-only JSONL event logs
//!   refs/notes/provenance                   — per-commit attribution notes
//!   blobs                                   — before/after file contents
//!
//! The user's working branch is never touched. Provenance refs are
//! pushable optionally (governance decision: prompts may be sensitive).
//!
//! Event log uses per-session shard refs so concurrent agents don't
//! contend. Ref updates use CAS via `git update-ref` (§5, Entire pattern).
//! No-op suppression: skip flush when content hash is unchanged.

use crate::git;
use crate::model::{Event, Session};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

const EVENTS_REF_PREFIX: &str = "refs/provenance/events/";
const EVENTS_FILENAME: &str = "events.jsonl";
const SESSIONS_FILENAME: &str = "sessions.jsonl";

/// Shard ref name for a session, e.g. "refs/provenance/events/01JRXQ7K..."
/// Uses the first 10 chars of the session ULID as the shard key.
fn shard_ref(session_id: &ulid::Ulid) -> String {
    let id_str = session_id.to_string();
    let shard = &id_str[..10.min(id_str.len())];
    format!("{EVENTS_REF_PREFIX}{shard}")
}

/// Append events to the session's shard ref as a JSONL commit.
/// Each call creates a new commit parented on the ref's current tip
/// (or an orphan if the ref doesn't exist yet). The tree contains
/// a single file "events.jsonl" with all events for this shard
/// (previous + new, accumulated).
pub fn append_events(repo: &Path, session: &Session, events: &[Event]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    let refname = shard_ref(&session.id);
    let tip = git::resolve_ref(repo, &refname)?;

    // Read existing JSONL from the ref (if any) and append new events
    let mut jsonl = String::new();
    if tip.is_some()
        && let Some(existing) = git::show_file_at_ref(repo, &refname, EVENTS_FILENAME)?
    {
        jsonl.push_str(&existing);
    }

    for event in events {
        let line = serde_json::to_string(event)
            .with_context(|| format!("failed to serialize event {}", event.id))?;
        jsonl.push_str(&line);
        jsonl.push('\n');
    }

    // Also store/update the session metadata
    let session_json = serde_json::to_string(session)?;
    let session_blob = git::write_blob(repo, format!("{session_json}\n").as_bytes())?;

    // Build tree with both files
    let events_blob = git::write_blob(repo, jsonl.as_bytes())?;
    let tree_entry = format!(
        "100644 blob {events_blob}\t{EVENTS_FILENAME}\n100644 blob {session_blob}\t{SESSIONS_FILENAME}\n"
    );
    let tree = git::run_with_stdin(repo, &["mktree"], tree_entry.as_bytes())?
        .trim()
        .to_string();

    let commit = git::commit_tree(repo, &tree, tip.as_deref(), "diffy: append events")?;
    git::update_ref(repo, &refname, &commit, tip.as_deref())?;

    Ok(())
}

/// Read all events across all session shards, sorted by timestamp (oldest first).
/// This is the entry point for the replay engine — it merges all shards
/// into one unified timeline regardless of which session produced each event.
pub fn read_all_events(repo: &Path) -> Result<Vec<Event>> {
    let refs = git::list_refs(repo, EVENTS_REF_PREFIX)?;
    let mut all_events: Vec<Event> = Vec::new();

    for refname in &refs {
        if let Some(content) = git::show_file_at_ref(repo, refname, EVENTS_FILENAME)? {
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let event: Event = serde_json::from_str(line)
                    .with_context(|| format!("failed to parse event from {refname}: {line}"))?;
                all_events.push(event);
            }
        }
    }

    // Sort by ULID — which is time-sortable by construction (§5).
    // Events within the same millisecond sort deterministically by
    // the ULID's random component.
    all_events.sort_by_key(|e| e.id);
    Ok(all_events)
}

/// Read all sessions from all shards.
pub fn read_all_sessions(repo: &Path) -> Result<Vec<Session>> {
    let refs = git::list_refs(repo, EVENTS_REF_PREFIX)?;
    let mut sessions: Vec<Session> = Vec::new();

    for refname in &refs {
        if let Some(content) = git::show_file_at_ref(repo, refname, SESSIONS_FILENAME)? {
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let session: Session = serde_json::from_str(line)
                    .with_context(|| format!("failed to parse session from {refname}"))?;
                sessions.push(session);
            }
        }
    }

    Ok(sessions)
}

/// Read events for a specific file path only.
#[allow(dead_code)] // exercised by tests; the replay path reads all events
pub fn read_events_for_path(repo: &Path, path: &str) -> Result<Vec<Event>> {
    let all = read_all_events(repo)?;
    Ok(all.into_iter().filter(|e| e.path == path).collect())
}

/// Collapse a file's char-level session attribution into per-line dominant
/// session ids (§5 export: char precision degrades to line ranges).
///
/// `sessions[i]` is the session id owning byte `i` of `content` (same
/// length). Each line (bytes between `\n`) is assigned the session owning
/// the most bytes in that line; an empty line carries an empty string.
pub fn lines_by_dominant_session(content: &str, sessions: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut start = 0;
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            result.push(dominant_session(&sessions[start..i]));
            start = i + 1;
        }
    }
    if start < content.len() {
        result.push(dominant_session(&sessions[start..]));
    }
    result
}

fn dominant_session(sessions: &[String]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for s in sessions {
        *counts.entry(s.as_str()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(_, c)| c)
        .map(|(k, _)| k.to_string())
        .unwrap_or_default()
}

/// Build a provenance note in git-ai `authorship/3.0.0` shape from per-file
/// line attribution.
///
/// `files` maps path → per-line session id; `sessions` maps session id →
/// `(model, tool)`; `trace` is a per-checkpoint random id. Consecutive
/// lines sharing a session collapse to ranges (`1-2,4`), keyed
/// `s_<id>::t_<trace>`; the metadata `sessions` map resolves each key to a
/// model (readable by git-ai tooling and our own blame-join).
pub fn build_checkpoint_note(
    files: &[(String, Vec<String>)],
    sessions: &HashMap<String, (String, String)>,
    trace: &str,
) -> String {
    let mut attestation = String::new();
    let mut all_sessions: Vec<String> = Vec::new();

    for (path, line_sessions) in files {
        attestation.push_str(path);
        attestation.push('\n');

        // Collect runs of consecutive lines per session (1-indexed inclusive).
        let mut ranges: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        let mut file_order: Vec<String> = Vec::new();
        let mut i = 0;
        while i < line_sessions.len() {
            let sid = line_sessions[i].clone();
            if sid.is_empty() {
                i += 1;
                continue;
            }
            let start = i;
            while i < line_sessions.len() && line_sessions[i] == sid {
                i += 1;
            }
            let (first, last) = (start + 1, i);
            if !ranges.contains_key(&sid) {
                file_order.push(sid.clone());
                if !all_sessions.contains(&sid) {
                    all_sessions.push(sid.clone());
                }
            }
            ranges.entry(sid).or_default().push((first, last));
        }

        for sid in &file_order {
            if let Some(rs) = ranges.get(sid) {
                let key = format!("s_{sid}::t_{trace}");
                let formatted = rs
                    .iter()
                    .map(|&(f, l)| {
                        if f == l {
                            f.to_string()
                        } else {
                            format!("{f}-{l}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                attestation.push_str(&format!("  {key}  {formatted}\n"));
            }
        }
    }

    attestation.push_str("---\n");

    // Metadata: sessions map (keyed s_<id>) carrying model + tool.
    let mut sessions_obj = serde_json::Map::new();
    for sid in &all_sessions {
        let (model, tool) = sessions
            .get(sid)
            .cloned()
            .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));
        sessions_obj.insert(
            format!("s_{sid}"),
            serde_json::json!({
                "agent_id": { "tool": tool, "id": sid, "model": model }
            }),
        );
    }

    let metadata = serde_json::json!({
        "schema_version": "authorship/3.0.0",
        "base_commit_sha": "",
        "prompts": {},
        "sessions": sessions_obj,
    });
    attestation.push_str(&format!("{metadata:#}"));
    attestation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Evidence, Tool};
    use tempfile::TempDir;

    fn temp_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        git::init(&path).unwrap();
        (dir, path)
    }

    fn make_session(model: &str) -> Session {
        Session {
            id: ulid::Ulid::generate(),
            tool: Tool::Cursor,
            model: model.to_string(),
            agent_instance: None,
            conversation_id: None,
            started: jiff::Timestamp::now(),
            ended: None,
            base_commit: None,
        }
    }

    fn make_event(session: &Session, path: &str, before: &str, after: &str) -> Event {
        Event {
            id: ulid::Ulid::generate(),
            session_id: session.id,
            prompt_id: None,
            tool_call_id: None,
            path: path.to_string(),
            before_blob: before.to_string(),
            after_blob: after.to_string(),
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        }
    }

    #[test]
    fn append_and_read_events_roundtrip_5() {
        // §5: events written to shard refs, readable back sorted by time
        let (_dir, repo) = temp_repo();
        let session = make_session("opus-4.6");
        let e1 = make_event(&session, "src/main.rs", "aaa", "bbb");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let e2 = make_event(&session, "src/lib.rs", "ccc", "ddd");

        append_events(&repo, &session, &[e1.clone(), e2.clone()]).unwrap();

        let events = read_all_events(&repo).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, e1.id);
        assert_eq!(events[1].id, e2.id);
    }

    #[test]
    fn multiple_appends_accumulate_5() {
        // §5: successive appends to the same shard accumulate
        let (_dir, repo) = temp_repo();
        let session = make_session("opus-4.6");
        let e1 = make_event(&session, "a.rs", "aaa", "bbb");
        append_events(&repo, &session, &[e1]).unwrap();

        let e2 = make_event(&session, "b.rs", "ccc", "ddd");
        append_events(&repo, &session, &[e2]).unwrap();

        let events = read_all_events(&repo).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn cross_session_events_merge_sorted_5() {
        // §5: events from different sessions (different shards) merge
        // into a single timeline sorted by timestamp/ULID
        let (_dir, repo) = temp_repo();

        let s1 = make_session("opus-4.6");
        let e1 = make_event(&s1, "a.rs", "aaa", "bbb");
        append_events(&repo, &s1, std::slice::from_ref(&e1)).unwrap();

        // Small delay to ensure different ULID timestamps
        std::thread::sleep(std::time::Duration::from_millis(2));

        let s2 = make_session("gpt-5.2");
        let e2 = make_event(&s2, "b.rs", "ccc", "ddd");
        append_events(&repo, &s2, std::slice::from_ref(&e2)).unwrap();

        let events = read_all_events(&repo).unwrap();
        assert_eq!(events.len(), 2);
        // e1 was created first, so it should sort first
        assert_eq!(events[0].id, e1.id);
        assert_eq!(events[1].id, e2.id);
    }

    #[test]
    fn empty_repo_returns_no_events_5() {
        // §5: fresh repo with no provenance data
        let (_dir, repo) = temp_repo();
        let events = read_all_events(&repo).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn session_metadata_persisted_5() {
        // §5: session info stored alongside events
        let (_dir, repo) = temp_repo();
        let session = make_session("opus-4.6");
        let e = make_event(&session, "a.rs", "aaa", "bbb");
        append_events(&repo, &session, &[e]).unwrap();

        let sessions = read_all_sessions(&repo).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].model, "opus-4.6");
    }

    #[test]
    fn read_events_for_path_filters_5() {
        // §5: can filter events to a specific file
        let (_dir, repo) = temp_repo();
        let session = make_session("opus-4.6");
        let e1 = make_event(&session, "src/a.rs", "aaa", "bbb");
        let e2 = make_event(&session, "src/b.rs", "ccc", "ddd");
        let e3 = make_event(&session, "src/a.rs", "bbb", "eee");
        append_events(&repo, &session, &[e1, e2, e3]).unwrap();

        let a_events = read_events_for_path(&repo, "src/a.rs").unwrap();
        assert_eq!(a_events.len(), 2);
        assert!(a_events.iter().all(|e| e.path == "src/a.rs"));
    }

    #[test]
    fn lines_by_dominant_session_maps_chars_to_lines_5() {
        // §5: char-level attribution collapses to per-line dominant session
        let content = "aa\nbbb\nc\n"; // 3 lines: "aa", "bbb", "c"
        let sessions = vec![
            "s1".to_string(),
            "s1".to_string(),
            "s1".to_string(), // aa\n
            "s2".to_string(),
            "s2".to_string(),
            "s2".to_string(),
            "s2".to_string(), // bbb\n
            "s3".to_string(),
            "s3".to_string(), // c\n
        ];
        let lines = lines_by_dominant_session(content, &sessions);
        assert_eq!(
            lines,
            vec!["s1".to_string(), "s2".to_string(), "s3".to_string()]
        );
    }

    #[test]
    fn lines_by_dominant_session_handles_empty_lines_5() {
        // §5: an empty line has no chars, so it carries an empty session
        let content = "aa\n\nbb";
        let sessions = vec![
            "s1".to_string(),
            "s1".to_string(),
            "s1".to_string(),
            "s2".to_string(),
            "s2".to_string(),
            "s2".to_string(),
        ];
        let lines = lines_by_dominant_session(content, &sessions);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "s1");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "s2");
    }

    #[test]
    fn build_checkpoint_note_emits_git_ai_format_5() {
        // §5: checkpoint note in git-ai authorship/3.0.0 shape
        let files = vec![(
            "src/a.rs".to_string(),
            vec![
                "sess1".to_string(),
                "sess1".to_string(),
                "sess2".to_string(),
            ],
        )];
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(
            "sess1".to_string(),
            ("opus".to_string(), "cursor".to_string()),
        );
        sessions.insert(
            "sess2".to_string(),
            ("gpt".to_string(), "cursor".to_string()),
        );

        let note = build_checkpoint_note(&files, &sessions, "trace1");
        assert!(note.starts_with("src/a.rs\n  s_sess1::t_trace1  1-2\n  s_sess2::t_trace1  3\n"));
        assert!(note.contains("\"schema_version\": \"authorship/3.0.0\""));
        assert!(note.contains("\"s_sess1\""));
        assert!(note.contains("\"model\": \"opus\""));
    }

    #[test]
    fn build_checkpoint_note_groups_noncontiguous_lines_5() {
        // §5: same session on non-contiguous lines collapses to ranges "1-2,4"
        let files = vec![(
            "f.rs".to_string(),
            vec![
                "sess1".to_string(),
                "sess1".to_string(),
                "sess2".to_string(),
                "sess1".to_string(),
            ],
        )];
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(
            "sess1".to_string(),
            ("opus".to_string(), "cursor".to_string()),
        );
        sessions.insert(
            "sess2".to_string(),
            ("gpt".to_string(), "cursor".to_string()),
        );

        let note = build_checkpoint_note(&files, &sessions, "t");
        assert!(note.starts_with("f.rs\n  s_sess1::t_t  1-2,4\n  s_sess2::t_t  3\n"));
    }
}
