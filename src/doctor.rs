//! `diffy doctor` — check that capture is set up and working (SPEC.md §4.1).
//!
//! Read-only. Each check reports ok / warn / fail with a fix, so a hook that
//! silently stopped recording shows up as one red line.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug)]
pub struct Check {
    pub status: Status,
    pub name: &'static str,
    pub detail: String,
}

/// What `doctor` reads from the process environment, passed in so tests
/// don't touch the real one.
pub struct Env<'a> {
    pub fallback_log: Option<&'a Path>,
    pub path_var: Option<&'a OsStr>,
}

fn check(status: Status, name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        status,
        name,
        detail: detail.into(),
    }
}

/// Run every check for `repo` with Cursor hooks expected under `workspace`.
pub fn run(repo: &Path, workspace: &Path, env: &Env) -> Vec<Check> {
    let mut checks = Vec::new();

    let git = check_git();
    let git_failed = git.status == Status::Fail;
    checks.push(git);
    if git_failed {
        return checks;
    }
    let repo_check = check_repo(repo);
    let repo_failed = repo_check.status == Status::Fail;
    checks.push(repo_check);
    if repo_failed {
        return checks;
    }

    checks.push(check_hooks_json(workspace));
    let (script, baked) = check_script(workspace);
    checks.push(script);
    if let Some(baked) = baked {
        checks.push(check_binary(&baked, env.path_var));
    }
    checks.push(match crate::hook_log::log_path(repo) {
        Ok(path) => check_log("hook log", &path),
        Err(err) => check(
            Status::Warn,
            "hook log",
            format!("can't locate it: {err:#}"),
        ),
    });
    checks.push(match env.fallback_log {
        Some(path) => check_log("fallback log", path),
        None => check(
            Status::Ok,
            "fallback log",
            "no state directory ($HOME is unset)",
        ),
    });
    checks.extend(check_events(repo));
    checks
}

pub fn has_failures(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == Status::Fail)
}

/// One line per check: `ok   name: detail`.
pub fn render(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            let status = match c.status {
                Status::Ok => "ok",
                Status::Warn => "warn",
                Status::Fail => "fail",
            };
            format!("{status:<5} {}: {}\n", c.name, c.detail)
        })
        .collect()
}

fn check_git() -> Check {
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => check(
            Status::Ok,
            "git",
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ),
        Ok(out) => check(
            Status::Fail,
            "git",
            format!(
                "`git --version` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ),
        Err(err) => check(
            Status::Fail,
            "git",
            format!("can't run git: {err} — install git"),
        ),
    }
}

fn check_repo(repo: &Path) -> Check {
    match crate::git::toplevel(repo) {
        Ok(Some(root)) => check(Status::Ok, "repo", root.display().to_string()),
        Ok(None) => check(
            Status::Fail,
            "repo",
            format!(
                "{} is not a git repository — run `git init` or pass --repo",
                repo.display()
            ),
        ),
        Err(err) => check(Status::Fail, "repo", format!("{err:#}")),
    }
}

fn check_hooks_json(workspace: &Path) -> Check {
    let path = workspace.join(".cursor/hooks.json");
    let fail = |why: String| check(Status::Fail, "hooks.json", why);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return fail(format!("{} is missing — run `diffy init`", path.display()));
        }
        Err(err) => return fail(format!("can't read {}: {err}", path.display())),
    };
    match serde_json::from_str::<serde_json::Value>(&content) {
        Err(err) => fail(format!(
            "{} is not valid JSON ({err}) — fix it, or `diffy init --force` to replace it",
            path.display()
        )),
        Ok(config) if !crate::has_diffy_entry(&config) => fail(format!(
            "{} has no diffy afterFileEdit entry — run `diffy init --force`",
            path.display()
        )),
        Ok(_) => check(Status::Ok, "hooks.json", path.display().to_string()),
    }
}

