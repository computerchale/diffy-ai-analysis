//! Hook problem log — see SPEC.md §4.1 ("…but never silently").
//!
//! The Cursor hook is fail-open (`|| exit 0`), so a failing `diffy hook`
//! would otherwise lose edits without anyone noticing. Every error and
//! warning is appended to `<git-dir>/diffy/hook.log`, and `comp`/`report`
//! surface a one-line notice while the log is non-empty.
//!
//! Problems that can't be tied to a repo (no diffy binary found, a payload
//! without `file_path`) go to a per-user fallback log instead.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Severity of a logged hook problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
        }
    }
}

/// Path of the hook log for `repo`. Inside the git dir, so it's never
/// committed and works for worktrees.
pub fn log_path(repo: &Path) -> Result<PathBuf> {
    let git_dir = crate::git::run(repo, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(git_dir.trim()).join("diffy/hook.log"))
}

/// Per-user log for problems not tied to a repo:
/// `${XDG_STATE_HOME:-$HOME/.local/state}/diffy/hook.log`. The generated
/// hook script computes the same path, so keep the two in sync.
pub fn fallback_log_path() -> Option<PathBuf> {
    fallback_log_path_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn fallback_log_path_from(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let state = match xdg_state_home.filter(|s| !s.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home.filter(|s| !s.is_empty())?).join(".local/state"),
    };
    Some(state.join("diffy/hook.log"))
}

/// Append one problem to `repo`'s hook log. See [`append_to`].
pub fn append(repo: &Path, level: Level, message: &str) -> Result<()> {
    append_to(&log_path(repo)?, level, message)
}

/// Append one problem as a single line: `<RFC 3339 ts> <level> <message>`.
/// Newlines in the message (e.g. git's stderr) are folded so each problem
/// stays one line and the count in [`summary_of`] stays honest.
pub fn append_to(path: &Path, level: Level, message: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let message = message
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join(" | ");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    writeln!(
        file,
        "{} {} {message}",
        jiff::Timestamp::now(),
        level.as_str()
    )
    .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// One-line notice for stderr when `repo`'s log is non-empty, else `None`.
pub fn summary(repo: &Path) -> Option<String> {
    summary_of(&log_path(repo).ok()?)
}

/// One-line notice for stderr when the log at `path` is non-empty.
pub fn summary_of(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let count = content.lines().filter(|l| !l.trim().is_empty()).count();
    (count > 0).then(|| {
        format!(
            "warning: {count} hook problem(s) logged in {} — some edits may not have been \
             recorded. Delete the file to clear this notice.",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap();
        crate::git::init(&path).unwrap();
        (dir, path)
    }

    #[test]
    fn hook_log_lives_in_git_dir_4_1() {
        let (_dir, repo) = temp_repo();
        assert_eq!(log_path(&repo).unwrap(), repo.join(".git/diffy/hook.log"));
    }

    #[test]
    fn hook_log_appends_one_line_per_problem_4_1() {
        let (_dir, repo) = temp_repo();
        append(&repo, Level::Error, "git failed:\nfatal: bad object").unwrap();
        append(&repo, Level::Warning, "lib.rs: couldn't place 1 edit(s)").unwrap();

        let log = std::fs::read_to_string(log_path(&repo).unwrap()).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "multi-line messages must stay on one line: {log:?}"
        );

        let (ts, rest) = lines[0].split_once(' ').unwrap();
        ts.parse::<jiff::Timestamp>()
            .expect("line starts with an RFC 3339 timestamp");
        assert!(rest.starts_with("error git failed:"), "{rest}");
        assert!(rest.contains("fatal: bad object"), "{rest}");
        assert!(
            lines[1].contains(" warning lib.rs: couldn't place"),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn fallback_log_prefers_xdg_state_home_4_1() {
        use std::ffi::OsStr;
        assert_eq!(
            fallback_log_path_from(Some(OsStr::new("/state")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/state/diffy/hook.log"))
        );
        assert_eq!(
            fallback_log_path_from(None, Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.local/state/diffy/hook.log"))
        );
        // An empty XDG_STATE_HOME counts as unset, per the XDG spec.
        assert_eq!(
            fallback_log_path_from(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.local/state/diffy/hook.log"))
        );
        assert_eq!(fallback_log_path_from(None, None), None);
    }

    #[test]
    fn hook_log_summary_absent_when_no_problems_4_1() {
        let (_dir, repo) = temp_repo();
        assert_eq!(summary(&repo), None);
    }

    #[test]
    fn hook_log_summary_counts_problems_and_names_path_4_1() {
        let (_dir, repo) = temp_repo();
        append(&repo, Level::Error, "one").unwrap();
        append(&repo, Level::Warning, "two").unwrap();

        let notice = summary(&repo).expect("notice when log is non-empty");
        assert!(notice.contains("2 hook problem"), "{notice}");
        assert!(notice.contains(".git/diffy/hook.log"), "{notice}");
    }
}
