//! Static HTML report generation. See SPEC.md §9.
//!
//! `diffy report` emits one self-contained HTML file with all data, JS,
//! and CSS embedded. No webserver — open the file in a browser.
//!
//! Layout (§9.1): file explorer (left pane, like coverage.py) +
//! character grid (right pane — every character is a colored cell, one
//! cell per char/space, colored by the model that authored it).
//!
//! Playback (§9.2): play/pause + timeline slider replays the event log
//! chronologically — 1 event/sec default, adjustable 0.1×–20×. The grid
//! animates as edits land and colors shift per model.
//!
//! Data source (§9.3): reads exclusively from git provenance refs —
//! `refs/provenance/events/*`, `refs/notes/provenance`, blobs. All
//! serialized into one embedded JSON blob inside the HTML.

use crate::{git, modules, replay, stats, store};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

/// Per-file data for the report JSON.
#[derive(Serialize)]
struct FileReport {
    path: String,
    /// The current file content (for rendering the grid).
    content: String,
    /// Per-character model attribution: one entry per char in `content`.
    /// Empty when `all_untracked` is true (the whole file has no provenance).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    char_models: Vec<String>,
    /// Per-character session attribution: the raw agent conversation id that
    /// authored each char (parallel to `char_models`). Empty when
    /// `all_untracked` is true. Lets the grid tooltip link a char back to
    /// the exact Cursor conversation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    char_sessions: Vec<String>,
    /// Composition: model → surviving chars in this file.
    composition: HashMap<String, usize>,
    total_chars: usize,
    /// True when the file on disk differs from replay state (§6.5 drift).
    /// Untracked chars are attributed to "untracked".
    drifted: bool,
    /// True when the file has no provenance at all — every char is untracked.
    /// Rendered from a flag instead of a per-char array (§9.3 size guard).
    all_untracked: bool,
}

/// Per-event data for playback.
#[derive(Serialize)]
struct EventReport {
    id: String,
    path: String,
    model: String,
    before: String,
    after: String,
    ts: String,
}

/// Top-level report data embedded in the HTML.
#[derive(Serialize)]
struct ReportData {
    files: Vec<FileReport>,
    events: Vec<EventReport>,
    /// Repo-wide composition: model → total chars.
    composition: HashMap<String, usize>,
    total_chars: usize,
    /// All models seen, with assigned colors.
    model_colors: HashMap<String, String>,
    /// §9.4 treemap: per-file area + dominant model.
    treemap: Vec<TreemapEntry>,
    /// §9.4 stratigraphy: cumulative surviving chars per model over time.
    stratigraphy: Vec<stats::StratigraphyPoint>,
    /// §9.4 heatmap: model × directory affinity.
    heatmap: Vec<HeatmapEntry>,
    /// §9.4 half-life: per-model survival curves.
    half_life: Vec<HalfLifeCurve>,
}

/// Treemap leaf: one file's area (chars) and its dominant model.
#[derive(Serialize)]
struct TreemapEntry {
    path: String,
    chars: usize,
    model: Option<String>,
}

/// Heatmap cell: surviving chars a model owns in a top-level directory.
#[derive(Serialize)]
struct HeatmapEntry {
    model: String,
    dir: String,
    chars: usize,
}

/// Per-model half-life survival curve.
#[derive(Serialize)]
struct HalfLifeCurve {
    model: String,
    points: Vec<stats::SurvivalPoint>,
}

/// Distinct colors for models — enough for typical usage.
const COLORS: &[&str] = &[
    "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948", "#b07aa1", "#ff9da7",
    "#9c755f", "#bab0ac", "#af7aa1", "#86bcb6", "#d37295", "#8cd17d", "#b6992d",
];