/// The script check, plus the binary path baked into it when it has one.
fn check_script(workspace: &Path) -> (Check, Option<PathBuf>) {
    let path = workspace.join(".cursor/hooks/diffy-hook.sh");
    let name = "hook script";
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let why = format!("{} is missing — run `diffy init --force`", path.display());
            return (check(Status::Fail, name, why), None);
        }
        Err(err) => {
            let why = format!("can't read {}: {err}", path.display());
            return (check(Status::Fail, name, why), None);
        }
    };
    if !is_executable(&path) {
        let why = format!(
            "{} is not executable — run `diffy init --force`",
            path.display()
        );
        return (check(Status::Fail, name, why), None);
    }
    let baked = baked_binary(&content);
    let current = baked
        .as_deref()
        .is_some_and(|bin| crate::hook_script(bin) == content);
    let result = if current {
        check(Status::Ok, name, path.display().to_string())
    } else {
        check(
            Status::Warn,
            name,
            format!(
                "{} differs from what this diffy generates (older version or edited) — run `diffy init --force`",
                path.display()
            ),
        )
    };
    (result, baked)
}

/// The `BAKED='…'` path from a generated script, undoing `shell_quote`.
fn baked_binary(script: &str) -> Option<PathBuf> {
    let quoted = script.lines().find_map(|l| l.strip_prefix("BAKED="))?;
    let inner = quoted.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(PathBuf::from(inner.replace(r"'\''", "'")))
}

fn check_binary(baked: &Path, path_var: Option<&OsStr>) -> Check {
    let name = "diffy binary";
    if is_executable(baked) {
        return check(Status::Ok, name, baked.display().to_string());
    }
    let on_path = path_var.and_then(|p| {
        std::env::split_paths(p)
            .map(|dir| dir.join("diffy"))
            .find(|candidate| is_executable(candidate))
    });
    match on_path {
        Some(found) => check(
            Status::Warn,
            name,
            format!(
                "{} is gone; the hook falls back to {} if Cursor's PATH has it — run `diffy init --force` to bake in a stable path",
                baked.display(),
                found.display()
            ),
        ),
        None => check(
            Status::Fail,
            name,
            format!(
                "{} is gone and there's no diffy on PATH, so edits aren't being recorded — `cargo install --path .`, then `diffy init --force`",
                baked.display()
            ),
        ),
    }
}

fn check_log(name: &'static str, path: &Path) -> Check {
    let entries = std::fs::read_to_string(path)
        .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    if entries == 0 {
        check(Status::Ok, name, "no problems logged")
    } else {
        check(
            Status::Warn,
            name,
            format!(
                "{entries} problem(s) in {} — read it, then delete it to clear",
                path.display()
            ),
        )
    }
}

