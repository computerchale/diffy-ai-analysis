//! diffy — character-level provenance for AI-written code.
//!
//! Short-lived CLI only: agent/git hooks invoke subcommands per event;
//! reports are static HTML. No webserver, no long-running processes
//! (`watch` mode, when it lands, is an optional background process).

// §14: no panics outside tests.
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

mod blame;
mod capture;
mod doctor;
mod git;
mod hook_log;
mod model;
mod modules;
mod replay;
mod report;
mod stats;
mod store;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "diffy",
    version,
    about = "Character-level provenance for AI-written code"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize diffy in a repo: create provenance refs namespace
    /// and install Cursor hooks for automatic capture.
    Init {
        /// Path to the git repo; defaults to current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Cursor workspace root (where .cursor/ lives or should be created).
        /// Required when the repo is a subfolder of the workspace.
        /// If omitted, walks up from --repo looking for an existing .cursor/,
        /// or defaults to the repo root itself.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Replace an existing .cursor/hooks.json that can't be merged
        /// (invalid JSON or unexpected shape). The old file is kept as
        /// hooks.json.bak.
        #[arg(long)]
        force: bool,
    },
    /// Check that capture is set up and working. Read-only; exits 1 if any
    /// check fails.
    Doctor {
        /// Path to the git repo; defaults to current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Cursor workspace root; found the same way as `diffy init`.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Handle one agent/editor hook event (invoked by Cursor/Claude Code hooks).
    /// Reads a JSON payload describing the edit.
    Hook {
        /// Path to the git repo. With --cursor, defaults to the repo that
        /// contains the payload's file_path; otherwise the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Hook event payload as JSON string, or @path to a payload file.
        /// With --cursor, this is the raw Cursor afterFileEdit payload.
        #[arg(long)]
        payload: String,
        /// Parse the payload as Cursor's native afterFileEdit format
        /// (file_path, edits[], model, conversation_id) instead of
        /// diffy's own HookPayload format.
        #[arg(long)]
        cursor: bool,
    },
    /// Bind recorded events to a new commit (invoked by post-commit hook)
    Checkpoint {
        /// Path to the git repo; defaults to current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Annotated source with per-span model attribution
    Blame {
        /// Path to the git repo; defaults to current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// File to annotate.
        path: String,
    },
    /// Composition breakdown of a file, directory, or the whole repo.
    /// Shows what percentage of surviving characters came from each model.
    Comp {
        /// Path to the git repo; defaults to current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// File or directory; omit for the whole repo.
        path: Option<String>,
        /// Output as JSON instead of human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Show the model and session behind a span, e.g. src/foo.rs:120
    Why {
        /// Path to the git repo; defaults to current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// File:line to attribute.
        target: String,
    },
    /// Survival curves: code half-life per model
    HalfLife {
        /// Path to the git repo; defaults to current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Only compute for this model; omit for all models combined.
        #[arg(long)]
        model: Option<String>,
    },
    /// Generate a self-contained HTML report (character grid, treemap, stratigraphy, heatmap).
    /// Reads all data from the git repo's provenance refs — no external state needed.
    Report {
        /// Path to a git repo; defaults to the current directory.
        #[arg(long)]
        repo: Option<String>,
        /// Output file path; defaults to diffy-report.html (with --modules,
        /// writes the module JSON to this file — requires exactly one of
        /// --json or --out).
        #[arg(long)]
        out: Option<String>,
        /// Emit raw JSON to stdout instead of writing HTML.
        #[arg(long)]
        json: bool,
        /// Omit per-character session/conversation ids from the report
        /// (privacy: the ids stay local and are never embedded).
        #[arg(long)]
        no_session: bool,
        /// Emit a module-level provenance report (SPEC.md §9.5)
        /// instead of the per-file/char payload.
        #[arg(long)]
        modules: bool,
    },
    /// POST-MVP (SPEC.md §18.1): background worktree watcher
    Watch {
        #[arg(long)]
        install: bool,
        #[arg(long)]
        stop: bool,
    },
    /// POST-MVP (SPEC.md §18.2): localhost live-report server
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        open: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init {
            repo,
            workspace,
            force,
        } => cmd_init(&repo, workspace.as_deref(), force),
        Commands::Doctor { repo, workspace } => cmd_doctor(&repo, workspace.as_deref()),
        Commands::Hook {
            repo,
            payload,
            cursor,
        } => run_hook(
            repo.as_deref(),
            &payload,
            cursor,
            hook_log::fallback_log_path().as_deref(),
        ),
        Commands::Comp { repo, path, json } => cmd_comp(&repo, path.as_deref(), json),
        Commands::Checkpoint { repo } => cmd_checkpoint(&repo),
        Commands::Blame { repo, path } => cmd_blame(&repo, &path),
        Commands::Why { repo, target } => cmd_why(&repo, &target),
        Commands::HalfLife { repo, model } => cmd_half_life(&repo, model.as_deref()),
        Commands::Report {
            repo,
            out,
            json,
            no_session,
            modules,
        } => cmd_report(repo, out, json, no_session, modules),
        Commands::Watch { .. } => Err(anyhow!(
            "`watch` is POST-MVP (SPEC.md §18.1) and not implemented yet"
        )),
        Commands::Serve { .. } => Err(anyhow!(
            "`serve` is POST-MVP (SPEC.md §18.2) and not implemented yet"
        )),
    }
}

/// `diffy report` — HTML report, raw JSON, or (with `--modules`) the C1
/// module-provenance export (SPEC.md §9.5).
///
/// With `--modules`, exactly one of `--json` (stdout) or `--out <file>` is
/// required; both or neither is a usage error. Without `--modules`, behavior is
/// unchanged: `--json` → stdout, else HTML to `--out` (default
/// `diffy-report.html`).
fn cmd_report(
    repo: Option<String>,
    out: Option<String>,
    json: bool,
    no_session: bool,
    modules: bool,
) -> Result<()> {
    let repo_path = repo
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    require_repo(&repo_path)?;
    print_hook_notices(&repo_path);
    let include_sessions = !no_session;
    if modules {
        match (json, &out) {
            (true, None) => report::write_module_json(&repo_path, &mut std::io::stdout()),
            (false, Some(f)) => {
                let out_path = PathBuf::from(f);
                report::write_module_report(&repo_path, &out_path)?;
                println!("Module report written to {}", out_path.display());
                Ok(())
            }
            (false, None) => Err(anyhow!(
                "`--modules` requires exactly one of `--json` or `--out <file>`"
            )),
            (true, Some(_)) => Err(anyhow!(
                "`--modules` accepts `--json` or `--out <file>`, not both"
            )),
        }
    } else if json {
        report::write_json(&repo_path, include_sessions)
    } else {
        let out_path = PathBuf::from(out.unwrap_or_else(|| "diffy-report.html".to_string()));
        report::write_report(&repo_path, &out_path, include_sessions)?;
        println!("Report written to {}", out_path.display());
        // Try to open in browser
        #[cfg(all(target_os = "macos", not(test)))]
        {
            let _ = std::process::Command::new("open").arg(&out_path).spawn();
        }
        Ok(())
    }
}

/// `diffy init` — verify the repo is a git repo and install Cursor hooks.
/// Handles repos that are subfolders of the Cursor workspace.
fn cmd_init(
    repo: &std::path::Path,
    workspace: Option<&std::path::Path>,
    force: bool,
) -> Result<()> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());

    // Verify it's a git repo
    git::run(&repo, &["rev-parse", "--git-dir"])
        .context("not a git repository — run `git init` first")?;

    let install_at = resolve_workspace(&repo, workspace);

    install_cursor_hooks(&install_at, &repo, force)?;
    println!("diffy initialized in {}", repo.display());
    println!("Provenance refs will be created on first hook event.");

    Ok(())
}