/// Generate the full report JSON from a repo's provenance data.
///
/// `include_sessions` controls whether per-character session/conversation
/// ids are embedded. When false (§9.3 privacy switch), `char_sessions` is
/// left empty so the ids never leave the machine.
fn build_report_data(repo: &Path, include_sessions: bool) -> Result<ReportData> {
    let events = store::read_all_events(repo)?;
    let sessions = store::read_all_sessions(repo)?;

    // session_id → model
    let session_models: HashMap<ulid::Ulid, String> =
        sessions.iter().map(|s| (s.id, s.model.clone())).collect();

    // event_id → model
    let event_models: HashMap<ulid::Ulid, String> = events
        .iter()
        .map(|e| {
            let model = session_models
                .get(&e.session_id)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            (e.id, model)
        })
        .collect();

    // session_id → raw conversation id (fall back to the ULID when absent).
    let session_conversations: HashMap<ulid::Ulid, String> = sessions
        .iter()
        .map(|s| {
            (
                s.id,
                s.conversation_id
                    .clone()
                    .unwrap_or_else(|| s.id.to_string()),
            )
        })
        .collect();

    // event_id → conversation id (empty string when the session is unknown).
    let event_sessions: HashMap<ulid::Ulid, String> = events
        .iter()
        .map(|e| {
            (
                e.id,
                session_conversations
                    .get(&e.session_id)
                    .cloned()
                    .unwrap_or_default(),
            )
        })
        .collect();

    // Assign colors to models (deferred until after file scan so
    // "untracked" is included if any drift is detected)

    // Replay to get per-file provenance
    let blob_reader = |hash: &str| -> Option<Vec<u8>> { git::read_blob(repo, hash).ok() };

    let file_compositions = replay::replay_all(&events, &blob_reader);

    // Surviving chars per event (§8).
    let surviving_by_event = replay::surviving_by_event(&file_compositions);

    // Build per-file reports
    let mut repo_composition: HashMap<String, usize> = HashMap::new();
    let mut repo_total: usize = 0;

    let mut file_reports: Vec<FileReport> = Vec::new();

    for fc in &file_compositions {
        // Read the actual file from disk (§6.5: source of truth)
        let disk_content = std::fs::read_to_string(repo.join(&fc.path)).unwrap_or_default();

        // Also get the replay's last-known content from the after_blob
        let replay_content = events
            .iter()
            .rev()
            .find(|e| e.path == fc.path && !e.transparent)
            .and_then(|e| git::read_blob(repo, &e.after_blob).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();

        // §6.5: detect drift — disk differs from replay state
        let drifted = disk_content != replay_content;

        // Use disk content for display; use replay spans for attribution.
        // If drifted, run a char-level diff between replay content and disk
        // content to map replay spans onto the actual file, marking new
        // chars as "untracked".
        let (content, char_models, char_sessions) = if drifted && !disk_content.is_empty() {
            reconcile_with_disk(
                &fc.doc,
                &replay_content,
                &disk_content,
                &event_models,
                &event_sessions,
            )
        } else {
            // No drift — replay spans map 1:1 to content
            let content = if disk_content.is_empty() {
                replay_content
            } else {
                disk_content
            };
            let (mut models, mut sessions) =
                expand_spans(&fc.doc.spans, &event_models, &event_sessions);
            models.truncate(content.len());
            sessions.truncate(content.len());
            while models.len() < content.len() {
                models.push("unknown".into());
            }
            while sessions.len() < content.len() {
                sessions.push(String::new());
            }
            (content, models, sessions)
        };

        // §9.3 privacy switch: drop per-char session ids when disabled so
        // they never leave the machine.
        let char_sessions = if include_sessions {
            char_sessions
        } else {
            Vec::new()
        };

        // Per-file composition
        let mut file_comp: HashMap<String, usize> = HashMap::new();
        for model in &char_models {
            *file_comp.entry(model.clone()).or_insert(0) += 1;
        }

        let total = content.len();
        for (m, c) in &file_comp {
            *repo_composition.entry(m.clone()).or_insert(0) += c;
        }
        repo_total += total;

        file_reports.push(FileReport {
            path: fc.path.clone(),
            content,
            char_models,
            char_sessions,
            composition: file_comp,
            total_chars: total,
            drifted,
            all_untracked: false,
        });
    }

    // Also include files on disk that have NO provenance events at all
    // (they exist in the repo but were never tracked by diffy)
    let tracked_paths: std::collections::HashSet<String> =
        file_reports.iter().map(|f| f.path.clone()).collect();
    if let Ok(entries) = walk_repo_files(repo) {
        for rel_path in entries {
            if tracked_paths.contains(&rel_path) {
                continue;
            }
            let disk_content = std::fs::read_to_string(repo.join(&rel_path)).unwrap_or_default();
            if disk_content.is_empty() {
                continue;
            }
            let total = disk_content.len();
            let mut file_comp = HashMap::new();
            file_comp.insert("untracked".to_string(), total);
            repo_total += total;
            *repo_composition.entry("untracked".to_string()).or_insert(0) += total;
            file_reports.push(FileReport {
                path: rel_path,
                content: disk_content,
                char_models: Vec::new(),
                char_sessions: Vec::new(),
                composition: file_comp,
                total_chars: total,
                drifted: true,
                all_untracked: true,
            });
        }
    }

    file_reports.sort_by(|a, b| a.path.cmp(&b.path));

    // --- §9.4 analytics ---

    // Treemap: per-file area (chars) + dominant model.
    let treemap: Vec<TreemapEntry> = file_reports
        .iter()
        .map(|f| TreemapEntry {
            path: f.path.clone(),
            chars: f.total_chars,
            model: stats::dominant_model(&f.composition),
        })
        .collect();

    // Heatmap: model × directory affinity, from per-file composition.
    let mut affinity_entries: Vec<(String, String, usize)> = Vec::new();
    for f in &file_reports {
        for (model, chars) in &f.composition {
            affinity_entries.push((f.path.clone(), model.clone(), *chars));
        }
    }
    let heatmap: Vec<HeatmapEntry> = stats::affinity(&affinity_entries)
        .into_iter()
        .map(|((model, dir), chars)| HeatmapEntry { model, dir, chars })
        .collect();

    // Stratigraphy: cumulative surviving chars per model over time.
    let mut chrono_events: Vec<&crate::model::Event> =
        events.iter().filter(|e| !e.transparent).collect();
    chrono_events.sort_by_key(|e| e.ts);
    let strat_rows: Vec<(i64, String, usize)> = chrono_events
        .iter()
        .map(|e| {
            let model = event_models
                .get(&e.id)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            let surviving = surviving_by_event.get(&e.id).copied().unwrap_or(0);
            (e.ts.as_second(), model, surviving)
        })
        .collect();
    let stratigraphy = stats::stratigraphy(&strat_rows);

    // Half-life: per-model survival curves.
    let now_seconds = jiff::Timestamp::now().as_second();
    let buckets = stats::build_emission_buckets(
        &events,
        &session_models,
        &surviving_by_event,
        blob_reader,
        now_seconds,
    );
    let half_life: Vec<HalfLifeCurve> = stats::half_life_by_model(&buckets)
        .into_iter()
        .map(|(model, points)| HalfLifeCurve { model, points })
        .collect();

    // Assign colors to all models (including "untracked" if drift detected)
    let mut all_models: Vec<String> = repo_composition.keys().cloned().collect();
    all_models.sort();
    all_models.dedup();
    let mut model_colors: HashMap<String, String> = all_models
        .iter()
        .filter(|m| m.as_str() != "untracked")
        .enumerate()
        .map(|(i, m)| (m.clone(), COLORS[i % COLORS.len()].to_string()))
        .collect();
    // "untracked" always gets a muted gray
    if all_models.contains(&"untracked".to_string()) {
        model_colors.insert("untracked".into(), "#585b70".into());
    }

    // Build event timeline for playback
    let event_reports: Vec<EventReport> = events
        .iter()
        .filter(|e| !e.transparent)
        .map(|e| {
            let model = event_models
                .get(&e.id)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            let before = git::read_blob(repo, &e.before_blob)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let after = git::read_blob(repo, &e.after_blob)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            EventReport {
                id: e.id.to_string(),
                path: e.path.clone(),
                model,
                before,
                after,
                ts: e.ts.to_string(),
            }
        })
        .collect();

    Ok(ReportData {
        files: file_reports,
        events: event_reports,
        composition: repo_composition,
        total_chars: repo_total,
        model_colors,
        treemap,
        stratigraphy,
        heatmap,
        half_life,
    })
}

/// Escape a JSON string so it can be embedded inside an HTML `<script>`
/// block without a `</script>` sequence breaking out of the script (§9).
/// The JSON spec allows `\/` as an escape for `/`, so `<\/script>` is a
/// lossless, safe substitution.
fn escape_json_for_html(json: &str) -> String {
    json.replace("</script>", "<\\/script>")
}

/// Write a self-contained HTML report file.
pub fn write_report(repo: &Path, out: &Path, include_sessions: bool) -> Result<()> {
    let data = build_report_data(repo, include_sessions)?;
    let json = serde_json::to_string(&data).context("failed to serialize report data")?;

    let safe_json = escape_json_for_html(&json);
    let html = TEMPLATE.replace("__DATA__", &safe_json);
    std::fs::write(out, &html)
        .with_context(|| format!("failed to write report to {}", out.display()))?;

    Ok(())
}

/// Emit report data as JSON to stdout.
pub fn write_json(repo: &Path, include_sessions: bool) -> Result<()> {
    let data = build_report_data(repo, include_sessions)?;
    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}

/// Build the C1 stable module-provenance export (SPEC.md §9.5).
///
/// Reuses [`build_report_data`] so `repo_total_chars` is the exact repo total
/// B1 reconciles against, and so the module input set is the same replayed ∪
/// untracked union that feeds the flat report. The export carries **no**
/// session/conversation identifiers — only aggregate counts — so `--no-session`
/// is a documented no-op here (§9.5 C1).
pub fn build_module_export(repo: &Path) -> Result<modules::ModuleExport> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let data = build_report_data(&repo, false)?;

    let files: Vec<modules::ModuleFile> = data
        .files
        .iter()
        .map(|f| modules::ModuleFile {
            path: f.path.clone(),
            total_chars: f.total_chars,
            composition: f.composition.clone(),
            drifted: f.drifted,
            all_untracked: f.all_untracked,
        })
        .collect();

    let modules = modules::build_module_reports(&repo, &files);
    Ok(modules::ModuleExport {
        schema_version: modules::MODULE_SCHEMA_VERSION.to_string(),
        generated_at: jiff::Timestamp::now().to_string(),
        repo_total_chars: data.total_chars,
        modules,
    })
}