fn check_events(repo: &Path) -> Vec<Check> {
    let events = match crate::store::read_all_events(repo) {
        Ok(events) => events,
        Err(err) => return vec![check(Status::Fail, "events", format!("{err:#}"))],
    };
    let Some(latest) = events.last() else {
        return vec![
            check(
                Status::Warn,
                "events",
                "none recorded yet — make an AI edit in Cursor, then run doctor again",
            ),
            check(Status::Ok, "event paths", "nothing to check"),
        ];
    };
    let summary = check(
        Status::Ok,
        "events",
        format!(
            "{} event(s), latest {} ({})",
            events.len(),
            latest.ts,
            latest.path
        ),
    );
    let bad = events
        .iter()
        .filter(|e| crate::capture::validate_rel_path(&e.path).is_err())
        .count();
    let paths = if bad == 0 {
        check(Status::Ok, "event paths", "all repo-relative")
    } else {
        check(
            Status::Warn,
            "event paths",
            format!(
                "{bad} of {} event(s) have absolute or `..` paths (recorded by an older diffy); those files' attribution won't match",
                events.len()
            ),
        )
    };
    vec![summary, paths]
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{capture, hook_log, store};

    fn temp_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        crate::git::init(&repo).unwrap();
        (dir, repo)
    }

    /// Fallback log path and an empty PATH dir, both inside `dir`.
    struct TestEnv {
        _dir: tempfile::TempDir,
        fallback_log: PathBuf,
        path_dir: PathBuf,
    }

    impl TestEnv {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let path_dir = dir.path().join("bin");
            std::fs::create_dir(&path_dir).unwrap();
            Self {
                fallback_log: dir.path().join("state/hook.log"),
                path_dir,
                _dir: dir,
            }
        }

        fn run(&self, repo: &Path, workspace: &Path) -> Vec<Check> {
            run(
                repo,
                workspace,
                &Env {
                    fallback_log: Some(&self.fallback_log),
                    path_var: Some(self.path_dir.as_os_str()),
                },
            )
        }
    }

    fn record_edit(repo: &Path) {
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        capture::process_hook(
            repo,
            &capture::HookPayload {
                model: "claude-opus-4-6".into(),
                tool: "cursor".into(),
                session_id: "s1".into(),
                path: "a.rs".into(),
                before: String::new(),
                after: "fn a() {}\n".into(),
                tool_call_id: None,
                prompt: None,
            },
        )
        .unwrap();
    }

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no {name:?} check in {checks:#?}"))
    }

    fn script_path(ws: &Path) -> PathBuf {
        ws.join(".cursor/hooks/diffy-hook.sh")
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn healthy_setup_passes_every_check_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        record_edit(&repo);

        let checks = TestEnv::new().run(&repo, &repo);
        let names: Vec<_> = checks.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "git",
                "repo",
                "hooks.json",
                "hook script",
                "diffy binary",
                "hook log",
                "fallback log",
                "events",
                "event paths",
            ]
        );
        assert!(checks.iter().all(|c| c.status == Status::Ok), "{checks:#?}");
        assert!(find(&checks, "events").detail.starts_with("1 event"));
        assert!(!has_failures(&checks));
    }

    #[test]
    fn not_a_repo_fails_and_skips_the_rest_4_1() {
        let dir = tempfile::TempDir::new().unwrap();
        let checks = TestEnv::new().run(dir.path(), dir.path());
        assert_eq!(checks.len(), 2, "{checks:#?}");
        assert_eq!(find(&checks, "repo").status, Status::Fail);
        assert!(has_failures(&checks));
    }

    #[test]
    fn missing_setup_fails_with_init_hint_4_1() {
        let (_dir, repo) = temp_repo();
        let checks = TestEnv::new().run(&repo, &repo);

        let hooks_json = find(&checks, "hooks.json");
        assert_eq!(hooks_json.status, Status::Fail);
        assert!(hooks_json.detail.contains("diffy init"), "{hooks_json:?}");
        assert_eq!(find(&checks, "hook script").status, Status::Fail);
        assert!(
            !checks.iter().any(|c| c.name == "diffy binary"),
            "no script → no baked path to check"
        );
        assert_eq!(find(&checks, "events").status, Status::Warn);
    }

    #[test]
    fn hooks_json_without_diffy_entry_fails_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        std::fs::write(
            repo.join(".cursor/hooks.json"),
            r#"{"version":1,"hooks":{"afterFileEdit":[{"command":"other.sh"}]}}"#,
        )
        .unwrap();

        let check = TestEnv::new().run(&repo, &repo);
        let check = find(&check, "hooks.json");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("--force"), "{check:?}");
    }

    #[test]
    fn invalid_hooks_json_fails_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        std::fs::write(repo.join(".cursor/hooks.json"), "{ nope").unwrap();

        let checks = TestEnv::new().run(&repo, &repo);
        assert_eq!(find(&checks, "hooks.json").status, Status::Fail);
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_script_fails_4_1() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        std::fs::set_permissions(script_path(&repo), std::fs::Permissions::from_mode(0o644))
            .unwrap();

        let checks = TestEnv::new().run(&repo, &repo);
        let check = find(&checks, "hook script");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("executable"), "{check:?}");
    }

    #[cfg(unix)]
    #[test]
    fn old_jq_script_warns_outdated_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        std::fs::write(
            script_path(&repo),
            "#!/bin/bash\nINPUT=$(cat)\nFILE_PATH=$(echo \"$INPUT\" | jq -r '.file_path')\n",
        )
        .unwrap();
        make_executable(&script_path(&repo));

        let checks = TestEnv::new().run(&repo, &repo);
        let check = find(&checks, "hook script");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("diffy init --force"), "{check:?}");
        assert!(!checks.iter().any(|c| c.name == "diffy binary"));
    }

    #[test]
    fn edited_script_warns_outdated_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        let mut script = std::fs::read_to_string(script_path(&repo)).unwrap();
        script.push_str("echo hi\n");
        std::fs::write(script_path(&repo), script).unwrap();

        let checks = TestEnv::new().run(&repo, &repo);
        assert_eq!(find(&checks, "hook script").status, Status::Warn);
        assert_eq!(find(&checks, "diffy binary").status, Status::Ok);
    }

    #[cfg(unix)]
    #[test]
    fn moved_binary_with_diffy_on_path_warns_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        let moved = Path::new("/moved/away/diffy");
        std::fs::write(script_path(&repo), crate::hook_script(moved)).unwrap();
        let env = TestEnv::new();
        let on_path = env.path_dir.join("diffy");
        std::fs::write(&on_path, "#!/bin/bash\n").unwrap();
        make_executable(&on_path);

        let checks = env.run(&repo, &repo);
        assert_eq!(find(&checks, "hook script").status, Status::Ok);
        let check = find(&checks, "diffy binary");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("/moved/away/diffy"), "{check:?}");
        assert!(
            check.detail.contains(&on_path.display().to_string()),
            "{check:?}"
        );
    }

    #[test]
    fn moved_binary_without_path_fallback_fails_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        std::fs::write(
            script_path(&repo),
            crate::hook_script(Path::new("/moved/away/diffy")),
        )
        .unwrap();

        let checks = TestEnv::new().run(&repo, &repo);
        let check = find(&checks, "diffy binary");
        assert_eq!(check.status, Status::Fail);
        assert!(has_failures(&checks));
    }

    #[test]
    fn baked_path_with_quote_round_trips_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        std::fs::write(
            script_path(&repo),
            crate::hook_script(Path::new("/it's gone/diffy")),
        )
        .unwrap();

        let checks = TestEnv::new().run(&repo, &repo);
        assert_eq!(find(&checks, "hook script").status, Status::Ok);
        assert!(
            find(&checks, "diffy binary")
                .detail
                .contains("/it's gone/diffy")
        );
    }

    #[test]
    fn logged_problems_warn_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        record_edit(&repo);
        let env = TestEnv::new();
        hook_log::append(&repo, hook_log::Level::Warning, "w").unwrap();
        hook_log::append_to(&env.fallback_log, hook_log::Level::Error, "e").unwrap();

        let checks = env.run(&repo, &repo);
        assert_eq!(find(&checks, "hook log").status, Status::Warn);
        let fallback = find(&checks, "fallback log");
        assert_eq!(fallback.status, Status::Warn);
        assert!(
            fallback
                .detail
                .contains(&env.fallback_log.display().to_string())
        );
        assert!(!has_failures(&checks), "warnings don't fail");
    }

    #[test]
    fn absolute_event_paths_warn_4_1() {
        let (_dir, repo) = temp_repo();
        crate::install_cursor_hooks(&repo, &repo, false).unwrap();
        record_edit(&repo);
        let session = store::read_all_sessions(&repo).unwrap().remove(0);
        let mut event = store::read_all_events(&repo).unwrap().remove(0);
        event.id = ulid::Ulid::generate();
        event.path = "/Users/someone/project/a.rs".into();
        store::append_events(&repo, &session, &[event]).unwrap();

        let checks = TestEnv::new().run(&repo, &repo);
        let check = find(&checks, "event paths");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.starts_with("1 of 2"), "{check:?}");
    }

    #[test]
    fn render_prints_one_line_per_check_4_1() {
        let checks = [
            Check {
                status: Status::Ok,
                name: "git",
                detail: "git version 2.50".into(),
            },
            Check {
                status: Status::Fail,
                name: "repo",
                detail: "not a git repository".into(),
            },
        ];
        assert_eq!(
            render(&checks),
            "ok    git: git version 2.50\nfail  repo: not a git repository\n"
        );
    }
}