/// Where Cursor hooks live for `repo`:
/// 1. Explicit --workspace flag (user knows best)
/// 2. Walk up looking for existing .cursor/ (stop before $HOME)
/// 3. Fall back to the repo root itself
fn resolve_workspace(repo: &std::path::Path, workspace: Option<&std::path::Path>) -> PathBuf {
    match workspace {
        Some(ws) => ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf()),
        None => find_cursor_workspace(repo).unwrap_or_else(|| repo.to_path_buf()),
    }
}

/// `diffy doctor` — print every check; fail if any check failed.
fn cmd_doctor(repo: &std::path::Path, workspace: Option<&std::path::Path>) -> Result<()> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let workspace = resolve_workspace(&repo, workspace);
    let fallback_log = hook_log::fallback_log_path();
    let path_var = std::env::var_os("PATH");
    let checks = doctor::run(
        &repo,
        &workspace,
        &doctor::Env {
            fallback_log: fallback_log.as_deref(),
            path_var: path_var.as_deref(),
        },
    );
    print!("{}", doctor::render(&checks));
    if doctor::has_failures(&checks) {
        std::process::exit(1);
    }
    Ok(())
}

/// Merge diffy's `afterFileEdit` entry into an existing hooks.json
/// (`None` = no file yet), keeping every non-diffy hook. Errors rather
/// than guessing when the input isn't JSON or has an unexpected shape
/// (§4.1): it may hold the user's other hooks.
fn merge_diffy_hook(existing: Option<&str>) -> Result<serde_json::Value> {
    let mut config: serde_json::Value = match existing {
        Some(content) => serde_json::from_str(content).context("hooks.json is not valid JSON")?,
        None => serde_json::json!({"version": 1, "hooks": {}}),
    };

    let hooks = config
        .as_object_mut()
        .context("hooks.json must be a JSON object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("hooks.json `hooks` must be an object")?;

    let after_file_edit = hooks
        .entry("afterFileEdit")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .context("hooks.json `hooks.afterFileEdit` must be an array")?;

    // Drop previous diffy entries so re-running init doesn't duplicate.
    after_file_edit.retain(|h| !is_diffy_hook(h));
    after_file_edit.push(serde_json::json!({
        "command": ".cursor/hooks/diffy-hook.sh"
    }));

    Ok(config)
}

fn is_diffy_hook(entry: &serde_json::Value) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains("diffy"))
}

/// True if hooks.json already registers a diffy `afterFileEdit` hook.
fn has_diffy_entry(config: &serde_json::Value) -> bool {
    config["hooks"]["afterFileEdit"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(is_diffy_hook))
}