/// Serialize the module export to pretty JSON (§9.5 C1).
pub fn module_export_json(repo: &Path) -> Result<String> {
    let export = build_module_export(repo)?;
    Ok(serde_json::to_string_pretty(&export)?)
}

/// Write the module export JSON to a writer (stdout in production, §9.5 C1).
pub fn write_module_json(repo: &Path, w: &mut impl Write) -> Result<()> {
    let json = module_export_json(repo)?;
    writeln!(w, "{}", json)?;
    Ok(())
}

/// Write the module export JSON to a file (§9.5 C1 `--out <file>`).
pub fn write_module_report(repo: &Path, out: &Path) -> Result<()> {
    let json = module_export_json(repo)?;
    std::fs::write(out, format!("{json}\n"))
        .with_context(|| format!("failed to write module report to {}", out.display()))?;
    Ok(())
}

/// Expand replay spans into parallel per-character model + session arrays.
fn expand_spans(
    spans: &[replay::Span],
    event_models: &HashMap<ulid::Ulid, String>,
    event_sessions: &HashMap<ulid::Ulid, String>,
) -> (Vec<String>, Vec<String>) {
    let total: usize = spans.iter().map(|s| s.len).sum();
    let mut models = Vec::with_capacity(total);
    let mut sessions = Vec::with_capacity(total);
    for span in spans {
        let model = replay::model_for_origin(span.origin, event_models);
        let session = event_sessions
            .get(&span.origin)
            .cloned()
            .unwrap_or_default();
        for _ in 0..span.len {
            models.push(model.clone());
            sessions.push(session.clone());
        }
    }
    (models, sessions)
}

/// Reconcile replay spans against the actual file on disk when they differ.
/// Runs a char-level diff between the replay's output and the disk content,
/// preserving provenance for equal chars and marking insertions as "untracked".
fn reconcile_with_disk(
    doc: &replay::ProvenanceDoc,
    replay_content: &str,
    disk_content: &str,
    event_models: &HashMap<ulid::Ulid, String>,
    event_sessions: &HashMap<ulid::Ulid, String>,
) -> (String, Vec<String>, Vec<String>) {
    use similar::TextDiff;

    // Build flat attribution arrays for the replay content (models + sessions).
    let (replay_models, replay_sessions) = expand_spans(&doc.spans, event_models, event_sessions);

    // Char-level diff: replay (old) → disk (new)
    let diff = TextDiff::from_chars(replay_content, disk_content);
    let mut disk_models: Vec<String> = Vec::with_capacity(disk_content.len());
    let mut disk_sessions: Vec<String> = Vec::with_capacity(disk_content.len());

    for op in diff.ops() {
        match op {
            similar::DiffOp::Equal { old_index, len, .. } => {
                for i in 0..*len {
                    let idx = old_index + i;
                    if idx < replay_models.len() {
                        disk_models.push(replay_models[idx].clone());
                        disk_sessions.push(replay_sessions[idx].clone());
                    } else {
                        disk_models.push("untracked".into());
                        disk_sessions.push(String::new());
                    }
                }
            }
            similar::DiffOp::Insert {
                new_index: _,
                new_len,
                ..
            } => {
                for _ in 0..*new_len {
                    disk_models.push("untracked".into());
                    disk_sessions.push(String::new());
                }
            }
            similar::DiffOp::Delete { .. } => {}
            similar::DiffOp::Replace { new_len, .. } => {
                for _ in 0..*new_len {
                    disk_models.push("untracked".into());
                    disk_sessions.push(String::new());
                }
            }
        }
    }

    while disk_models.len() < disk_content.len() {
        disk_models.push("untracked".into());
        disk_sessions.push(String::new());
    }

    (disk_content.to_string(), disk_models, disk_sessions)
}

