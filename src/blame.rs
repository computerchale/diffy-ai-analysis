//! Blame-join engine — approximate, line-level attribution for history
//! that predates instrumentation. See SPEC.md §7.
//!
//! Joins `git blame --porcelain -M -C -w` output against per-commit
//! provenance notes (refs/notes/provenance). Also the interop/export
//! path: this is the same join git-ai-style tooling performs, so our
//! notes stay compatible with the emerging standard.
//!
//! Note format (§5, git-ai `authorship/3.0.0`): two sections split by a
//! literal `---` line — an attestation section of file-path headers, each
//! followed by indented `  <hash>  <line-ranges>` entries, then a JSON
//! metadata section with `sessions` and `prompts` maps. Invariant I1:
//! lines with no note (or attributed to a human) yield `model: None`.

use crate::git;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// One blamed line joined with its attribution, if any.
#[derive(Debug)]
pub struct BlamedLine {
    pub line_no: usize,
    pub commit: String,
    /// Model slug from the commit's provenance note, if one exists.
    pub model: Option<String>,
    pub content: String,
}

/// A parsed provenance note (git-ai `authorship/3.0.0` shape).
#[derive(Debug, Clone)]
struct ParsedNote {
    /// file path → (line number → attestation hash)
    files: HashMap<String, HashMap<usize, String>>,
    /// session key (e.g. `s_0123456789abcd`) → model
    sessions: HashMap<String, String>,
    /// legacy prompt hash (16-hex) → model
    prompts: HashMap<String, String>,
}

/// Run git blame (with move/copy detection) on a path, then join each line
/// against its commit's provenance note to recover the authoring model.
pub fn blame_join(repo: &Path, path: &str) -> Result<Vec<BlamedLine>> {
    let porcelain = git::blame_porcelain(repo, path)?;
    let mut lines = parse_blame_porcelain(&porcelain)?;

    // Cache parsed notes per commit — many lines share a commit.
    let mut note_cache: HashMap<String, Option<ParsedNote>> = HashMap::new();

    for line in &mut lines {
        let parsed = match note_cache.get(&line.commit) {
            Some(p) => p.clone(),
            None => {
                let note = git::read_note(repo, "provenance", &line.commit)?;
                let parsed = note.as_deref().map(parse_note);
                note_cache.insert(line.commit.clone(), parsed.clone());
                parsed
            }
        };
        if let Some(note) = parsed {
            let model = note
                .files
                .get(path)
                .and_then(|l| l.get(&line.line_no))
                .cloned()
                .and_then(|hash| resolve_model(&hash, &note));
            line.model = model;
        }
    }

    Ok(lines)
}

/// Resolve an attestation hash to a model slug. `s_…::t_…` session keys map
/// through the `sessions` table; `h_…` human keys and unknown hashes never
/// resolve to a model (I1 — we don't guess).
fn resolve_model(hash: &str, note: &ParsedNote) -> Option<String> {
    if hash.starts_with("s_") {
        // "s_<id>::t_<trace>" → session key "s_<id>"
        let key = hash.split("::").next().unwrap_or(hash);
        note.sessions.get(key).cloned()
    } else if hash.starts_with("h_") {
        None
    } else {
        note.prompts.get(hash).cloned()
    }
}

/// Parse `git blame --porcelain` output into per-line records.
///
/// Handles grouped headers: a commit owning N consecutive lines emits one
/// header line `<sha> <orig> <final> <N>` followed by N tab-prefixed
/// content lines. Attribute lines (`author`, `committer`, `summary`,
/// `previous`, `boundary`, `filename`) are skipped.
fn parse_blame_porcelain(porcelain: &str) -> Result<Vec<BlamedLine>> {
    let mut lines = Vec::new();
    let mut commit: Option<String> = None;
    let mut line_no: usize = 0;
    let mut remaining: usize = 0;

    for raw in porcelain.lines() {
        if let Some(content) = raw.strip_prefix('\t') {
            anyhow::ensure!(
                remaining > 0,
                "blame content line without a preceding header"
            );
            lines.push(BlamedLine {
                line_no,
                commit: commit.clone().context("missing commit for blame line")?,
                model: None,
                content: content.to_string(),
            });
            line_no += 1;
            remaining -= 1;
        } else if let Some(tok) = raw.split_whitespace().next()
            && tok.len() == 40
            && tok.chars().all(|c| c.is_ascii_hexdigit())
        {
            commit = Some(tok.to_string());
            let mut parts = raw.split_whitespace().skip(1);
            let _orig = parts.next();
            line_no = parts
                .next()
                .and_then(|s| s.parse().ok())
                .context("blame header missing final line number")?;
            remaining = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        }
    }

    Ok(lines)
}