/// Walk up from `start` looking for a project-level `.cursor/` directory,
/// which indicates the Cursor workspace root. Stops before `$HOME` to
/// avoid confusing the user-level `~/.cursor/` config with a workspace.
fn find_cursor_workspace(start: &std::path::Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let mut dir = start.to_path_buf();
    loop {
        // Don't go above $HOME — ~/.cursor/ is user config, not a workspace
        if let Some(ref h) = home
            && dir == *h
        {
            return None;
        }
        if dir.join(".cursor").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Install the diffy Cursor hook at the workspace root.
/// Creates .cursor/hooks/diffy-hook.sh and updates .cursor/hooks.json.
fn install_cursor_hooks(
    workspace: &std::path::Path,
    repo: &std::path::Path,
    force: bool,
) -> Result<()> {
    // Resolve hooks.json before touching disk, so a refusal (§4.1
    // fail-closed) leaves the workspace exactly as it was.
    let hooks_json_path = workspace.join(".cursor/hooks.json");
    let existing = if hooks_json_path.exists() {
        Some(
            std::fs::read_to_string(&hooks_json_path)
                .with_context(|| format!("failed to read {}", hooks_json_path.display()))?,
        )
    } else {
        None
    };
    let config = match merge_diffy_hook(existing.as_deref()) {
        Ok(config) => config,
        Err(err) if force => {
            let backup = hooks_json_path.with_extension("json.bak");
            std::fs::copy(&hooks_json_path, &backup)
                .with_context(|| format!("failed to back up {}", hooks_json_path.display()))?;
            eprintln!(
                "warning: replacing unmergeable {} ({err:#}); previous contents saved to {}",
                hooks_json_path.display(),
                backup.display()
            );
            merge_diffy_hook(None)?
        }
        Err(err) => {
            return Err(err.context(format!(
                "refusing to overwrite {} — fix it by hand, or re-run with --force \
                 to replace it (the old file is kept as hooks.json.bak)",
                hooks_json_path.display()
            )));
        }
    };

    let hooks_dir = workspace.join(".cursor/hooks");
    let hook_path = hooks_dir.join("diffy-hook.sh");
    if !force {
        let has_entry = existing
            .as_deref()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .is_some_and(|v| has_diffy_entry(&v));
        if has_entry || hook_path.exists() {
            anyhow::bail!(
                "diffy is already set up in {} — re-run with --force to reinstall",
                workspace.display()
            );
        }
    }

    std::fs::create_dir_all(&hooks_dir).context("failed to create .cursor/hooks/ directory")?;

    // Find the diffy binary path
    let diffy_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("diffy"));

    std::fs::write(&hook_path, hook_script(&diffy_bin)).context("failed to write hook script")?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    std::fs::write(&hooks_json_path, serde_json::to_string_pretty(&config)?)?;

    println!();
    println!("Cursor hook installed:");
    println!("  script: {}", hook_path.display());
    println!("  config: {}", hooks_json_path.display());
    println!();
    if workspace != repo {
        println!(
            "  (workspace: {}, repo: {} — sub-repo detected)",
            workspace.display(),
            repo.display()
        );
    }
    println!("Agent edits in this repo will now be tracked automatically.");
    Ok(())
}

/// The generated Cursor hook script (§4.1).
///
/// Design notes (learned from live debugging):
/// - Payload goes via stdin (@-): no temp-file race between this script
///   exiting and a backgrounded diffy reading the file.
/// - Synchronous, NOT backgrounded: Cursor reaps the hook's process group
///   when the script exits, killing background jobs before they run.
/// - Always exits 0: a diffy failure must never break the editor.
fn hook_script(diffy_bin: &std::path::Path) -> String {
    format!(
        r#"#!/bin/bash
# diffy Cursor hook — afterFileEdit handler (SPEC.md §4.1)
# Auto-generated by `diffy init`; regenerate with `diffy init --force`.
# Needs only bash: diffy parses the payload and finds the repo itself.

BAKED={baked}
DIFFY="$BAKED"
if [ ! -x "$DIFFY" ]; then
  DIFFY=$(command -v diffy 2>/dev/null)
fi

if [ -z "$DIFFY" ]; then
  # Without diffy there's no repo-specific log, so use the per-user one
  # (same path as hook_log::fallback_log_path).
  LOG="${{XDG_STATE_HOME:-$HOME/.local/state}}/diffy/hook.log"
  mkdir -p "$(dirname "$LOG")" 2>/dev/null
  echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) error diffy binary not found (not at $BAKED or on PATH); edit not recorded" >> "$LOG" 2>/dev/null
  exit 0
fi

# Synchronous on purpose: backgrounded jobs are reaped by Cursor's hook
# cleanup. The payload goes straight from our stdin (@-).
"$DIFFY" hook --cursor --payload @- || true
exit 0
"#,
        baked = shell_quote(&diffy_bin.to_string_lossy())
    )
}

/// Single-quote `s` for bash, so paths with spaces or `$` stay literal.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Read a `--payload` argument into a JSON string (§4.1).
///
/// Three forms: `@-` reads the payload from stdin, `@<path>` reads it from
/// a file, and anything else is the literal JSON string. The `@-` form lets
/// hook scripts pipe the agent's payload directly — no temp file, no race
/// between the backgrounded diffy process and the script's cleanup.
fn read_payload_arg(arg: &str, mut stdin: impl std::io::Read) -> Result<String> {
    match arg.strip_prefix('@') {
        Some("-") => {
            let mut buf = String::new();
            stdin
                .read_to_string(&mut buf)
                .context("failed to read payload from stdin")?;
            Ok(buf)
        }
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read payload file: {path}")),
        None => Ok(arg.to_string()),
    }
}

/// `diffy hook` entry point: dispatches on `--cursor` and records any
/// problem in the hook log (§4.1), since the hook script discards our exit
/// status. Problems that happen before a repo is known go to `fallback_log`.
fn run_hook(
    repo: Option<&std::path::Path>,
    payload_str: &str,
    cursor: bool,
    fallback_log: Option<&std::path::Path>,
) -> Result<()> {
    let json = match read_payload_arg(payload_str, std::io::stdin()) {
        Ok(json) => json,
        Err(err) => return Err(log_hook_error(repo, fallback_log, err)),
    };
    let found;
    let repo = match repo {
        Some(repo) => repo,
        None if cursor => match repo_for_cursor_payload(&json) {
            Ok(Some(root)) => {
                found = root;
                &found
            }
            // Edits outside any git repo have nothing to be recorded in.
            Ok(None) => return Ok(()),
            Err(err) => return Err(log_hook_error(None, fallback_log, err)),
        },
        None => std::path::Path::new("."),
    };

    let result = if cursor {
        cmd_hook_cursor(repo, &json)
    } else {
        cmd_hook(repo, &json)
    };
    let warning = match result {
        Ok(warning) => warning,
        Err(err) if err.downcast_ref::<capture::IgnoredByGit>().is_some() => Some(err.to_string()),
        Err(err) => return Err(log_hook_error(Some(repo), fallback_log, err)),
    };
    if let Some(warning) = warning {
        eprintln!("warning: {warning}");
        hook_log::append(repo, hook_log::Level::Warning, &warning)?;
    }
    Ok(())
}

/// Fail unless `repo` is inside a git work tree, so a wrong `--repo` isn't
/// reported as "no provenance data yet" (§12).
fn require_repo(repo: &std::path::Path) -> Result<()> {
    if git::toplevel(repo)?.is_none() {
        let shown = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
        bail!(
            "{} is not a git repository — run inside one or pass --repo <path>",
            shown.display()
        );
    }
    Ok(())
}

fn print_hook_notices(repo: &std::path::Path) {
    for notice in hook_notices(repo, hook_log::fallback_log_path().as_deref()) {
        eprintln!("{notice}");
    }
}

/// Notices for `comp`/`report`: the repo's hook log, then the per-user
/// fallback log (§4.1).
fn hook_notices(repo: &std::path::Path, fallback_log: Option<&std::path::Path>) -> Vec<String> {
    let fallback = fallback_log.and_then(hook_log::summary_of);
    hook_log::summary(repo)
        .into_iter()
        .chain(fallback)
        .collect()
}

