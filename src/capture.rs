//! Capture layer. See SPEC.md §4.
//!
//! Hook mode (§4.1): agent hooks invoke `diffy hook --payload …` on every
//! edit tool call. The payload is JSON describing what changed.
//!
//! This module parses hook payloads into Events and Sessions.

use crate::model::{Event, Evidence, Session, Tool};
use anyhow::{Context, Result};
use serde::Deserialize;
use ulid::Ulid;

/// A single search-replace edit from Cursor's afterFileEdit payload.
#[derive(Debug, Clone, Deserialize)]
pub struct CursorEdit {
    pub old_string: String,
    pub new_string: String,
}

/// Cursor's native afterFileEdit payload (§4.1).
/// Cursor sends {file_path, edits[], model, conversation_id, ...} — not
/// full before/after content. We parse this natively and reconstruct the
/// before content by reverse-applying the edits against the file on disk.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // extra fields mirror Cursor's schema (§4.1) for forward-compat
pub struct CursorPayload {
    /// Absolute path to the edited file.
    pub file_path: String,
    /// Array of search-replace diffs applied by the agent.
    pub edits: Vec<CursorEdit>,
    /// Model slug, e.g. "claude-opus-4-6".
    pub model: String,
    /// Conversation / session ID (canonical session identifier).
    pub conversation_id: String,
    /// Per-turn generation ID.
    pub generation_id: Option<String>,
    /// Always "afterFileEdit" for this hook type.
    pub hook_event_name: Option<String>,
    /// Cursor version string.
    pub cursor_version: Option<String>,
    /// Workspace root directories.
    pub workspace_roots: Option<Vec<String>>,
    /// Structured model identifier.
    pub model_id: Option<String>,
    /// Model parameters.
    pub model_params: Option<serde_json::Value>,
    /// User email if available.
    pub user_email: Option<String>,
    /// Path to the conversation transcript.
    pub transcript_path: Option<String>,
}

impl CursorPayload {
    /// Convert to a HookPayload given the reconstructed before content, the
    /// file's current (after) content, and the relative path within the repo.
    pub fn to_hook_payload(
        &self,
        before: String,
        after_content: &str,
        rel_path: &str,
    ) -> HookPayload {
        HookPayload {
            model: self.model.clone(),
            tool: "cursor".into(),
            session_id: self.conversation_id.clone(),
            path: rel_path.into(),
            before,
            after: after_content.into(),
            tool_call_id: self.generation_id.clone(),
            prompt: None,
        }
    }
}

/// Reconstruct the file's "before" state by reverse-applying Cursor edits.
/// The file on disk is already the "after" state. We walk the edits in
/// reverse order, replacing each `new_string` back with `old_string`.
///
/// Returns `None` when any edit can't be placed unambiguously: its
/// `new_string` must occur exactly once at that step, and can't be empty.
/// Guessing the first match would credit the model with someone else's
/// chars (§4.1, I4).
pub fn reconstruct_before(after: &str, edits: &[CursorEdit]) -> Option<String> {
    let mut content = after.to_string();
    for edit in edits.iter().rev() {
        if edit.new_string.is_empty() {
            return None;
        }
        let mut matches = content.match_indices(&edit.new_string);
        let (idx, _) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        content = format!(
            "{}{}{}",
            &content[..idx],
            edit.old_string,
            &content[idx + edit.new_string.len()..]
        );
    }
    Some(content)
}

/// The edit was deliberately not captured because git ignores the file
/// (§4.1). Callers treat this as a skip, not a failure.
#[derive(Debug, thiserror::Error)]
#[error("skipped {path}: ignored by git")]
pub struct IgnoredByGit {
    pub path: String,
}

/// Model name for changes diffy saw but couldn't attribute. Matches the
/// report's drift bucket (§6.5).
pub const UNTRACKED_MODEL: &str = "untracked";

/// Outcome of capturing one Cursor edit.
#[derive(Debug)]
pub struct CursorCapture {
    pub session: Session,
    pub event: Event,
    /// Set when the edit couldn't be placed and was recorded as untracked.
    pub warning: Option<String>,
}