/// Parse a provenance note into per-file line attestation plus the
/// session/prompt hash → model lookups.
///
/// The attestation section is a sequence of file-path headers (unindented,
/// optionally quoted) each followed by indented `  <hash>  <ranges>` lines.
/// The metadata section (`sessions` / `prompts` maps) carries the model
/// under `agent_id.model` for each key.
fn parse_note(note: &str) -> ParsedNote {
    let mut files: HashMap<String, HashMap<usize, String>> = HashMap::new();
    let mut sessions: HashMap<String, String> = HashMap::new();
    let mut prompts: HashMap<String, String> = HashMap::new();

    let mut after_sep = false;
    let mut metadata = String::new();
    let mut current_file: Option<String> = None;

    for raw in note.lines() {
        if !after_sep {
            let trimmed = raw.trim();
            if trimmed == "---" {
                after_sep = true;
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            if let Some(entry) = raw.strip_prefix("  ") {
                // indented attestation entry: "<hash> <ranges>"
                if let (Some(file), Some((hash, ranges))) =
                    (current_file.as_ref(), entry.split_once(' '))
                {
                    let map = files.entry(file.clone()).or_default();
                    for n in expand_ranges(ranges.trim()) {
                        map.insert(n, hash.trim().to_string());
                    }
                }
            } else {
                // file path header (may be quoted for spaces)
                current_file = Some(trimmed.trim_matches('"').to_string());
            }
        } else {
            metadata.push_str(raw);
            metadata.push('\n');
        }
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&metadata) {
        for (target, field) in [(&mut sessions, "sessions"), (&mut prompts, "prompts")] {
            if let Some(obj) = value.get(field).and_then(|s| s.as_object()) {
                for (key, rec) in obj {
                    if let Some(model) = rec
                        .get("agent_id")
                        .and_then(|a| a.get("model"))
                        .and_then(|m| m.as_str())
                    {
                        target.insert(key.clone(), model.to_string());
                    }
                }
            }
        }
    }

    ParsedNote {
        files,
        sessions,
        prompts,
    }
}

/// Expand a comma-separated inclusive range string like `"1-4,9"` into the
/// list of line numbers it covers.
fn expand_ranges(ranges: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in ranges.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (a.parse::<usize>(), b.parse::<usize>()) {
                out.extend(start..=end);
            }
        } else if let Ok(n) = part.parse::<usize>() {
            out.push(n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        crate::git::init(&path).unwrap();
        crate::git::run(&path, &["config", "user.name", "diffy-test"]).unwrap();
        crate::git::run(&path, &["config", "user.email", "diffy-test@example.com"]).unwrap();
        (dir, path)
    }

    /// A git-ai-shaped metadata section: `sessions` is a map keyed by
    /// `s_<id>` with the model under `agent_id.model`.
    const SESSIONS_JSON: &str = r#"{
  "schema_version": "authorship/3.0.0",
  "base_commit_sha": "",
  "prompts": {},
  "sessions": {
    "s_0123456789abcd": { "agent_id": { "tool": "cursor", "id": "conv-1", "model": "opus-4.6" } },
    "s_55555555555555": { "agent_id": { "tool": "cursor", "id": "conv-2", "model": "gpt-5.2" } }
  }
}"#;

    #[test]
    fn parse_blame_porcelain_extracts_lines_7() {
        // §7: porcelain output (grouped commits) parses to per-line records
        let porcelain = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 2\n\
                         author Alice\n\
                         author-mail <a@example.com>\n\
                         author-time 1600000000\n\
                         author-tz +0000\n\
                         committer Alice\n\
                         committer-mail <a@example.com>\n\
                         committer-time 1600000000\n\
                         committer-tz +0000\n\
                         summary first commit\n\
                         filename f.txt\n\
                         \tone\n\
                         \ttwo\n\
                         bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2 3\n\
                         author Bob\n\
                         author-mail <b@example.com>\n\
                         author-time 1600000001\n\
                         author-tz +0000\n\
                         committer Bob\n\
                         committer-mail <b@example.com>\n\
                         committer-time 1600000001\n\
                         committer-tz +0000\n\
                         summary second commit\n\
                         filename f.txt\n\
                         \tthree\n";

        let lines = parse_blame_porcelain(porcelain).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_no, 1);
        assert_eq!(lines[0].commit, "a".repeat(40));
        assert_eq!(lines[0].content, "one");
        assert_eq!(lines[1].line_no, 2);
        assert_eq!(lines[1].commit, "a".repeat(40));
        assert_eq!(lines[1].content, "two");
        assert_eq!(lines[2].line_no, 3);
        assert_eq!(lines[2].commit, "b".repeat(40));
        assert_eq!(lines[2].content, "three");
    }

    #[test]
    fn parse_note_parses_git_ai_format_7() {
        // §7: real git-ai authorship/3.0.0 note — file headers, s_/h_ keys,
        // and a `sessions` map (not array)
        let note = format!(
            "src/a.rs\n  s_0123456789abcd::t_fedcba98765432  1-2\n  h_human1  3\n\
             src/b.rs\n  s_55555555555555::t_aaaaaaaaaaaaaa  1\n---\n{SESSIONS_JSON}\n"
        );

        let parsed = parse_note(&note);
        assert_eq!(
            parsed.sessions.get("s_0123456789abcd"),
            Some(&"opus-4.6".to_string())
        );
        assert_eq!(
            parsed.sessions.get("s_55555555555555"),
            Some(&"gpt-5.2".to_string())
        );
        let a = parsed.files.get("src/a.rs").expect("src/a.rs attestation");
        assert_eq!(
            a.get(&1),
            Some(&"s_0123456789abcd::t_fedcba98765432".to_string())
        );
        assert_eq!(
            a.get(&2),
            Some(&"s_0123456789abcd::t_fedcba98765432".to_string())
        );
        assert_eq!(a.get(&3), Some(&"h_human1".to_string()));
        let b = parsed.files.get("src/b.rs").expect("src/b.rs attestation");
        assert_eq!(
            b.get(&1),
            Some(&"s_55555555555555::t_aaaaaaaaaaaaaa".to_string())
        );
    }

    #[test]
    fn resolve_model_routes_session_and_human_7() {
        // §7: s_ hashes resolve via sessions, h_ hashes never resolve (I1)
        let parsed = parse_note(&format!("---\n{SESSIONS_JSON}"));
        assert_eq!(
            resolve_model("s_0123456789abcd::t_xyz", &parsed),
            Some("opus-4.6".to_string())
        );
        assert_eq!(resolve_model("h_human1", &parsed), None);
        assert_eq!(resolve_model("deadbeefdeadbeef", &parsed), None);
    }

    /// `git commit` with an explicit identity, which CI runners don't have.
    fn test_commit(repo: &std::path::Path) {
        crate::git::run(
            repo,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "init",
            ],
        )
        .unwrap();
    }

    #[test]
    fn blame_join_returns_models_from_notes_7() {
        // §7: blame output joined against provenance notes yields a model
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join("f.txt"), "one\ntwo\n").unwrap();
        crate::git::run(&repo, &["add", "f.txt"]).unwrap();
        test_commit(&repo);
        let head = crate::git::head(&repo).unwrap().unwrap();

        let note =
            format!("f.txt\n  s_0123456789abcd::t_fedcba98765432  1-2\n---\n{SESSIONS_JSON}");
        crate::git::write_note(&repo, "provenance", &head, &note).unwrap();

        let lines = blame_join(&repo, "f.txt").unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].model.as_deref(), Some("opus-4.6"));
        assert_eq!(lines[1].model.as_deref(), Some("opus-4.6"));
    }

    #[test]
    fn blame_join_lines_without_notes_have_no_model_7() {
        // §7: no provenance note → model is None (never guessed, I1)
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join("f.txt"), "one\ntwo\n").unwrap();
        crate::git::run(&repo, &["add", "f.txt"]).unwrap();
        test_commit(&repo);

        let lines = blame_join(&repo, "f.txt").unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.model.is_none()));
    }
}