/// Record `err` in the repo's hook log, or the fallback log when there's no
/// usable repo. Logging is best effort: the original error matters more.
fn log_hook_error(
    repo: Option<&std::path::Path>,
    fallback_log: Option<&std::path::Path>,
    err: anyhow::Error,
) -> anyhow::Error {
    let message = format!("{err:#}");
    let logged =
        repo.is_some_and(|r| hook_log::append(r, hook_log::Level::Error, &message).is_ok());
    if !logged && let Some(path) = fallback_log {
        let _ = hook_log::append_to(path, hook_log::Level::Error, &message);
    }
    err
}

/// The repo containing a Cursor payload's `file_path` (§4.1).
fn repo_for_cursor_payload(json: &str) -> Result<Option<PathBuf>> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("failed to parse Cursor hook payload JSON")?;
    let file_path = value
        .get("file_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.is_empty())
        .context("Cursor hook payload has no file_path")?;
    let file_path = std::path::Path::new(file_path);
    git::toplevel(file_path.parent().unwrap_or(file_path))
}

/// `diffy hook --cursor` — process a Cursor afterFileEdit payload.
/// Returns a warning when the edit was recorded as untracked.
fn cmd_hook_cursor(repo: &std::path::Path, json: &str) -> Result<Option<String>> {
    let cursor: capture::CursorPayload =
        serde_json::from_str(json).context("failed to parse Cursor hook payload JSON")?;

    let capture::CursorCapture {
        session,
        event,
        warning,
    } = capture::process_cursor_hook(repo, &cursor)?;
    println!(
        "recorded: {} → {} (model: {}, event: {})",
        event.path, event.after_blob, session.model, event.id
    );
    Ok(warning)
}

/// `diffy hook` — process one native hook event.
fn cmd_hook(repo: &std::path::Path, json: &str) -> Result<Option<String>> {
    let payload: capture::HookPayload =
        serde_json::from_str(json).context("failed to parse hook payload JSON")?;

    let (session, event) = capture::process_hook(repo, &payload)?;
    println!(
        "recorded: {} → {} (model: {}, event: {})",
        event.path, event.after_blob, session.model, event.id
    );
    Ok(None)
}