/// Process a Cursor afterFileEdit payload end-to-end:
/// read the file from disk (after state), reconstruct before,
/// compute relative path, then delegate to `process_hook`.
pub fn process_cursor_hook(
    repo: &std::path::Path,
    cursor: &CursorPayload,
) -> Result<CursorCapture> {
    // §4.1: resolve both sides before stripping, so symlinked paths (macOS
    // /var vs /private/var, a symlinked project folder) still yield a
    // repo-relative path. Outside the repo is an error, never an absolute path.
    let repo_canon = repo
        .canonicalize()
        .with_context(|| format!("failed to resolve repo path {}", repo.display()))?;
    let file_canon = std::path::Path::new(&cursor.file_path)
        .canonicalize()
        .with_context(|| format!("failed to resolve edited file {}", cursor.file_path))?;
    let rel_path = file_canon
        .strip_prefix(&repo_canon)
        .with_context(|| {
            format!(
                "{} is outside the repo {}",
                file_canon.display(),
                repo_canon.display()
            )
        })?
        .to_string_lossy()
        .into_owned();

    // Read the file from disk — it's already in the "after" state
    let after_content = std::fs::read_to_string(&file_canon)
        .with_context(|| format!("failed to read edited file: {}", file_canon.display()))?;

    if let Some(before) = reconstruct_before(&after_content, &cursor.edits) {
        let hook = cursor.to_hook_payload(before, &after_content, &rel_path);
        let (session, event) = process_hook(repo, &hook)?;
        return Ok(CursorCapture {
            session,
            event,
            warning: None,
        });
    }

    // §4.1 fail-closed: bridge from the last recorded state to the disk
    // under `untracked`, so replay's before-state stays consistent and
    // later events aren't shifted onto the wrong chars.
    let last_after = last_recorded_after(repo, &rel_path)?.unwrap_or_default();
    let hook = HookPayload {
        model: UNTRACKED_MODEL.into(),
        tool: "cursor".into(),
        // A separate session, so the real conversation's model isn't
        // overwritten in sessions.jsonl.
        session_id: format!("{UNTRACKED_MODEL}:{}", cursor.conversation_id),
        path: rel_path.clone(),
        before: last_after,
        after: after_content,
        tool_call_id: cursor.generation_id.clone(),
        prompt: None,
    };
    let (session, event) = process_hook(repo, &hook)?;
    let warning = format!(
        "{rel_path}: couldn't place {} edit(s) from conversation {} (text not found, \
         repeated, or a pure deletion); recorded as {UNTRACKED_MODEL}",
        cursor.edits.len(),
        cursor.conversation_id
    );
    Ok(CursorCapture {
        session,
        event,
        warning: Some(warning),
    })
}

/// JSON payload sent by an agent hook (e.g. Cursor hook, Claude Code PostToolUse).
/// This is the external contract — agents produce this; we consume it.
#[derive(Debug, Deserialize)]
pub struct HookPayload {
    /// Exact model slug, e.g. "claude-opus-4-6"
    pub model: String,
    /// Agent tool name: "cursor", "claude-code", "codex", "gemini"
    pub tool: String,
    /// Session/conversation id from the agent
    pub session_id: String,
    /// File path relative to repo root
    pub path: String,
    /// File content before the edit (full text)
    pub before: String,
    /// File content after the edit (full text)
    pub after: String,
    /// Agent-reported tool call id, if available
    pub tool_call_id: Option<String>,
    /// Prompt text that triggered this edit, if available (§3; captured in a later milestone)
    #[allow(dead_code)]
    pub prompt: Option<String>,
}

/// Parse the tool name string into our Tool enum.
fn parse_tool(s: &str) -> Tool {
    match s.to_lowercase().as_str() {
        "cursor" => Tool::Cursor,
        "claude-code" | "claude_code" | "claudecode" => Tool::ClaudeCode,
        "codex" => Tool::Codex,
        "gemini" => Tool::Gemini,
        other => Tool::Other(other.to_string()),
    }
}

/// Process a hook payload: write blobs, create session+event, store them.
/// Returns the session and event for confirmation/logging.
pub fn process_hook(repo: &std::path::Path, payload: &HookPayload) -> Result<(Session, Event)> {
    validate_rel_path(&payload.path)?;
    // §4.1: checked before any blob is written — hash-object bypasses
    // .gitignore, so this is the only thing keeping ignored secrets out.
    if crate::git::is_ignored(repo, &payload.path)? {
        return Err(IgnoredByGit {
            path: payload.path.clone(),
        }
        .into());
    }

    // §6.5: replay assumes `before` is the last recorded state. If the file
    // changed in between, record that change as untracked first, so the
    // model's diff lands on the right chars.
    let event_id = match last_recorded_after(repo, &payload.path)? {
        Some(last) if last != payload.before => {
            let gap = HookPayload {
                model: UNTRACKED_MODEL.into(),
                tool: payload.tool.clone(),
                session_id: format!("{UNTRACKED_MODEL}:{}", payload.session_id),
                path: payload.path.clone(),
                before: last,
                after: payload.before.clone(),
                tool_call_id: None,
                prompt: None,
            };
            let (_, bridge) = record_event(repo, &gap, Ulid::generate())?;
            // Events sort by ULID; increment keeps the bridge strictly first
            // even within the same millisecond.
            bridge.id.increment().unwrap_or_else(|_| Ulid::generate())
        }
        _ => Ulid::generate(),
    };
    record_event(repo, payload, event_id)
}