/// Walk the repo's tracked files (non-.git, non-binary).
fn walk_repo_files(repo: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    walk_dir(repo, repo, &mut files)?;
    Ok(files)
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip VCS, dependency, build, and cache directories. These are
        // never authored source — they bloat the report (a .mypy_cache can
        // hold thousands of JSON files) and, in the case of .envs/, can
        // carry secrets (§11).
        if SKIP_DIRS.contains(&name_str.as_ref()) {
            continue;
        }
        if path.is_dir() {
            walk_dir(root, &path, out)?;
        } else if path.is_file() {
            // Skip secret files (§11): anything named .env* and the .aurora
            // credential file diffy's own tooling writes.
            if name_str.starts_with(".env") || name_str == ".aurora" {
                continue;
            }
            // Skip non-source files: HTML reports, binaries, images, etc.
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            match ext {
                "html" | "htm" | "png" | "jpg" | "jpeg" | "gif" | "svg" | "woff" | "woff2"
                | "ttf" | "ico" | "pdf" | "zip" | "tar" | "gz" | "lock" | "bin" | "exe" | "dll"
                | "so" | "dylib" | "o" | "a" => continue,
                _ => {}
            }
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().into_owned();
                out.push(rel_str);
            }
        }
    }
    Ok(())
}

/// Directories that are never source and must be excluded from the scan.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".cursor",
    "target",
    "node_modules",
    // Python
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "__pycache__",
    ".venv",
    "venv",
    // JS / frontend build artifacts
    ".nuxt",
    ".output",
    ".next",
    ".turbo",
    "dist",
    "build",
    // Secrets / runtime state
    ".envs",
    // Misc caches
    ".cache",
    ".data",
    ".idea",
    ".vscode",
];

const TEMPLATE: &str = include_str!("../assets/report.html");

#[cfg(test)]
#[allow(non_snake_case)] // test names cite the feature id (`_B1`/`_C1`) per SPEC.md §9.5
mod tests {
    use super::*;
    use crate::model::{Event, Evidence, Session, Tool};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn ulid() -> ulid::Ulid {
        ulid::Ulid::generate()
    }