/// `diffy comp [path]` — composition breakdown.
fn cmd_comp(repo: &std::path::Path, path: Option<&str>, json_output: bool) -> Result<()> {
    require_repo(repo)?;
    print_hook_notices(repo);
    // Read all events and sessions
    let events = store::read_all_events(repo)?;
    let sessions = store::read_all_sessions(repo)?;

    if events.is_empty() {
        println!("No provenance data yet. Use `diffy hook` to record events.");
        return Ok(());
    }

    // Build session_id → model lookup
    let session_models: HashMap<ulid::Ulid, String> =
        sessions.iter().map(|s| (s.id, s.model.clone())).collect();

    // Filter events by path if specified
    let filtered_events: Vec<_> = match path {
        Some(p) => events
            .into_iter()
            .filter(|e| e.path.starts_with(p))
            .collect(),
        None => events,
    };

    // Replay to get per-file composition
    let blob_reader = |hash: &str| -> Option<Vec<u8>> { git::read_blob(repo, hash).ok() };

    let files = replay::replay_all(&filtered_events, &blob_reader);

    // Aggregate composition across all files: model → chars
    let mut model_chars: HashMap<String, usize> = HashMap::new();
    let mut total_chars: usize = 0;

    let event_models: HashMap<ulid::Ulid, String> = filtered_events
        .iter()
        .filter_map(|e| session_models.get(&e.session_id).map(|m| (e.id, m.clone())))
        .collect();
    for (origin, chars) in replay::surviving_by_event(&files) {
        let model = replay::model_for_origin(origin, &event_models);
        *model_chars.entry(model).or_insert(0) += chars;
        total_chars += chars;
    }

    if json_output {
        let output = serde_json::json!({
            "path": path.unwrap_or("(repo)"),
            "total_chars": total_chars,
            "files": files.len(),
            "composition": model_chars,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        let label = path.unwrap_or("(entire repo)");
        println!("{label}  ({total_chars} chars, {} files)", files.len());
        println!();

        // Sort by chars descending
        let mut sorted: Vec<_> = model_chars.into_iter().collect();
        sorted.sort_by_key(|&(_, c)| std::cmp::Reverse(c));

        for (model, chars) in &sorted {
            let pct = if total_chars > 0 {
                (*chars as f64 / total_chars as f64) * 100.0
            } else {
                0.0
            };
            println!("  {model:<25} {pct:5.1}%  ({chars} chars)");
        }
    }

    Ok(())
}

/// `diffy blame <file>` — line-level annotated source via blame-join (§7).
fn cmd_blame(repo: &std::path::Path, path: &str) -> Result<()> {
    require_repo(repo)?;
    let lines = blame::blame_join(repo, path)?;
    if lines.is_empty() {
        println!("No blame output for {path}.");
        return Ok(());
    }
    for l in &lines {
        let model = l.model.as_deref().unwrap_or("untracked");
        println!("{:>6}  {:<24}  {}", l.line_no, model, l.content);
    }
    Ok(())
}

/// `diffy checkpoint` — bind recorded events to HEAD via a provenance note (§3, §5).
fn cmd_checkpoint(repo: &std::path::Path) -> Result<()> {
    require_repo(repo)?;
    let events = store::read_all_events(repo)?;
    let sessions = store::read_all_sessions(repo)?;

    if events.is_empty() {
        println!("No provenance events to bind — nothing recorded yet.");
        return Ok(());
    }

    let head =
        git::head(repo)?.context("no HEAD commit — commit something before checkpointing")?;

    // session id → (model, tool)
    let session_info: HashMap<String, (String, String)> = sessions
        .iter()
        .map(|s| (s.id.to_string(), (s.model.clone(), tool_str(&s.tool))))
        .collect();

    // event id → session id (for char→session mapping)
    let event_session: HashMap<ulid::Ulid, String> = events
        .iter()
        .map(|e| (e.id, e.session_id.to_string()))
        .collect();

    let blob_reader = |hash: &str| -> Option<Vec<u8>> { git::read_blob(repo, hash).ok() };
    let files = replay::replay_all(&events, &blob_reader);

    let mut note_files: Vec<(String, Vec<String>)> = Vec::new();
    for fc in &files {
        // Current content = last non-transparent event's after state for this path.
        let content = events
            .iter()
            .rev()
            .find(|e| e.path == fc.path && !e.transparent)
            .and_then(|e| blob_reader(&e.after_blob))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();

        // Char-level session attribution from the piece table.
        let mut char_sessions: Vec<String> = Vec::with_capacity(content.len());
        for span in &fc.doc.spans {
            let sid = event_session.get(&span.origin).cloned().unwrap_or_default();
            char_sessions.resize(char_sessions.len() + span.len, sid);
        }
        char_sessions.resize(content.len(), String::new());

        let line_sessions = store::lines_by_dominant_session(&content, &char_sessions);
        note_files.push((fc.path.clone(), line_sessions));
    }

    let trace = ulid::Ulid::generate().to_string();
    let note = store::build_checkpoint_note(&note_files, &session_info, &trace);
    git::write_note(repo, "provenance", &head, &note)?;
    println!(
        "Checkpoint bound {} files to commit {}",
        note_files.len(),
        &head[..head.len().min(8)]
    );
    Ok(())
}

/// `diffy why <file>:<line>` — which model/session authored a line (§12).
fn cmd_why(repo: &std::path::Path, target: &str) -> Result<()> {
    require_repo(repo)?;
    let (path, line) = target
        .rsplit_once(':')
        .context("target must be `<file>:<line>` (e.g. src/main.rs:120)")?;
    let line_no: usize = line
        .trim()
        .parse()
        .context("line number must be an integer")?;
    anyhow::ensure!(line_no > 0, "line numbers are 1-indexed");

    let events = store::read_all_events(repo)?;
    let sessions = store::read_all_sessions(repo)?;
    let session_models: HashMap<ulid::Ulid, String> =
        sessions.iter().map(|s| (s.id, s.model.clone())).collect();

    let blob_reader = |hash: &str| -> Option<Vec<u8>> { git::read_blob(repo, hash).ok() };
    let files = replay::replay_all(&events, &blob_reader);
    let fc = files
        .iter()
        .find(|fc| fc.path == path)
        .with_context(|| format!("no provenance recorded for {path}"))?;

    // Current content for this path.
    let content = events
        .iter()
        .rev()
        .find(|e| e.path == path && !e.transparent)
        .and_then(|e| blob_reader(&e.after_blob))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();

    let event_session: HashMap<ulid::Ulid, String> = events
        .iter()
        .map(|e| (e.id, e.session_id.to_string()))
        .collect();

    let mut char_sessions: Vec<String> = Vec::with_capacity(content.len());
    for span in &fc.doc.spans {
        let sid = event_session.get(&span.origin).cloned().unwrap_or_default();
        char_sessions.resize(char_sessions.len() + span.len, sid);
    }
    char_sessions.resize(content.len(), String::new());

    let line_sessions = store::lines_by_dominant_session(&content, &char_sessions);
    match line_sessions.get(line_no - 1) {
        Some(sid) if !sid.is_empty() => {
            let model = sid
                .parse::<ulid::Ulid>()
                .ok()
                .and_then(|u| session_models.get(&u).cloned());
            println!(
                "{path}:{line_no}\n  model:   {}\n  session: {sid}",
                model.as_deref().unwrap_or("unknown")
            );
        }
        _ => println!("{path}:{line_no}\n  no attribution (untracked)"),
    }
    Ok(())
}

/// `diffy half-life` — survival curve per model (§8).
fn cmd_half_life(repo: &std::path::Path, model: Option<&str>) -> Result<()> {
    require_repo(repo)?;
    let events = store::read_all_events(repo)?;
    let sessions = store::read_all_sessions(repo)?;

    if events.is_empty() {
        println!("No provenance data yet. Use `diffy hook` to record events.");
        return Ok(());
    }

    let session_models: HashMap<ulid::Ulid, String> =
        sessions.iter().map(|s| (s.id, s.model.clone())).collect();

    let blob_reader = |hash: &str| -> Option<Vec<u8>> { git::read_blob(repo, hash).ok() };
    let files = replay::replay_all(&events, &blob_reader);
    let surviving = replay::surviving_by_event(&files);

    let now = jiff::Timestamp::now().as_second();
    let all_buckets =
        stats::build_emission_buckets(&events, &session_models, &surviving, blob_reader, now);

    // Select one model's buckets, or all models combined.
    let buckets: Vec<stats::EmissionBucket> = match model {
        Some(m) => all_buckets.get(m).cloned().unwrap_or_default(),
        None => all_buckets.values().flatten().cloned().collect(),
    };

    let points = stats::half_life(&buckets);
    let label = model.unwrap_or("(all models)");
    if points.is_empty() {
        println!("{label}: no survival data");
        return Ok(());
    }
    println!("{label}: code survival curve");
    println!("  age_days  survival");
    for p in &points {
        println!("  {:>8}  {:.3}", p.age_days, p.survival);
    }
    match stats::half_life_at(&points) {
        Some(hl) => println!("  half-life: {hl} days"),
        None => println!(
            "  half-life: >{} days (survival never dropped below 50%)",
            points.last().map(|p| p.age_days).unwrap_or(0)
        ),
    }
    Ok(())
}

/// Human-readable name for an agent tool.
fn tool_str(tool: &model::Tool) -> String {
    match tool {
        model::Tool::Cursor => "cursor".to_string(),
        model::Tool::ClaudeCode => "claude-code".to_string(),
        model::Tool::Codex => "codex".to_string(),
        model::Tool::Gemini => "gemini".to_string(),
        model::Tool::Other(s) => s.clone(),
    }
}

#[cfg(test)]
#[allow(non_snake_case)] // test names cite the feature id (`_C1`) per SPEC.md §9.5
mod tests {
    use super::Cli;
    use super::cmd_report;
    use super::hook_script;
    use super::install_cursor_hooks;
    use super::read_payload_arg;
    use super::run_hook;

    #[test]
    fn payload_arg_inline_json_4_1() {
        // §4.1: literal JSON string passes through unchanged
        let json = r#"{"model":"opus","path":"a.rs"}"#;
        let out = read_payload_arg(json, std::io::empty()).unwrap();
        assert_eq!(out, json);
    }

    #[test]
    fn payload_arg_at_file_4_1() {
        // §4.1: @path reads the payload from a file
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("payload.json");
        std::fs::write(&path, r#"{"model":"gpt-5.2"}"#).unwrap();
        let arg = format!("@{}", path.display());
        let out = read_payload_arg(&arg, std::io::empty()).unwrap();
        assert_eq!(out, r#"{"model":"gpt-5.2"}"#);
    }

    #[test]
    fn payload_arg_at_dash_reads_stdin_4_1() {
        // §4.1: @- reads the payload from stdin — this is what the Cursor
        // hook script uses to pipe the payload without a temp file (avoids
        // the race where the script's cleanup deletes the file before the
        // backgrounded diffy process reads it).
        let json = r#"{"file_path":"/repo/a.rs","edits":[]}"#;
        let out = read_payload_arg("@-", json.as_bytes()).unwrap();
        assert_eq!(out, json);
    }

    #[test]
    fn payload_arg_missing_file_errors_4_1() {
        // §4.1 / I4 fail-closed: a missing @file is an error, not empty input
        let res = read_payload_arg("@/nonexistent/payload.json", std::io::empty());
        assert!(res.is_err());
    }

    #[test]
    fn why_help_does_not_overclaim_prompt_12() {
        // §12: `why` reports model + session, not prompt — prompt capture is
        // deferred (§3, §11). The CLI help must not claim it shows a prompt.
        use clap::CommandFactory;
        let cmd = Cli::command();
        let why = cmd.find_subcommand("why").expect("why subcommand exists");
        let about = why.get_about().map(|s| s.to_string()).unwrap_or_default();
        assert!(
            !about.contains("prompt"),
            "why help must not claim prompt capture: {about}"
        );
        assert!(
            about.contains("session"),
            "why help must mention session: {about}"
        );
    }

    // --- C1 CLI: --modules output-target enforcement ---

    #[test]
    fn modules_cli_rejects_neither_json_nor_out_C1() {
        // §9.5 C1: --modules alone is a usage error.
        let err = cmd_report(None, None, false, false, true);
        assert!(err.is_err(), "--modules without --json/--out must error");
    }

    #[test]
    fn modules_cli_rejects_both_json_and_out_C1() {
        // §9.5 C1: --modules --json --out X is a usage error (no silent precedence).
        let err = cmd_report(None, Some("x.json".to_string()), true, false, true);
        assert!(
            err.is_err(),
            "--modules with both --json and --out must error"
        );
    }

    // --- §4.1 init: hooks.json merge is fail-closed ---

    fn write_hooks_json(ws: &std::path::Path, content: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(ws.join(".cursor")).unwrap();
        let path = ws.join(".cursor/hooks.json");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn after_file_edit_commands(path: &std::path::Path) -> Vec<String> {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        v["hooks"]["afterFileEdit"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["command"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn init_preserves_existing_non_diffy_hooks_4_1() {
        let ws = tempfile::TempDir::new().unwrap();
        let path = write_hooks_json(
            ws.path(),
            r#"{"version":1,"hooks":{"afterFileEdit":[{"command":"./lint.sh"}],"stop":[{"command":"./bye.sh"}]}}"#,
        );
        install_cursor_hooks(ws.path(), ws.path(), false).unwrap();

        let cmds = after_file_edit_commands(&path);
        assert_eq!(cmds, vec!["./lint.sh", ".cursor/hooks/diffy-hook.sh"]);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["hooks"]["stop"][0]["command"], "./bye.sh");
    }

    #[test]
    fn init_force_reinstall_does_not_duplicate_diffy_hook_4_1() {
        let ws = tempfile::TempDir::new().unwrap();
        install_cursor_hooks(ws.path(), ws.path(), false).unwrap();
        install_cursor_hooks(ws.path(), ws.path(), true).unwrap();
        let cmds = after_file_edit_commands(&ws.path().join(".cursor/hooks.json"));
        assert_eq!(cmds, vec![".cursor/hooks/diffy-hook.sh"]);
    }

    #[test]
    fn init_refuses_when_already_set_up_4_1() {
        let ws = tempfile::TempDir::new().unwrap();
        install_cursor_hooks(ws.path(), ws.path(), false).unwrap();
        let path = ws.path().join(".cursor/hooks.json");
        let before = std::fs::read_to_string(&path).unwrap();

        let err = install_cursor_hooks(ws.path(), ws.path(), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already set up"), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn init_refuses_when_only_hook_script_exists_4_1() {
        // A leftover script with no hooks.json entry still counts as set up.
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(ws.path().join(".cursor/hooks")).unwrap();
        let script = ws.path().join(".cursor/hooks/diffy-hook.sh");
        std::fs::write(&script, "#!/bin/bash\n# custom\n").unwrap();

        assert!(install_cursor_hooks(ws.path(), ws.path(), false).is_err());
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            "#!/bin/bash\n# custom\n"
        );
        assert!(!ws.path().join(".cursor/hooks.json").exists());
    }

    #[test]
    fn init_invalid_hooks_json_errors_and_leaves_file_untouched_4_1() {
        // §4.1 fail-closed: an unparseable hooks.json may hold the user's
        // other hooks; overwriting it would silently delete them.
        let ws = tempfile::TempDir::new().unwrap();
        let broken = r#"{"version":1,"hooks":{"afterFileEdit":[{"command":"./lint.sh"},]}"#;
        let path = write_hooks_json(ws.path(), broken);

        let err = install_cursor_hooks(ws.path(), ws.path(), false).unwrap_err();
        assert!(
            format!("{err:#}").contains("--force"),
            "error should suggest --force: {err:#}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        assert!(!ws.path().join(".cursor/hooks.json.bak").exists());
    }

    #[test]
    fn init_wrong_shape_hooks_json_errors_and_leaves_file_untouched_4_1() {
        // afterFileEdit must be an array; anything else would previously
        // skip installing our hook while reporting success.
        let ws = tempfile::TempDir::new().unwrap();
        let odd = r#"{"version":1,"hooks":{"afterFileEdit":{"command":"./lint.sh"}}}"#;
        let path = write_hooks_json(ws.path(), odd);

        assert!(install_cursor_hooks(ws.path(), ws.path(), false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), odd);
    }

    #[test]
    fn init_force_replaces_invalid_hooks_json_with_backup_4_1() {
        let ws = tempfile::TempDir::new().unwrap();
        let broken = "not json at all";
        let path = write_hooks_json(ws.path(), broken);

        install_cursor_hooks(ws.path(), ws.path(), true).unwrap();

        assert_eq!(
            after_file_edit_commands(&path),
            vec![".cursor/hooks/diffy-hook.sh"]
        );
        let bak = ws.path().join(".cursor/hooks.json.bak");
        assert_eq!(std::fs::read_to_string(bak).unwrap(), broken);
    }

    // --- §4.1 hook problems are logged, never silent ---

    fn hook_log_lines(repo: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(repo.join(".git/diffy/hook.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn temp_git_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        super::git::init(&repo).unwrap();
        (dir, repo)
    }

    #[test]
    fn run_hook_logs_errors_4_1() {
        let (_dir, repo) = temp_git_repo();
        assert!(run_hook(Some(&repo), r#"{"not":"a payload"}"#, false, None).is_err());

        let lines = hook_log_lines(&repo);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains(" error failed to parse hook payload JSON"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn run_hook_logs_unplaceable_cursor_edit_as_warning_4_1() {
        let (_dir, repo) = temp_git_repo();
        let file = repo.join("lib.rs");
        std::fs::write(&file, "}\n}\n").unwrap();
        let payload = serde_json::json!({
            "file_path": file,
            "edits": [{"old_string": "", "new_string": "}"}],
            "model": "claude-opus-4-6",
            "conversation_id": "conv-1",
        })
        .to_string();

        run_hook(Some(&repo), &payload, true, None).unwrap();

        let lines = hook_log_lines(&repo);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains(" warning lib.rs: couldn't place"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn run_hook_skips_gitignored_file_with_warning_4_1() {
        // A deliberate skip succeeds (nothing for Cursor to act on) and is
        // logged as a warning naming the path, never the contents.
        let (_dir, repo) = temp_git_repo();
        std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        let payload = r#"{"model":"opus","tool":"cursor","session_id":"s","path":".env","before":"","after":"API_KEY=sk-live-SECRET123"}"#;

        run_hook(Some(&repo), payload, false, None).unwrap();

        let lines = hook_log_lines(&repo);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains(" warning skipped .env: ignored by git"),
            "{}",
            lines[0]
        );
        assert!(
            !lines[0].contains("SECRET"),
            "log must not contain file contents"
        );
    }

    #[test]
    fn run_hook_success_logs_nothing_4_1() {
        let (_dir, repo) = temp_git_repo();
        let payload = r#"{"model":"opus","tool":"cursor","session_id":"s","path":"a.rs","before":"","after":"x"}"#;
        run_hook(Some(&repo), payload, false, None).unwrap();
        assert!(hook_log_lines(&repo).is_empty());
    }

    // --- §4.1 generated hook script: bash only, binary fallback ---

    #[test]
    fn hook_script_needs_no_jq_4_1() {
        let script = hook_script(std::path::Path::new("/opt/diffy/bin/diffy"));
        assert!(!script.contains("jq"), "{script}");
        assert!(script.contains("/opt/diffy/bin/diffy"));
        assert!(script.contains("command -v diffy"));
        assert!(script.contains("XDG_STATE_HOME"));
    }

    /// A fake `diffy` that records its args and stdin next to itself.
    #[cfg(unix)]
    fn stub_diffy(dir: &std::path::Path, name: &str, exit: i32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let out = dir.join("called");
        std::fs::write(
            &path,
            format!(
                "#!/bin/bash\necho \"$@\" > '{0}.args'\ncat > '{0}.stdin'\nexit {exit}\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Run the generated script under bash with a controlled environment.
    #[cfg(unix)]
    fn run_script(
        diffy_bin: &std::path::Path,
        path_env: &str,
        state_home: &std::path::Path,
        stdin: &str,
    ) -> std::process::ExitStatus {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("diffy-hook.sh");
        std::fs::write(&script, hook_script(diffy_bin)).unwrap();
        let mut child = std::process::Command::new("/bin/bash")
            .arg(&script)
            .env_clear()
            .env("PATH", path_env)
            .env("HOME", state_home)
            .env("XDG_STATE_HOME", state_home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn hook_script_pipes_payload_to_baked_binary_4_1() {
        let bin = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        let diffy = stub_diffy(bin.path(), "diffy-baked", 0);

        let status = run_script(
            &diffy,
            "/usr/bin:/bin",
            state.path(),
            r#"{"file_path":"/x"}"#,
        );
        assert!(status.success());
        let args = std::fs::read_to_string(bin.path().join("called.args")).unwrap();
        assert_eq!(args.trim(), "hook --cursor --payload @-");
        let stdin = std::fs::read_to_string(bin.path().join("called.stdin")).unwrap();
        assert_eq!(stdin, r#"{"file_path":"/x"}"#);
    }

    #[cfg(unix)]
    #[test]
    fn hook_script_falls_back_to_diffy_on_path_4_1() {
        let bin = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        stub_diffy(bin.path(), "diffy", 0);
        let path_env = format!("{}:/usr/bin:/bin", bin.path().display());

        let status = run_script(
            std::path::Path::new("/moved/away/diffy"),
            &path_env,
            state.path(),
            "{}",
        );
        assert!(status.success());
        assert!(
            bin.path().join("called.args").exists(),
            "diffy on PATH should run"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hook_script_logs_when_no_binary_found_4_1() {
        let state = tempfile::TempDir::new().unwrap();
        let status = run_script(
            std::path::Path::new("/moved/away/diffy"),
            "/usr/bin:/bin",
            state.path(),
            "{}",
        );
        assert!(status.success(), "hook must never fail the editor");

        let log = std::fs::read_to_string(state.path().join("diffy/hook.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 1, "{log:?}");
        let (ts, rest) = lines[0].split_once(' ').unwrap();
        ts.parse::<jiff::Timestamp>()
            .expect("same line format as diffy's log");
        assert!(rest.starts_with("error diffy binary not found"), "{rest}");
        assert!(rest.contains("/moved/away/diffy"), "{rest}");
    }

    #[cfg(unix)]
    #[test]
    fn hook_script_exits_zero_when_diffy_fails_4_1() {
        let bin = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        let diffy = stub_diffy(bin.path(), "diffy-baked", 3);
        assert!(run_script(&diffy, "/usr/bin:/bin", state.path(), "{}").success());
    }

    // --- §4.1 `diffy hook --cursor` finds the repo itself ---

    fn cursor_json(file: &std::path::Path, content: &str) -> String {
        serde_json::json!({
            "file_path": file,
            "edits": [{"old_string": "", "new_string": content}],
            "model": "claude-opus-4-6",
            "conversation_id": "conv-1",
        })
        .to_string()
    }

    #[test]
    fn run_hook_cursor_finds_repo_from_file_path_4_1() {
        let (_dir, repo) = temp_git_repo();
        let fallback = tempfile::TempDir::new().unwrap();
        let fallback_log = fallback.path().join("hook.log");
        std::fs::create_dir(repo.join("sub")).unwrap();
        let file = repo.join("sub/lib.rs");
        std::fs::write(&file, "fn a() {}\n").unwrap();

        run_hook(
            None,
            &cursor_json(&file, "fn a() {}\n"),
            true,
            Some(&fallback_log),
        )
        .unwrap();

        let events = super::store::read_all_events(&repo).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "sub/lib.rs");
        assert!(!fallback_log.exists());
    }

    #[test]
    fn run_hook_cursor_outside_any_repo_is_silent_4_1() {
        let dir = tempfile::TempDir::new().unwrap();
        let fallback_log = dir.path().join("state/hook.log");
        let file = dir.path().join("loose.rs");
        std::fs::write(&file, "x\n").unwrap();

        run_hook(None, &cursor_json(&file, "x\n"), true, Some(&fallback_log)).unwrap();
        assert!(
            !fallback_log.exists(),
            "nothing to record, nothing to report"
        );
    }

    #[test]
    fn run_hook_cursor_payload_without_file_path_logs_to_fallback_4_1() {
        let dir = tempfile::TempDir::new().unwrap();
        let fallback_log = dir.path().join("hook.log");

        assert!(run_hook(None, r#"{"model":"m"}"#, true, Some(&fallback_log)).is_err());
        let log = std::fs::read_to_string(&fallback_log).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(
            log.contains(" error ") && log.contains("file_path"),
            "{log}"
        );
    }

    #[test]
    fn run_hook_unreadable_payload_logs_to_fallback_4_1() {
        let dir = tempfile::TempDir::new().unwrap();
        let fallback_log = dir.path().join("hook.log");

        assert!(
            run_hook(
                None,
                "@/nonexistent/payload.json",
                true,
                Some(&fallback_log)
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&fallback_log)
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn hook_notices_include_fallback_log_4_1() {
        let (_dir, repo) = temp_git_repo();
        let state = tempfile::TempDir::new().unwrap();
        let fallback_log = state.path().join("hook.log");
        assert!(super::hook_notices(&repo, Some(&fallback_log)).is_empty());

        super::hook_log::append_to(
            &fallback_log,
            super::hook_log::Level::Error,
            "diffy binary not found",
        )
        .unwrap();
        super::hook_log::append(&repo, super::hook_log::Level::Warning, "w").unwrap();

        let notices = super::hook_notices(&repo, Some(&fallback_log));
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(
            notices[1].contains(&fallback_log.display().to_string()),
            "{notices:?}"
        );
    }

    // --- §12: commands fail clearly outside a git repo ---

    #[test]
    fn read_commands_fail_outside_a_git_repo_12() {
        let dir = tempfile::TempDir::new().unwrap();
        let not_repo = dir.path();
        let out = not_repo.join("r.html");
        let repo_arg = || Some(not_repo.display().to_string());

        let results = [
            ("comp", super::cmd_comp(not_repo, None, false)),
            ("comp --json", super::cmd_comp(not_repo, None, true)),
            (
                "report",
                cmd_report(
                    repo_arg(),
                    Some(out.display().to_string()),
                    false,
                    false,
                    false,
                ),
            ),
            (
                "report --json",
                cmd_report(repo_arg(), None, true, false, false),
            ),
            (
                "report --modules",
                cmd_report(repo_arg(), None, true, false, true),
            ),
            ("blame", super::cmd_blame(not_repo, "a.rs")),
            ("why", super::cmd_why(not_repo, "a.rs:1")),
            ("half-life", super::cmd_half_life(not_repo, None)),
            ("checkpoint", super::cmd_checkpoint(not_repo)),
        ];
        for (name, result) in results {
            let err = result.expect_err(name);
            let msg = format!("{err:#}");
            assert!(
                msg.contains("is not a git repository") && msg.contains("--repo"),
                "{name}: {msg}"
            );
        }
        assert!(!out.exists(), "no report written for a non-repo");
    }

    #[test]
    fn read_commands_work_in_a_repo_without_init_12() {
        let (_dir, repo) = temp_git_repo();
        super::cmd_comp(&repo, None, false).unwrap();
        super::cmd_half_life(&repo, None).unwrap();
        super::cmd_checkpoint(&repo).unwrap();
    }

    #[test]
    fn init_cli_accepts_force_flag_4_1() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["diffy", "init", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Init { force: true, .. }
        ));
    }

    #[test]
    fn doctor_cli_takes_repo_and_workspace_4_1() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["diffy", "doctor", "--repo", "r", "--workspace", "w"]).unwrap();
        let super::Commands::Doctor { repo, workspace } = cli.command else {
            panic!("not doctor");
        };
        assert_eq!(repo, std::path::Path::new("r"));
        assert_eq!(workspace.as_deref(), Some(std::path::Path::new("w")));
    }
}