/// The file's after-state as of its most recent recorded event, if any.
fn last_recorded_after(repo: &std::path::Path, path: &str) -> Result<Option<String>> {
    crate::store::read_events_for_path(repo, path)?
        .last()
        .map(|e| crate::git::read_blob(repo, &e.after_blob))
        .transpose()
        .map(|blob| blob.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Write blobs and store one event with the given id. No checks: callers
/// go through [`process_hook`].
fn record_event(
    repo: &std::path::Path,
    payload: &HookPayload,
    id: Ulid,
) -> Result<(Session, Event)> {
    // Write before/after content as git blobs
    let before_blob = crate::git::write_blob(repo, payload.before.as_bytes())
        .context("failed to write before blob")?;
    let after_blob = crate::git::write_blob(repo, payload.after.as_bytes())
        .context("failed to write after blob")?;

    // Derive a deterministic session ULID from the agent's session id
    // so repeated events from the same session land in the same shard.
    let session_ulid = session_ulid_from_str(&payload.session_id);

    let session = Session {
        id: session_ulid,
        tool: parse_tool(&payload.tool),
        model: payload.model.clone(),
        agent_instance: None,
        conversation_id: Some(payload.session_id.clone()),
        started: jiff::Timestamp::now(),
        ended: None,
        base_commit: crate::git::head(repo)?.map(|s| s.to_string()),
    };

    let event = Event {
        id,
        session_id: session_ulid,
        prompt_id: None,
        tool_call_id: payload.tool_call_id.clone(),
        path: payload.path.clone(),
        before_blob,
        after_blob,
        transparent: false,
        evidence: Evidence::ToolCall,
        ts: jiff::Timestamp::now(),
    };

    // Store the event
    crate::store::append_events(repo, &session, std::slice::from_ref(&event))?;

    Ok((session, event))
}

/// §4.1: event paths must be repo-relative and stay inside the repo, so a
/// bad integration can't plant absolute paths (which never match files on
/// disk and leak the local layout into pushable refs).
pub(crate) fn validate_rel_path(path: &str) -> Result<()> {
    use std::path::Component;
    let ok = !path.is_empty()
        && std::path::Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    anyhow::ensure!(
        ok,
        "hook payload path must be relative to the repo root without `..`, got {path:?}"
    );
    Ok(())
}

/// Derive a fully deterministic ULID from the agent's session id string.
/// Same string → same ULID every time, so all events from one session
/// land in the same shard ref (§5). The ULID is not time-sortable
/// (session ordering comes from event timestamps instead).
fn session_ulid_from_str(s: &str) -> Ulid {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    let hi = h.finish();
    // Hash again for the low bits
    "salt".hash(&mut h);
    s.hash(&mut h);
    let lo = h.finish();
    Ulid::from((hi as u128) << 64 | lo as u128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        crate::git::init(&path).unwrap();
        (dir, path)
    }

    #[test]
    fn process_hook_stores_event_4_1() {
        // §4.1: hook payload → event stored in provenance refs
        let (_dir, repo) = temp_repo();
        let payload = HookPayload {
            model: "opus-4.6".into(),
            tool: "cursor".into(),
            session_id: "test-session-123".into(),
            path: "src/main.rs".into(),
            before: "".into(),
            after: "fn main() {}".into(),
            tool_call_id: Some("toolu_abc".into()),
            prompt: Some("create a main function".into()),
        };

        let (session, event) = process_hook(&repo, &payload).unwrap();
        assert_eq!(session.model, "opus-4.6");
        assert_eq!(event.path, "src/main.rs");

        // Verify event is readable from the store
        let events = crate::store::read_all_events(&repo).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "src/main.rs");
    }

    #[test]
    fn process_hook_preserves_conversation_id_4_1() {
        // §4.1: the agent's raw session/conversation id (Cursor's
        // conversation_id) must be preserved verbatim on the Session so the
        // report can link a span back to the originating chat — not just the
        // derived hash ULID.
        let (_dir, repo) = temp_repo();
        let payload = HookPayload {
            model: "opus-4.6".into(),
            tool: "cursor".into(),
            session_id: "conv-abc-123".into(),
            path: "src/main.rs".into(),
            before: "".into(),
            after: "fn main() {}".into(),
            tool_call_id: None,
            prompt: None,
        };

        let (session, _event) = process_hook(&repo, &payload).unwrap();
        assert_eq!(session.conversation_id.as_deref(), Some("conv-abc-123"));
    }

    #[test]
    fn same_session_id_same_shard_4_1() {
        // §4.1 + §5: same agent session → same shard ref
        let (_dir, repo) = temp_repo();
        for (before, after) in [("", "v1"), ("v1", "v2"), ("v2", "v3")] {
            let payload = HookPayload {
                model: "opus-4.6".into(),
                tool: "cursor".into(),
                session_id: "same-session".into(),
                path: "a.rs".into(),
                before: before.into(),
                after: after.into(),
                tool_call_id: None,
                prompt: None,
            };
            process_hook(&repo, &payload).unwrap();
        }

        let events = crate::store::read_all_events(&repo).unwrap();
        assert_eq!(events.len(), 3);
        // All share the same session_id
        assert!(events.iter().all(|e| e.session_id == events[0].session_id));
    }

    // --- Cursor afterFileEdit payload tests ---

    #[test]
    fn parse_cursor_payload_single_edit_4_1() {
        // §4.1: Cursor's afterFileEdit sends {file_path, edits[], model, conversation_id}
        let json = r#"{
            "file_path": "/home/user/project/src/main.rs",
            "edits": [
                { "old_string": "hello", "new_string": "hello world" }
            ],
            "model": "claude-opus-4-6",
            "model_id": "claude-opus-4-6",
            "conversation_id": "conv-abc-123",
            "generation_id": "gen-001",
            "hook_event_name": "afterFileEdit",
            "cursor_version": "1.0.0",
            "workspace_roots": ["/home/user/project"]
        }"#;

        let cursor: CursorPayload = serde_json::from_str(json).unwrap();
        assert_eq!(cursor.file_path, "/home/user/project/src/main.rs");
        assert_eq!(cursor.model, "claude-opus-4-6");
        assert_eq!(cursor.conversation_id, "conv-abc-123");
        assert_eq!(cursor.edits.len(), 1);
        assert_eq!(cursor.edits[0].old_string, "hello");
        assert_eq!(cursor.edits[0].new_string, "hello world");
    }

    #[test]
    fn parse_cursor_payload_multiple_edits_4_1() {
        // §4.1: Cursor can send multiple edits in one afterFileEdit event
        let json = r#"{
            "file_path": "/repo/lib.rs",
            "edits": [
                { "old_string": "fn foo()", "new_string": "fn foo() -> bool" },
                { "old_string": "return;", "new_string": "return true;" }
            ],
            "model": "gpt-5.2",
            "conversation_id": "conv-xyz",
            "generation_id": "gen-002",
            "hook_event_name": "afterFileEdit",
            "cursor_version": "1.0.0",
            "workspace_roots": ["/repo"]
        }"#;

        let cursor: CursorPayload = serde_json::from_str(json).unwrap();
        assert_eq!(cursor.edits.len(), 2);
        assert_eq!(cursor.edits[1].new_string, "return true;");
    }

    #[test]
    fn cursor_reconstruct_before_single_edit_4_1() {
        // §4.1: given "after" content on disk and edits, reconstruct "before"
        let after = "fn main() {\n    println!(\"hello world\");\n}";
        let edits = vec![CursorEdit {
            old_string: "hello".into(),
            new_string: "hello world".into(),
        }];

        let before = reconstruct_before(after, &edits);
        assert_eq!(
            before.as_deref(),
            Some("fn main() {\n    println!(\"hello\");\n}")
        );
    }

    #[test]
    fn cursor_reconstruct_before_multiple_edits_4_1() {
        // §4.1: reverse-apply multiple edits in reverse order
        let after = "fn foo() -> bool {\n    return true;\n}";
        let edits = vec![
            CursorEdit {
                old_string: "fn foo()".into(),
                new_string: "fn foo() -> bool".into(),
            },
            CursorEdit {
                old_string: "return;".into(),
                new_string: "return true;".into(),
            },
        ];

        let before = reconstruct_before(after, &edits);
        assert_eq!(before.as_deref(), Some("fn foo() {\n    return;\n}"));
    }

    #[test]
    fn cursor_reconstruct_before_new_file_4_1() {
        // §4.1: new file creation — old_string is empty for each edit
        let after = "fn main() {}";
        let edits = vec![CursorEdit {
            old_string: "".into(),
            new_string: "fn main() {}".into(),
        }];

        let before = reconstruct_before(after, &edits);
        assert_eq!(before.as_deref(), Some(""));
    }

    fn edit(old: &str, new: &str) -> CursorEdit {
        CursorEdit {
            old_string: old.into(),
            new_string: new.into(),
        }
    }

    #[test]
    fn reconstruct_before_refuses_repeated_new_string_4_1() {
        // I4: `}` appears twice, so the edit's location is ambiguous.
        let after = "fn a() {\n}\nfn b() {\n}\n";
        assert_eq!(reconstruct_before(after, &[edit("", "}")]), None);
    }

    #[test]
    fn reconstruct_before_refuses_missing_new_string_4_1() {
        // The file changed again before the hook read it.
        let after = "fn main() { goodbye(); }";
        assert_eq!(
            reconstruct_before(after, &[edit("hello()", "hello_world()")]),
            None
        );
    }

    #[test]
    fn reconstruct_before_refuses_pure_deletion_4_1() {
        // An empty new_string matches everywhere; there's nothing to locate.
        assert_eq!(reconstruct_before("abc", &[edit("removed", "")]), None);
    }

    #[test]
    fn reconstruct_before_refuses_if_any_edit_is_ambiguous_4_1() {
        // The first edit is fine on its own; the second is repeated.
        let after = "let x = 1;\nok();\nok();\n";
        let edits = [edit("let x = 0;", "let x = 1;"), edit("", "ok();")];
        assert_eq!(reconstruct_before(after, &edits), None);
    }

    fn cursor_payload(
        file_path: &std::path::Path,
        edits: Vec<CursorEdit>,
        conv: &str,
    ) -> CursorPayload {
        CursorPayload {
            file_path: file_path.to_string_lossy().into(),
            edits,
            model: "claude-opus-4-6".into(),
            conversation_id: conv.into(),
            generation_id: None,
            hook_event_name: None,
            cursor_version: None,
            workspace_roots: None,
            model_id: None,
            model_params: None,
            user_email: None,
            transcript_path: None,
        }
    }

    fn blob_text(repo: &std::path::Path, hash: &str) -> String {
        String::from_utf8(crate::git::read_blob(repo, hash).unwrap()).unwrap()
    }

    // --- §4.1 event paths are always repo-relative ---

    /// A repo at its resolved path, plus a symlink pointing at it.
    #[cfg(unix)]
    fn repo_with_symlink() -> (TempDir, TempDir, std::path::PathBuf, std::path::PathBuf) {
        let (dir, _) = temp_repo();
        let real = dir.path().canonicalize().unwrap();
        let links = TempDir::new().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        (dir, links, real, link)
    }

    fn only_event_path(repo: &std::path::Path) -> String {
        let events = crate::store::read_all_events(repo).unwrap();
        assert_eq!(events.len(), 1);
        events[0].path.clone()
    }

    #[cfg(unix)]
    #[test]
    fn cursor_symlinked_file_path_is_stored_relative_4_1() {
        let (_d, _l, real, link) = repo_with_symlink();
        std::fs::write(link.join("lib.rs"), "fn a() {}").unwrap();
        let payload = cursor_payload(&link.join("lib.rs"), vec![edit("", "fn a() {}")], "c");

        let cap = process_cursor_hook(&real, &payload).unwrap();
        assert_eq!(cap.event.path, "lib.rs");
        assert_eq!(only_event_path(&real), "lib.rs");
    }

    #[cfg(unix)]
    #[test]
    fn cursor_symlinked_repo_is_stored_relative_4_1() {
        let (_d, _l, real, link) = repo_with_symlink();
        std::fs::write(real.join("lib.rs"), "fn a() {}").unwrap();
        let payload = cursor_payload(&real.join("lib.rs"), vec![edit("", "fn a() {}")], "c");

        let cap = process_cursor_hook(&link, &payload).unwrap();
        assert_eq!(cap.event.path, "lib.rs");
    }

    #[test]
    fn cursor_non_normalized_repo_path_is_stored_relative_4_1() {
        // Stands in for `--repo .`-style paths without changing the test
        // process's cwd (tests run in parallel).
        let (_dir, repo) = temp_repo();
        std::fs::create_dir(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/lib.rs"), "fn a() {}").unwrap();
        let payload = cursor_payload(&repo.join("sub/lib.rs"), vec![edit("", "fn a() {}")], "c");

        let cap = process_cursor_hook(&repo.join("sub/.."), &payload).unwrap();
        assert_eq!(cap.event.path, "sub/lib.rs");
    }

    #[test]
    fn cursor_file_outside_repo_is_rejected_4_1() {
        let (_dir, repo) = temp_repo();
        let elsewhere = TempDir::new().unwrap();
        let file = elsewhere.path().join("lib.rs");
        std::fs::write(&file, "fn a() {}").unwrap();

        let err = process_cursor_hook(
            &repo,
            &cursor_payload(&file, vec![edit("", "fn a() {}")], "c"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("outside the repo"), "{err:#}");
        assert!(crate::store::read_all_events(&repo).unwrap().is_empty());
    }

    // --- §6.5 gaps between events are bridged as untracked ---

    fn native(path: &str, before: &str, after: &str) -> HookPayload {
        HookPayload {
            model: "claude-opus-4-6".into(),
            tool: "cursor".into(),
            session_id: "conv-1".into(),
            path: path.into(),
            before: before.into(),
            after: after.into(),
            tool_call_id: None,
            prompt: None,
        }
    }

    /// Replay the repo and sum surviving chars per model.
    fn chars_by_model(repo: &std::path::Path) -> std::collections::HashMap<String, usize> {
        let events = crate::store::read_all_events(repo).unwrap();
        let sessions = crate::store::read_all_sessions(repo).unwrap();
        let docs = crate::replay::replay_all(&events, &|h| crate::git::read_blob(repo, h).ok());
        let mut by_model = std::collections::HashMap::new();
        for doc in &docs {
            for (eid, n) in doc.doc.composition_by_event() {
                let e = events.iter().find(|e| e.id == eid).unwrap();
                let model = &sessions
                    .iter()
                    .find(|s| s.id == e.session_id)
                    .unwrap()
                    .model;
                *by_model.entry(model.clone()).or_default() += n;
            }
        }
        by_model
    }

    #[test]
    fn hand_edit_between_ai_edits_is_untracked_6_5() {
        // The human prepends "// note\n" between two AI edits. Without a
        // bridge, replay copies the model's old spans onto the human's
        // chars and credits the model with all 28.
        let (_dir, repo) = temp_repo();
        process_hook(&repo, &native("a.rs", "", "fn a() {}\n")).unwrap();
        process_hook(
            &repo,
            &native(
                "a.rs",
                "// note\nfn a() {}\n",
                "// note\nfn a() {}\nfn b() {}\n",
            ),
        )
        .unwrap();

        let by_model = chars_by_model(&repo);
        assert_eq!(by_model.get("claude-opus-4-6"), Some(&20), "{by_model:?}");
        assert_eq!(by_model.get("untracked"), Some(&8), "{by_model:?}");
    }

    #[test]
    fn bridge_sorts_before_the_event_and_uses_own_session_6_5() {
        let (_dir, repo) = temp_repo();
        process_hook(&repo, &native("a.rs", "", "x\n")).unwrap();
        let (session, event) = process_hook(&repo, &native("a.rs", "x\ny\n", "x\ny\nz\n")).unwrap();
        assert_eq!(session.model, "claude-opus-4-6");

        let events = crate::store::read_all_events(&repo).unwrap();
        assert_eq!(events.len(), 3);
        let bridge = &events[1];
        assert_eq!(
            events[2].id, event.id,
            "bridge must sort strictly before the event"
        );
        assert_eq!(blob_text(&repo, &bridge.before_blob), "x\n");
        assert_eq!(blob_text(&repo, &bridge.after_blob), "x\ny\n");
        let sessions = crate::store::read_all_sessions(&repo).unwrap();
        let bridge_model = &sessions
            .iter()
            .find(|s| s.id == bridge.session_id)
            .unwrap()
            .model;
        assert_eq!(bridge_model, "untracked");
    }

    #[test]
    fn no_bridge_when_before_matches_last_after_6_5() {
        let (_dir, repo) = temp_repo();
        process_hook(&repo, &native("a.rs", "", "x\n")).unwrap();
        process_hook(&repo, &native("a.rs", "x\n", "x\ny\n")).unwrap();
        assert_eq!(crate::store::read_all_events(&repo).unwrap().len(), 2);
    }

    #[test]
    fn no_bridge_on_a_files_first_event_6_5() {
        let (_dir, repo) = temp_repo();
        process_hook(&repo, &native("a.rs", "existing\n", "existing\nnew\n")).unwrap();
        assert_eq!(crate::store::read_all_events(&repo).unwrap().len(), 1);
    }

    #[test]
    fn bridges_are_per_file_6_5() {
        // Another file's history mustn't count as this file's last state.
        let (_dir, repo) = temp_repo();
        process_hook(&repo, &native("a.rs", "", "a\n")).unwrap();
        process_hook(&repo, &native("b.rs", "", "b\n")).unwrap();
        process_hook(&repo, &native("a.rs", "a\n", "a\na\n")).unwrap();
        assert_eq!(crate::store::read_all_events(&repo).unwrap().len(), 3);
    }

    // --- §4.1 files git ignores are never captured ---

    fn blob_exists(repo: &std::path::Path, content: &str) -> bool {
        let hash = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["hash-object", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(content.as_bytes())?;
                c.wait_with_output()
            })
            .unwrap();
        let hash = String::from_utf8(hash.stdout).unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["cat-file", "-e", hash.trim()])
            .status()
            .unwrap()
            .success()
    }

    fn assert_skipped_ignored(err: anyhow::Error, path: &str) {
        let skip = err
            .downcast_ref::<IgnoredByGit>()
            .unwrap_or_else(|| panic!("expected IgnoredByGit, got {err:#}"));
        assert_eq!(skip.path, path);
    }

    #[test]
    fn native_hook_skips_gitignored_file_4_1() {
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        let secret = "API_KEY=sk-live-SECRET123\n";
        let payload = HookPayload {
            model: "opus".into(),
            tool: "cursor".into(),
            session_id: "s".into(),
            path: ".env".into(),
            before: "".into(),
            after: secret.into(),
            tool_call_id: None,
            prompt: None,
        };

        assert_skipped_ignored(process_hook(&repo, &payload).unwrap_err(), ".env");
        assert!(crate::store::read_all_events(&repo).unwrap().is_empty());
        assert!(
            !blob_exists(&repo, secret),
            "secret must not reach the object store"
        );
    }

    #[test]
    fn cursor_hook_skips_gitignored_file_4_1() {
        // The exact manual repro: .env in .gitignore, agent edits it.
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        let secret = "API_KEY=sk-live-SECRET123\n";
        let file = repo.join(".env");
        std::fs::write(&file, secret).unwrap();

        let err = process_cursor_hook(&repo, &cursor_payload(&file, vec![edit("", secret)], "c"))
            .unwrap_err();
        assert_skipped_ignored(err, ".env");
        assert!(crate::store::read_all_events(&repo).unwrap().is_empty());
        assert!(!blob_exists(&repo, secret));
    }

    #[test]
    fn cursor_hook_skips_file_in_gitignored_dir_4_1() {
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir(repo.join("build")).unwrap();
        let file = repo.join("build/out.js");
        std::fs::write(&file, "bundle();\n").unwrap();

        let err = process_cursor_hook(
            &repo,
            &cursor_payload(&file, vec![edit("", "bundle();\n")], "c"),
        )
        .unwrap_err();
        assert_skipped_ignored(err, "build/out.js");
    }

    #[test]
    fn ambiguous_edit_to_gitignored_file_is_skipped_not_untracked_4_1() {
        // The untracked bridge must not bypass the ignore check.
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        let file = repo.join(".env");
        std::fs::write(&file, "A=1\nA=1\n").unwrap();

        let err = process_cursor_hook(&repo, &cursor_payload(&file, vec![edit("", "A=1\n")], "c"))
            .unwrap_err();
        assert_skipped_ignored(err, ".env");
        assert!(!blob_exists(&repo, "A=1\nA=1\n"));
    }

    #[test]
    fn tracked_file_matching_ignore_pattern_is_captured_4_1() {
        // Mirrors git: a force-added file is part of history already.
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(repo.join("keep.log"), "x\n").unwrap();
        crate::git::run(&repo, &["add", "-f", "keep.log"]).unwrap();

        let file = repo.join("keep.log");
        std::fs::write(&file, "x\ny\n").unwrap();
        let cap = process_cursor_hook(
            &repo,
            &cursor_payload(&file, vec![edit("x\n", "x\ny\n")], "c"),
        )
        .unwrap();
        assert_eq!(cap.event.path, "keep.log");
    }

    #[test]
    fn native_hook_rejects_non_relative_paths_4_1() {
        let (_dir, repo) = temp_repo();
        for bad in ["", "/etc/passwd", "../a.rs", "src/../../a.rs"] {
            let payload = HookPayload {
                model: "opus".into(),
                tool: "cursor".into(),
                session_id: "s".into(),
                path: bad.into(),
                before: "".into(),
                after: "x".into(),
                tool_call_id: None,
                prompt: None,
            };
            assert!(
                process_hook(&repo, &payload).is_err(),
                "path {bad:?} must be rejected"
            );
        }
        assert!(crate::store::read_all_events(&repo).unwrap().is_empty());
    }

    #[test]
    fn cursor_ambiguous_edit_recorded_as_untracked_4_1() {
        // I4: an unplaceable edit isn't credited to the model. It's recorded
        // as `untracked`, bridging from the last recorded state to the disk.
        let (_dir, repo) = temp_repo();
        let file = repo.join("lib.rs");
        let first = "fn a() {\n}\n";
        std::fs::write(&file, first).unwrap();
        process_cursor_hook(
            &repo,
            &cursor_payload(&file, vec![edit("", first)], "conv-1"),
        )
        .unwrap();

        let second = "fn a() {\n}\nfn b() {\n}\n";
        std::fs::write(&file, second).unwrap();
        let cap = process_cursor_hook(&repo, &cursor_payload(&file, vec![edit("", "}")], "conv-1"))
            .unwrap();

        assert_eq!(cap.session.model, "untracked");
        assert!(
            cap.warning.as_deref().is_some_and(|w| w.contains("lib.rs")),
            "{:?}",
            cap.warning
        );
        assert_eq!(blob_text(&repo, &cap.event.before_blob), first);
        assert_eq!(blob_text(&repo, &cap.event.after_blob), second);

        // The real conversation's session keeps its model.
        let sessions = crate::store::read_all_sessions(&repo).unwrap();
        assert!(sessions.iter().any(|s| s.model == "claude-opus-4-6"));
        assert!(sessions.iter().any(|s| s.model == "untracked"));
    }

    #[test]
    fn cursor_ambiguous_edit_without_history_starts_from_empty_4_1() {
        let (_dir, repo) = temp_repo();
        let file = repo.join("lib.rs");
        std::fs::write(&file, "}\n}\n").unwrap();
        let cap = process_cursor_hook(&repo, &cursor_payload(&file, vec![edit("", "}")], "conv-2"))
            .unwrap();

        assert_eq!(cap.session.model, "untracked");
        assert_eq!(blob_text(&repo, &cap.event.before_blob), "");
        assert_eq!(blob_text(&repo, &cap.event.after_blob), "}\n}\n");
    }

    #[test]
    fn cursor_ambiguous_edit_replay_attributes_nothing_to_model_4_1() {
        // End to end: after an unplaceable edit, none of the new chars are
        // credited to the model, and the earlier model chars survive.
        let (_dir, repo) = temp_repo();
        let file = repo.join("lib.rs");
        std::fs::write(&file, "fn a() {\n}\n").unwrap();
        process_cursor_hook(
            &repo,
            &cursor_payload(&file, vec![edit("", "fn a() {\n}\n")], "c"),
        )
        .unwrap();
        std::fs::write(&file, "fn a() {\n}\nfn b() {\n}\n").unwrap();
        process_cursor_hook(&repo, &cursor_payload(&file, vec![edit("", "}")], "c")).unwrap();

        let events = crate::store::read_all_events(&repo).unwrap();
        let sessions = crate::store::read_all_sessions(&repo).unwrap();
        let model_of = |e: &Event| {
            sessions
                .iter()
                .find(|s| s.id == e.session_id)
                .unwrap()
                .model
                .clone()
        };
        let docs = crate::replay::replay_all(&events, &|h| crate::git::read_blob(&repo, h).ok());
        let mut by_model: std::collections::HashMap<String, usize> = Default::default();
        for (eid, n) in docs[0].doc.composition_by_event() {
            let e = events.iter().find(|e| e.id == eid).unwrap();
            *by_model.entry(model_of(e)).or_default() += n;
        }
        assert_eq!(by_model.get("claude-opus-4-6"), Some(&11)); // "fn a() {\n}\n"
        assert_eq!(by_model.get("untracked"), Some(&11)); // "fn b() {\n}\n"
    }

    #[test]
    fn cursor_to_hook_payload_4_1() {
        // §4.1: CursorPayload converts to HookPayload with correct fields
        let cursor = CursorPayload {
            file_path: "/home/user/project/src/lib.rs".into(),
            edits: vec![CursorEdit {
                old_string: "old".into(),
                new_string: "new".into(),
            }],
            model: "claude-opus-4-6".into(),
            conversation_id: "conv-session-42".into(),
            generation_id: Some("gen-7".into()),
            hook_event_name: Some("afterFileEdit".into()),
            cursor_version: Some("1.0.0".into()),
            workspace_roots: Some(vec!["/home/user/project".into()]),
            model_id: None,
            model_params: None,
            user_email: None,
            transcript_path: None,
        };
        let after_content = "this is new content";

        let hook =
            cursor.to_hook_payload("this is old content".into(), after_content, "src/lib.rs");
        assert_eq!(hook.model, "claude-opus-4-6");
        assert_eq!(hook.tool, "cursor");
        assert_eq!(hook.session_id, "conv-session-42");
        assert_eq!(hook.path, "src/lib.rs");
        assert_eq!(hook.after, "this is new content");
    }

    #[test]
    fn cursor_process_end_to_end_4_1() {
        // §4.1: full pipeline — Cursor payload → blobs + event in store
        let (_dir, repo) = temp_repo();

        // Write the "after" file to disk (Cursor already edited it)
        let src_dir = repo.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() { hello_world(); }").unwrap();

        let cursor = CursorPayload {
            file_path: src_dir.join("main.rs").to_string_lossy().into(),
            edits: vec![CursorEdit {
                old_string: "hello()".into(),
                new_string: "hello_world()".into(),
            }],
            model: "claude-opus-4-6".into(),
            conversation_id: "conv-e2e-test".into(),
            generation_id: Some("gen-1".into()),
            hook_event_name: Some("afterFileEdit".into()),
            cursor_version: None,
            workspace_roots: None,
            model_id: None,
            model_params: None,
            user_email: None,
            transcript_path: None,
        };

        let CursorCapture {
            session,
            event,
            warning,
        } = process_cursor_hook(&repo, &cursor).unwrap();
        assert_eq!(warning, None);
        assert_eq!(session.model, "claude-opus-4-6");
        assert_eq!(event.path, "src/main.rs");

        // Verify before blob contains the reconstructed original
        let before_bytes = crate::git::read_blob(&repo, &event.before_blob).unwrap();
        assert_eq!(
            String::from_utf8(before_bytes).unwrap(),
            "fn main() { hello(); }"
        );

        // Verify after blob matches file on disk
        let after_bytes = crate::git::read_blob(&repo, &event.after_blob).unwrap();
        assert_eq!(
            String::from_utf8(after_bytes).unwrap(),
            "fn main() { hello_world(); }"
        );

        // Verify stored in event log
        let events = crate::store::read_all_events(&repo).unwrap();
        assert_eq!(events.len(), 1);
    }
}