    fn temp_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        crate::git::init(&path).unwrap();
        (dir, path)
    }

    fn make_session(model: &str) -> Session {
        Session {
            id: ulid(),
            tool: Tool::Cursor,
            model: model.to_string(),
            agent_instance: None,
            conversation_id: None,
            started: jiff::Timestamp::now(),
            ended: None,
            base_commit: None,
        }
    }

    // --- §6.5 drift reconciliation ---

    #[test]
    fn reconcile_with_disk_preserves_surviving_provenance_6_5() {
        // §6.5: equal chars keep their provenance; inserted chars are untracked
        let e1 = ulid();
        let mut doc = replay::ProvenanceDoc::new();
        doc.spans = vec![replay::Span {
            len: 11,
            origin: e1,
        }];
        let mut event_models = HashMap::new();
        event_models.insert(e1, "opus".to_string());
        let mut event_sessions = HashMap::new();
        event_sessions.insert(e1, "conv-1".to_string());

        let (content, models, sessions) = reconcile_with_disk(
            &doc,
            "hello world",
            "hello big world",
            &event_models,
            &event_sessions,
        );

        assert_eq!(content, "hello big world");
        let mut expected = vec!["opus".to_string(); 6]; // "hello "
        expected.extend(vec!["untracked".to_string(); 4]); // "big "
        expected.extend(vec!["opus".to_string(); 5]); // "world"
        assert_eq!(models, expected);
        // Sessions track models: surviving chars keep their conversation id,
        // inserted chars carry an empty session.
        let mut expected_sessions = vec!["conv-1".to_string(); 6];
        expected_sessions.extend(vec![String::new(); 4]);
        expected_sessions.extend(vec!["conv-1".to_string(); 5]);
        assert_eq!(sessions, expected_sessions);
    }

    #[test]
    fn reconcile_with_disk_deletion_drops_removed_chars_6_5() {
        // §6.5: chars deleted from disk simply disappear from the mapping
        let e1 = ulid();
        let mut doc = replay::ProvenanceDoc::new();
        doc.spans = vec![replay::Span {
            len: 11,
            origin: e1,
        }];
        let mut event_models = HashMap::new();
        event_models.insert(e1, "opus".to_string());
        let mut event_sessions = HashMap::new();
        event_sessions.insert(e1, "conv-1".to_string());

        let (_content, models, sessions) =
            reconcile_with_disk(&doc, "hello world", "hello", &event_models, &event_sessions);
        assert_eq!(models, vec!["opus".to_string(); 5]);
        assert_eq!(sessions, vec!["conv-1".to_string(); 5]);
    }

    #[test]
    fn reconcile_with_disk_replacement_marks_untracked_6_5() {
        // §6.5: a replaced char is untracked, not guessed (I1)
        let e1 = ulid();
        let mut doc = replay::ProvenanceDoc::new();
        doc.spans = vec![replay::Span { len: 3, origin: e1 }];
        let mut event_models = HashMap::new();
        event_models.insert(e1, "opus".to_string());
        let mut event_sessions = HashMap::new();
        event_sessions.insert(e1, "conv-1".to_string());

        let (_content, models, sessions) =
            reconcile_with_disk(&doc, "abc", "axc", &event_models, &event_sessions);
        assert_eq!(models, vec!["opus", "untracked", "opus"]);
        assert_eq!(sessions, vec!["conv-1", "", "conv-1"]);
    }

    // --- §9 report generation ---

    #[test]
    fn build_report_data_flags_drifted_file_9() {
        // §6.5 + §9: a file edited after its last recorded event is flagged
        // drifted and its new chars are marked untracked end-to-end.
        let (_dir, repo) = temp_repo();
        let session = make_session("opus");
        let before_blob = crate::git::write_blob(&repo, b"").unwrap();
        let after_blob = crate::git::write_blob(&repo, b"hello world").unwrap();
        let event = Event {
            id: ulid(),
            session_id: session.id,
            prompt_id: None,
            tool_call_id: None,
            path: "a.rs".to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(&repo, &session, &[event]).unwrap();

        // The file on disk differs from the recorded after-state.
        std::fs::write(repo.join("a.rs"), "hello brave world").unwrap();

        let data = build_report_data(&repo, true).unwrap();
        let fr = data
            .files
            .iter()
            .find(|f| f.path == "a.rs")
            .expect("a.rs should be in the report");
        assert!(fr.drifted, "file should be flagged drifted");
        assert_eq!(fr.total_chars, "hello brave world".len());
        let untracked = fr.composition.get("untracked").copied().unwrap_or(0);
        assert_eq!(untracked, 6, "the inserted 'brave ' should be untracked");
    }

    #[test]
    fn report_carries_session_ids_per_char_9_3() {
        // §9.3: the report must carry the raw conversation id per character so
        // the grid tooltip can link a span back to the originating chat —
        // not just the model. char_sessions is parallel to char_models.
        let (_dir, repo) = temp_repo();
        let mut session = make_session("opus");
        session.conversation_id = Some("conv-abc-123".to_string());
        let before_blob = crate::git::write_blob(&repo, b"").unwrap();
        let after_blob = crate::git::write_blob(&repo, b"hello").unwrap();
        let event = Event {
            id: ulid(),
            session_id: session.id,
            prompt_id: None,
            tool_call_id: None,
            path: "a.rs".to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(&repo, &session, &[event]).unwrap();
        // Match the replayed content exactly so the file is not drifted.
        std::fs::write(repo.join("a.rs"), "hello").unwrap();

        let data = build_report_data(&repo, true).unwrap();
        let fr = data
            .files
            .iter()
            .find(|f| f.path == "a.rs")
            .expect("a.rs should be in the report");
        assert_eq!(fr.char_models, vec!["opus".to_string(); 5]);
        assert_eq!(fr.char_sessions, vec!["conv-abc-123".to_string(); 5]);
    }

    #[test]
    fn no_session_flag_omits_session_data_9_3() {
        // §9.3 privacy switch: when sessions are disabled, the report must
        // NOT embed per-character session/conversation ids — the ids should
        // never leave the machine, and the JSON must not contain them.
        let (_dir, repo) = temp_repo();
        let mut session = make_session("opus");
        session.conversation_id = Some("conv-abc-123".to_string());
        let before_blob = crate::git::write_blob(&repo, b"").unwrap();
        let after_blob = crate::git::write_blob(&repo, b"hello").unwrap();
        let event = Event {
            id: ulid(),
            session_id: session.id,
            prompt_id: None,
            tool_call_id: None,
            path: "a.rs".to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(&repo, &session, &[event]).unwrap();
        std::fs::write(repo.join("a.rs"), "hello").unwrap();

        let data = build_report_data(&repo, false).unwrap();
        let fr = data
            .files
            .iter()
            .find(|f| f.path == "a.rs")
            .expect("a.rs should be in the report");
        assert!(fr.char_sessions.is_empty(), "char_sessions must be empty");

        // The serialized JSON must not contain the conversation id at all.
        let json = serde_json::to_string(&data).unwrap();
        assert!(
            !json.contains("conv-abc-123"),
            "conversation id leaked into JSON when sessions disabled"
        );
    }

    #[test]
    fn fully_untracked_file_uses_flag_not_char_array_9() {
        // §9.3: a file with no provenance events at all must not store a
        // per-char "untracked" array (which balloons report size); a single
        // all_untracked flag + empty char_models carries the same info.
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join("a.rs"), "hello world").unwrap();

        let data = build_report_data(&repo, true).unwrap();
        let fr = data
            .files
            .iter()
            .find(|f| f.path == "a.rs")
            .expect("a.rs should be in the report (walked from disk)");
        assert!(fr.all_untracked, "fully-untracked file must set the flag");
        assert!(
            fr.char_models.is_empty(),
            "char_models must be empty (not one entry per char)"
        );
        assert_eq!(fr.composition.get("untracked"), Some(&11));
        assert_eq!(fr.total_chars, 11);

        // And the serialized JSON must not embed a per-char array.
        let json = serde_json::to_string(&data).unwrap();
        assert!(
            !json.contains("\"untracked\",\"untracked\""),
            "per-char untracked array leaked into JSON"
        );
    }

    #[test]
    fn walk_repo_files_excludes_non_source_9() {
        // §9.3: non-source files and build dirs are excluded from the scan
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".cursor/hooks")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("README.md"), "# hi").unwrap();
        std::fs::write(root.join("report.html"), "<html>").unwrap();
        std::fs::write(root.join("image.png"), "png").unwrap();
        std::fs::write(root.join("Cargo.lock"), "lock").unwrap();
        std::fs::write(root.join("target/out.rs"), "x").unwrap();
        std::fs::write(root.join("node_modules/lib.rs"), "x").unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        std::fs::write(root.join(".cursor/hooks/h.sh"), "x").unwrap();

        let files = walk_repo_files(root).unwrap();
        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(files.contains(&"README.md".to_string()));
        assert!(!files.iter().any(|f| f.ends_with(".html")));
        assert!(!files.iter().any(|f| f.ends_with(".png")));
        assert!(!files.iter().any(|f| f.ends_with(".lock")));
        assert!(!files.iter().any(|f| f.contains("target/")));
        assert!(!files.iter().any(|f| f.contains("node_modules/")));
        assert!(!files.iter().any(|f| f.contains(".git")));
        assert!(!files.iter().any(|f| f.contains(".cursor")));
    }

    #[test]
    fn escape_json_for_html_neutralizes_script_close_9() {
        // §9: JSON embedded in HTML must not break out via </script>
        assert_eq!(
            escape_json_for_html(r#"{"x":"</script>"}"#),
            r#"{"x":"<\/script>"}"#
        );
    }

    #[test]
    fn walk_repo_files_excludes_cache_dirs_and_secrets_9_3() {
        // §9.3 / §11: cache/artifact dirs and secret files must not be
        // embedded in the report. A real project carries .mypy_cache, .nuxt,
        // .output, __pycache__, .venv, dist, build, and .env secrets; these
        // bloat the report and can leak credentials.
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Real source stays.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.py"), "print('hi')\n").unwrap();
        std::fs::write(root.join("README.md"), "# hi").unwrap();

        // Cache/artifact dirs — all must be skipped.
        for d in [
            ".mypy_cache",
            ".pytest_cache",
            ".ruff_cache",
            "__pycache__",
            ".nuxt",
            ".output",
            ".next",
            ".turbo",
            ".venv",
            "venv",
            "dist",
            "build",
            ".cache",
            ".data",
        ] {
            std::fs::create_dir_all(root.join(d).join("sub")).unwrap();
            std::fs::write(root.join(d).join("sub/cache.json"), "x").unwrap();
        }

        // Secret files — must not be embedded (§11).
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::write(root.join(".env.local"), "SECRET=2").unwrap();
        std::fs::create_dir_all(root.join(".envs/.local")).unwrap();
        std::fs::write(root.join(".envs/.local/.aurora"), "SECRET=3").unwrap();

        let files = walk_repo_files(root).unwrap();

        assert!(
            files.contains(&"src/main.py".to_string()),
            "source must stay"
        );
        assert!(files.contains(&"README.md".to_string()), "docs must stay");
        for d in [
            ".mypy_cache",
            ".pytest_cache",
            ".ruff_cache",
            "__pycache__",
            ".nuxt",
            ".output",
            ".next",
            ".turbo",
            ".venv",
            "venv",
            "dist",
            "build",
            ".cache",
            ".data",
        ] {
            assert!(
                !files.iter().any(|f| f.starts_with(&format!("{d}/"))),
                "{d}/ must be excluded, got: {files:?}"
            );
        }
        assert!(
            !files
                .iter()
                .any(|f| f.ends_with(".env") || f.contains(".envs/")),
            "secret files must be excluded: {files:?}"
        );
    }

    #[test]
    fn build_report_data_includes_analytics_9_4() {
        // §9.4: report JSON carries treemap, stratigraphy, heatmap, and
        // per-model half-life curves.
        let (_dir, repo) = temp_repo();
        let session = make_session("opus");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let before_blob = crate::git::write_blob(&repo, b"").unwrap();
        let after_blob = crate::git::write_blob(&repo, b"hello").unwrap();
        let event = Event {
            id: ulid(),
            session_id: session.id,
            prompt_id: None,
            tool_call_id: None,
            path: "src/a.rs".to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(&repo, &session, &[event]).unwrap();
        // Match the replayed content exactly so the file is not drifted.
        std::fs::write(repo.join("src/a.rs"), "hello").unwrap();

        let data = build_report_data(&repo, true).unwrap();

        // Treemap: one file, dominant model = opus, area = 5 chars
        assert_eq!(data.treemap.len(), 1);
        assert_eq!(data.treemap[0].path, "src/a.rs");
        assert_eq!(data.treemap[0].chars, 5);
        assert_eq!(data.treemap[0].model.as_deref(), Some("opus"));

        // Heatmap: (opus, "src") → 5 surviving chars
        let hm = data
            .heatmap
            .iter()
            .find(|e| e.model == "opus" && e.dir == "src")
            .expect("heatmap has opus/src entry");
        assert_eq!(hm.chars, 5);

        // Half-life: opus has a survival curve with one point
        let hl = data
            .half_life
            .iter()
            .find(|c| c.model == "opus")
            .expect("half-life has opus curve");
        assert_eq!(hl.points.len(), 1);

        // Stratigraphy: at least one point; opus layer accumulates to 5
        assert!(!data.stratigraphy.is_empty());
        assert_eq!(
            data.stratigraphy.last().unwrap().layers.get("opus"),
            Some(&5)
        );
    }

    // --- B1 module report (end-to-end through the report pipeline) ---

    /// Record one non-transparent event writing `after` to `path`, with the
    /// matching content written to disk (so the file is not drifted).
    fn record_event(repo: &std::path::Path, session: &Session, path: &str, after: &str) {
        let before_blob = crate::git::write_blob(repo, b"").unwrap();
        let after_blob = crate::git::write_blob(repo, after.as_bytes()).unwrap();
        let event = Event {
            id: ulid(),
            session_id: session.id,
            prompt_id: None,
            tool_call_id: None,
            path: path.to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(repo, session, &[event]).unwrap();
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(repo.join(parent)).unwrap();
        }
        std::fs::write(repo.join(path), after).unwrap();
    }

    /// Record an event whose `before` is given, as in logs written before
    /// capture-side gap bridging existed.
    fn record_edit(
        repo: &std::path::Path,
        session: &Session,
        path: &str,
        before: &str,
        after: &str,
    ) {
        let event = Event {
            id: ulid(),
            session_id: session.id,
            prompt_id: None,
            tool_call_id: None,
            path: path.to_string(),
            before_blob: crate::git::write_blob(repo, before.as_bytes()).unwrap(),
            after_blob: crate::git::write_blob(repo, after.as_bytes()).unwrap(),
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(repo, session, &[event]).unwrap();
        std::fs::write(repo.join(path), after).unwrap();
    }

    #[test]
    fn report_half_life_and_composition_survive_gappy_logs_8() {
        // §6.5 / §8: pre-existing content and hand edits between events show
        // up as untracked, and survival never exceeds 1.
        let (_dir, repo) = temp_repo();
        let opus = make_session("opus");
        record_edit(&repo, &opus, "a.rs", "legacy\n", "legacy\nopus\n");
        std::thread::sleep(std::time::Duration::from_millis(2));
        record_edit(
            &repo,
            &opus,
            "a.rs",
            "legacy\nopus\nhand\n",
            "legacy\nopus\nhand\nopus2\n",
        );

        let data = build_report_data(&repo, true).unwrap();

        let opus_curve = data.half_life.iter().find(|c| c.model == "opus").unwrap();
        for p in &opus_curve.points {
            assert!(
                p.survival <= 1.0,
                "survival {} at age {}",
                p.survival,
                p.age_days
            );
        }
        assert!(data.half_life.iter().all(|c| c.model != "untracked"));

        let total = "legacy\nopus\nhand\nopus2\n".len();
        assert_eq!(data.total_chars, total);
        assert_eq!(
            data.composition.get("opus"),
            Some(&("opus\n".len() + "opus2\n".len()))
        );
        assert_eq!(
            data.composition.get("untracked"),
            Some(&("legacy\n".len() + "hand\n".len()))
        );
        assert!(
            !data.composition.contains_key("unknown"),
            "{:?}",
            data.composition
        );
    }

    #[test]
    fn module_report_matches_flat_comp_totals_B1() {
        // §9.5 B1: summing module total_chars equals repo_total_chars (replayed ∪
        // untracked union, no chars lost or double-counted).
        let (_dir, repo) = temp_repo();
        let opus = make_session("opus");
        record_event(&repo, &opus, "src/a.rs", "hello");
        // An untracked file on disk (no events).
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/extra.rs"), "untracked!").unwrap();

        let export = build_module_export(&repo).unwrap();
        let sum: usize = export.modules.iter().map(|m| m.total_chars).sum();
        assert_eq!(sum, export.repo_total_chars);
        assert!(export.repo_total_chars > 0);
    }

    #[test]
    fn module_report_event_deleted_file_zero_chars_B1() {
        // §9.5 B1: a file deleted via an event (after_blob empty) is attributed
        // by its lexical top-level directory with total_chars 0, contributing 0
        // to repo_total.
        let (_dir, repo) = temp_repo();
        let opus = make_session("opus");
        let before_blob = crate::git::write_blob(&repo, b"hello").unwrap();
        let after_blob = crate::git::write_blob(&repo, b"").unwrap();
        let event = Event {
            id: ulid(),
            session_id: opus.id,
            prompt_id: None,
            tool_call_id: None,
            path: "src/a.rs".to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(&repo, &opus, &[event]).unwrap();

        let export = build_module_export(&repo).unwrap();
        let m = export.modules.iter().find(|m| m.id == "src").unwrap();
        assert_eq!(m.total_chars, 0);
        assert_eq!(export.repo_total_chars, 0);
    }

    #[test]
    fn module_report_disk_deleted_file_replay_chars_B1() {
        // §9.5 B1: a file deleted on disk without a delete event falls back to
        // replay content, flags drifted, and its chars count toward repo_total.
        let (_dir, repo) = temp_repo();
        let opus = make_session("opus");
        let before_blob = crate::git::write_blob(&repo, b"").unwrap();
        let after_blob = crate::git::write_blob(&repo, b"hello").unwrap();
        let event = Event {
            id: ulid(),
            session_id: opus.id,
            prompt_id: None,
            tool_call_id: None,
            path: "src/a.rs".to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(&repo, &opus, &[event]).unwrap();
        // Do NOT write src/a.rs to disk — deleted on disk without a delete event.

        let export = build_module_export(&repo).unwrap();
        let m = export.modules.iter().find(|m| m.id == "src").unwrap();
        assert_eq!(m.total_chars, 5);
        assert_eq!(m.composition.get("opus"), Some(&5));
        assert!(m.drifted);
        assert_eq!(export.repo_total_chars, 5);
    }

    // --- C1 stable module export ---

    #[test]
    fn modules_export_emits_versioned_schema_C1() {
        // §9.5 C1: the export declares the versioned schema.
        let (_dir, repo) = temp_repo();
        let export = build_module_export(&repo).unwrap();
        assert_eq!(export.schema_version, "diffy-modules/1.0");
    }

    #[test]
    fn modules_export_omits_unbuilt_fields_C1() {
        // §9.5 C1: future fields are additive and must be absent (not null).
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join("a.rs"), "x").unwrap();
        let json = module_export_json(&repo).unwrap();
        assert!(!json.contains("ai_churn_surviving_ratio"));
        assert!(!json.contains("cost_to_value_proxy"));
        assert!(!json.contains("agent_hours"));
    }

    #[test]
    fn modules_export_contains_no_session_ids_C1() {
        // §9.5 C1: the module export carries only aggregate counts — no
        // conversation id or session ULID anywhere.
        let (_dir, repo) = temp_repo();
        let mut session = make_session("opus");
        session.conversation_id = Some("conv-abc-123".to_string());
        let before_blob = crate::git::write_blob(&repo, b"").unwrap();
        let after_blob = crate::git::write_blob(&repo, b"hello").unwrap();
        let event = Event {
            id: ulid(),
            session_id: session.id,
            prompt_id: None,
            tool_call_id: None,
            path: "a.rs".to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(&repo, &session, &[event]).unwrap();
        std::fs::write(repo.join("a.rs"), "hello").unwrap();

        let json = module_export_json(&repo).unwrap();
        assert!(!json.contains("conv-abc-123"));
        assert!(!json.contains(&session.id.to_string()));
    }

    #[test]
    fn modules_export_sorts_by_id_C1() {
        // §9.5 C1: modules[] sorted by id ascending.
        let (_dir, repo) = temp_repo();
        std::fs::create_dir_all(repo.join("z")).unwrap();
        std::fs::write(repo.join("z/Cargo.toml"), "[package]\nname = \"z\"\n").unwrap();
        std::fs::write(repo.join("z/mod.rs"), "z").unwrap();
        std::fs::create_dir_all(repo.join("a")).unwrap();
        std::fs::write(repo.join("a/Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        std::fs::write(repo.join("a/mod.rs"), "a").unwrap();
        std::fs::write(repo.join("README.md"), "# hi").unwrap(); // → "(root)"

        let export = build_module_export(&repo).unwrap();
        let ids: Vec<&str> = export.modules.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["(root)", "a", "z"]);
    }

    #[test]
    fn modules_export_sorted_composition_keys_C1() {
        // §9.5 C1: composition keys serialize in sorted order (ordered map).
        let (_dir, repo) = temp_repo();
        let zebra = make_session("zebra");
        record_event(&repo, &zebra, "src/a.rs", "zz");
        let alpha = make_session("alpha");
        record_event(&repo, &alpha, "src/b.rs", "aa");

        let json = module_export_json(&repo).unwrap();
        let a = json.find("\"alpha\"").expect("alpha key present");
        let z = json.find("\"zebra\"").expect("zebra key present");
        assert!(a < z, "composition keys must be sorted: alpha before zebra");
    }

    #[test]
    fn modules_export_generated_at_is_rfc3339_utc_C1() {
        // §9.5 C1: generated_at is an RFC 3339 UTC timestamp.
        let (_dir, repo) = temp_repo();
        let export = build_module_export(&repo).unwrap();
        assert!(export.generated_at.ends_with('Z'), "UTC offset should be Z");
        let parsed: jiff::Timestamp = export
            .generated_at
            .parse()
            .expect("generated_at must parse as RFC 3339");
        assert_eq!(parsed.to_string(), export.generated_at);
    }

    #[test]
    fn modules_cli_json_writes_stdout_C1() {
        // §9.5 C1: --modules --json writes JSON to stdout (a writer in tests).
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join("a.rs"), "x").unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_module_json(&repo, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["schema_version"], "diffy-modules/1.0");
    }

    #[test]
    fn modules_cli_out_writes_file_C1() {
        // §9.5 C1: --modules --out <file> writes JSON to the file.
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join("a.rs"), "x").unwrap();
        let out = repo.join("modules.json");
        write_module_report(&repo, &out).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["schema_version"], "diffy-modules/1.0");
    }

    #[test]
    fn modules_cli_no_session_is_noop_C1() {
        // §9.5 C1: --no-session output is identical to --modules except for
        // generated_at (parse, remove generated_at, assert structural equality).
        let (_dir, repo) = temp_repo();
        std::fs::write(repo.join("a.rs"), "x").unwrap();
        let mut a = serde_json::to_value(build_module_export(&repo).unwrap()).unwrap();
        let mut b = serde_json::to_value(build_module_export(&repo).unwrap()).unwrap();
        a.as_object_mut().unwrap().remove("generated_at");
        b.as_object_mut().unwrap().remove("generated_at");
        assert_eq!(a, b);
    }

    #[test]
    fn modules_export_empty_repo_C1() {
        // §9.5 C1: no events and no walkable source files → modules: [],
        // repo_total_chars: 0.
        let (_dir, repo) = temp_repo();
        let export = build_module_export(&repo).unwrap();
        assert!(export.modules.is_empty());
        assert_eq!(export.repo_total_chars, 0);
    }

    #[test]
    fn modules_export_zero_char_module_json_shape_C1() {
        // §9.5 C1: a zero-char module's JSON has dominant_model absent (not null),
        // composition {}, and no extraneous keys.
        let (_dir, repo) = temp_repo();
        let opus = make_session("opus");
        let before_blob = crate::git::write_blob(&repo, b"hello").unwrap();
        let after_blob = crate::git::write_blob(&repo, b"").unwrap();
        let event = Event {
            id: ulid(),
            session_id: opus.id,
            prompt_id: None,
            tool_call_id: None,
            path: "src/a.rs".to_string(),
            before_blob,
            after_blob,
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        };
        crate::store::append_events(&repo, &opus, &[event]).unwrap();

        let json = module_export_json(&repo).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let m = v["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "src")
            .expect("src module present");
        assert!(
            m.get("dominant_model").is_none(),
            "dominant_model must be absent, not null"
        );
        assert_eq!(m["composition"], serde_json::json!({}));
        let mut keys: Vec<String> = m.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "composition",
                "drifted",
                "file_count",
                "fully_untracked",
                "id",
                "total_chars"
            ]
        );
    }
}
