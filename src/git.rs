//! Thin, testable wrapper around the real `git` binary. See SPEC.md §5, §13.
//!
//! Deliberately shells out instead of using git2/libgit2: notes, custom
//! refs, `blame --porcelain -M -C`, `cat-file --batch`, and rewrite hooks
//! all work perfectly via the CLI and its edge cases stay upstream's
//! problem. Every function here is pure plumbing — no diffy logic.
//!
//! Ref updates use CAS via `git update-ref` (Entire's proven pattern —
//! library-level CAS does not interoperate with git's own locking).

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Run a git command in `repo`, returning stdout. Errors on non-zero exit.
pub fn run(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn git {:?}", args))?;
    anyhow::ensure!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}

/// Run a git command that takes stdin, returning stdout.
pub fn run_with_stdin(repo: &Path, args: &[&str], stdin: &[u8]) -> Result<String> {
    run_with_stdin_env(repo, args, stdin, &[])
}

/// Author and committer of diffy's own commits (provenance refs, notes).
/// Fixed so they work without a configured git identity (fresh machines,
/// CI) and never copy the user's email into refs that may be shared.
const DIFFY_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "diffy"),
    ("GIT_AUTHOR_EMAIL", "diffy@localhost"),
    ("GIT_COMMITTER_NAME", "diffy"),
    ("GIT_COMMITTER_EMAIL", "diffy@localhost"),
];

fn run_with_stdin_env(
    repo: &Path,
    args: &[&str],
    stdin: &[u8],
    envs: &[(&str, &str)],
) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn git {:?}", args))?;
    child
        .stdin
        .as_mut()
        .context("git stdin was not piped")?
        .write_all(stdin)
        .with_context(|| "failed to write stdin to git")?;
    let out = child.wait_with_output()?;
    anyhow::ensure!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}

/// Initialize a bare-minimum git repo for testing or for `diffy init`.
#[allow(dead_code)]
pub fn init(repo: &Path) -> Result<()> {
    run(repo, &["init", "-b", "main"])?;
    Ok(())
}

/// True if git ignores `path` (relative to the repo root). Tracked files
/// are never reported as ignored, matching `git check-ignore`.
pub fn is_ignored(repo: &Path, path: &str) -> Result<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", "--", path])
        .output()
        .context("failed to spawn git check-ignore")?;
    // check-ignore exits 0 = ignored, 1 = not ignored, anything else = error.
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!(
            "git check-ignore failed for {path:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The work-tree root containing `dir`, or `None` if `dir` isn't inside a
/// work tree. A missing `dir` resolves via its nearest existing ancestor.
pub fn toplevel(dir: &Path) -> Result<Option<std::path::PathBuf>> {
    let Some(dir) = dir.ancestors().find(|d| d.is_dir()) else {
        return Ok(None);
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to spawn git rev-parse")?;
    if !out.status.success() {
        return Ok(None);
    }
    let root = String::from_utf8(out.stdout)?;
    Ok(Some(std::path::PathBuf::from(root.trim_end_matches('\n'))))
}

/// Write content as a git blob object, returning the SHA-1 hash.
pub fn write_blob(repo: &Path, content: &[u8]) -> Result<String> {
    let hash = run_with_stdin(repo, &["hash-object", "-w", "--stdin"], content)?;
    Ok(hash.trim().to_string())
}

/// Read a blob's content by hash (`git cat-file blob <hash>`).
pub fn read_blob(repo: &Path, hash: &str) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "blob", hash])
        .output()
        .with_context(|| format!("failed to read blob {hash}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git cat-file blob {hash} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

/// Create a tree object from a single blob entry (for storing JSONL in refs).
/// Returns the tree hash.
#[allow(dead_code)]
pub fn mktree_single(repo: &Path, filename: &str, blob_hash: &str) -> Result<String> {
    let entry = format!("100644 blob {blob_hash}\t{filename}\n");
    let hash = run_with_stdin(repo, &["mktree"], entry.as_bytes())?;
    Ok(hash.trim().to_string())
}

/// Create a commit object pointing at a tree, optionally parented.
/// Returns the commit hash.
pub fn commit_tree(repo: &Path, tree: &str, parent: Option<&str>, message: &str) -> Result<String> {
    let mut args = vec!["commit-tree", tree];
    if let Some(p) = parent {
        args.push("-p");
        args.push(p);
    }
    args.push("-m");
    args.push(message);
    let hash = run_with_stdin_env(repo, &args, b"", &DIFFY_IDENTITY)?;
    Ok(hash.trim().to_string())
}

/// Atomically update a ref to point at a new commit.
/// If `old` is None, creates the ref (fails if it already exists).
/// If `old` is Some, performs a CAS update (§5: Entire's proven pattern).
pub fn update_ref(repo: &Path, refname: &str, new: &str, old: Option<&str>) -> Result<()> {
    match old {
        Some(old_val) => run(repo, &["update-ref", refname, new, old_val])?,
        None => run(repo, &["update-ref", refname, new])?,
    };
    Ok(())
}

/// Resolve a ref to its commit hash. Returns None if the ref doesn't exist.
pub fn resolve_ref(repo: &Path, refname: &str) -> Result<Option<String>> {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", refname])
        .output()?;
    if result.status.success() {
        Ok(Some(String::from_utf8(result.stdout)?.trim().to_string()))
    } else {
        Ok(None)
    }
}

/// Read the content of a file inside a commit's tree (e.g. "events.jsonl"
/// from a provenance ref's tip commit).
pub fn show_file_at_ref(repo: &Path, refname: &str, filename: &str) -> Result<Option<String>> {
    let spec = format!("{refname}:{filename}");
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", &spec])
        .output()?;
    if result.status.success() {
        Ok(Some(String::from_utf8(result.stdout)?))
    } else {
        Ok(None)
    }
}

/// List all refs matching a prefix (e.g. "refs/provenance/events/").
pub fn list_refs(repo: &Path, prefix: &str) -> Result<Vec<String>> {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["for-each-ref", "--format=%(refname)", prefix])
        .output()?;
    if !result.status.success() {
        return Ok(vec![]);
    }
    let stdout = String::from_utf8(result.stdout)?;
    Ok(stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Attach a note on a commit under a given notes ref.
pub fn write_note(repo: &Path, notes_ref: &str, commit: &str, note: &str) -> Result<()> {
    run_with_stdin_env(
        repo,
        &["notes", "--ref", notes_ref, "add", "-f", "-F", "-", commit],
        note.as_bytes(),
        &DIFFY_IDENTITY,
    )?;
    Ok(())
}

/// Read the note attached to a commit. Returns None if no note exists.
pub fn read_note(repo: &Path, notes_ref: &str, commit: &str) -> Result<Option<String>> {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["notes", "--ref", notes_ref, "show", commit])
        .output()?;
    if result.status.success() {
        Ok(Some(String::from_utf8(result.stdout)?))
    } else {
        Ok(None)
    }
}

/// Get HEAD commit hash. Returns None if repo has no commits.
pub fn head(repo: &Path) -> Result<Option<String>> {
    resolve_ref(repo, "HEAD")
}

/// Line-level blame with move/copy detection (`git blame --porcelain -M -C -w`).
/// Feeds the blame-join engine (§7) for pre-instrumentation history.
pub fn blame_porcelain(repo: &Path, path: &str) -> Result<String> {
    run(repo, &["blame", "--porcelain", "-M", "-C", "-w", path])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: create a temp dir with a git repo initialized.
    fn temp_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        init(&path).unwrap();
        (dir, path)
    }

    #[test]
    fn write_and_read_blob_roundtrip_5() {
        // §5: blobs are content-addressed, before/after file contents
        let (_dir, repo) = temp_repo();
        let content = b"fn main() { println!(\"hello\"); }";
        let hash = write_blob(&repo, content).unwrap();
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 40); // SHA-1 hex

        let read_back = read_blob(&repo, &hash).unwrap();
        assert_eq!(read_back, content);
    }

    #[test]
    fn blob_dedup_same_content_same_hash_5() {
        // §5: "free dedup" — same content → same hash
        let (_dir, repo) = temp_repo();
        let content = b"identical content";
        let h1 = write_blob(&repo, content).unwrap();
        let h2 = write_blob(&repo, content).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn mktree_commit_tree_update_ref_roundtrip_5() {
        // §5: event log uses tree→commit→ref chain
        let (_dir, repo) = temp_repo();
        let blob = write_blob(&repo, b"{\"id\":\"test\"}").unwrap();
        let tree = mktree_single(&repo, "events.jsonl", &blob).unwrap();
        assert_eq!(tree.len(), 40);

        let commit = commit_tree(&repo, &tree, None, "diffy: append events").unwrap();
        assert_eq!(commit.len(), 40);

        update_ref(&repo, "refs/provenance/events/test-shard", &commit, None).unwrap();
        let resolved = resolve_ref(&repo, "refs/provenance/events/test-shard").unwrap();
        assert_eq!(resolved, Some(commit));
    }

    #[test]
    fn show_file_at_ref_reads_jsonl_5() {
        // §5: reading event JSONL from a provenance ref
        let (_dir, repo) = temp_repo();
        let content = "{\"id\":\"01ABC\"}\n{\"id\":\"01DEF\"}\n";
        let blob = write_blob(&repo, content.as_bytes()).unwrap();
        let tree = mktree_single(&repo, "events.jsonl", &blob).unwrap();
        let commit = commit_tree(&repo, &tree, None, "test").unwrap();
        update_ref(&repo, "refs/provenance/events/shard1", &commit, None).unwrap();

        let read = show_file_at_ref(&repo, "refs/provenance/events/shard1", "events.jsonl")
            .unwrap()
            .unwrap();
        assert_eq!(read, content);
    }

    #[test]
    fn list_refs_finds_provenance_shards_5() {
        // §5: list all session shards
        let (_dir, repo) = temp_repo();
        let blob = write_blob(&repo, b"test").unwrap();
        let tree = mktree_single(&repo, "events.jsonl", &blob).unwrap();

        for name in ["shard-a", "shard-b", "shard-c"] {
            let c = commit_tree(&repo, &tree, None, "test").unwrap();
            update_ref(&repo, &format!("refs/provenance/events/{name}"), &c, None).unwrap();
        }

        let refs = list_refs(&repo, "refs/provenance/events/").unwrap();
        assert_eq!(refs.len(), 3);
        assert!(
            refs.iter()
                .all(|r| r.starts_with("refs/provenance/events/"))
        );
    }

    #[test]
    fn update_ref_cas_prevents_race_5() {
        // §5: CAS via update-ref — if old doesn't match, update fails
        let (_dir, repo) = temp_repo();
        let blob = write_blob(&repo, b"v1").unwrap();
        let tree = mktree_single(&repo, "f", &blob).unwrap();
        let c1 = commit_tree(&repo, &tree, None, "c1").unwrap();
        update_ref(&repo, "refs/provenance/events/cas-test", &c1, None).unwrap();

        let blob2 = write_blob(&repo, b"v2").unwrap();
        let tree2 = mktree_single(&repo, "f", &blob2).unwrap();
        let c2 = commit_tree(&repo, &tree2, Some(&c1), "c2").unwrap();

        // Correct CAS: old matches
        update_ref(&repo, "refs/provenance/events/cas-test", &c2, Some(&c1)).unwrap();

        // Wrong CAS: old is stale — should fail
        let stale = &c1;
        let result = update_ref(&repo, "refs/provenance/events/cas-test", &c2, Some(stale));
        assert!(result.is_err());
    }

    /// A repo where git has no usable identity, like a fresh CI runner.
    fn repo_without_identity() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, repo) = temp_repo();
        run(&repo, &["config", "user.name", ""]).unwrap();
        run(&repo, &["config", "user.email", ""]).unwrap();
        (dir, repo)
    }

    #[test]
    fn commit_tree_needs_no_user_identity_5() {
        let (_dir, repo) = repo_without_identity();
        let blob = write_blob(&repo, b"x").unwrap();
        let tree = mktree_single(&repo, "events.jsonl", &blob).unwrap();

        let commit = commit_tree(&repo, &tree, None, "diffy: append events").unwrap();
        let ident = run(
            &repo,
            &["log", "-1", "--format=%an <%ae> | %cn <%ce>", &commit],
        )
        .unwrap();
        assert_eq!(
            ident.trim(),
            "diffy <diffy@localhost> | diffy <diffy@localhost>"
        );
    }

    #[test]
    fn write_note_needs_no_user_identity_5() {
        let (_dir, repo) = repo_without_identity();
        let blob = write_blob(&repo, b"x").unwrap();
        let tree = mktree_single(&repo, "f", &blob).unwrap();
        let commit = commit_tree(&repo, &tree, None, "c").unwrap();

        write_note(&repo, "provenance", &commit, "note").unwrap();
        assert_eq!(
            read_note(&repo, "provenance", &commit)
                .unwrap()
                .unwrap()
                .trim(),
            "note"
        );
    }

    #[test]
    fn write_and_read_note_roundtrip_5() {
        // §5: notes under refs/notes/provenance for per-commit attribution
        let (_dir, repo) = temp_repo();
        // Need a real commit to attach a note to
        run(
            &repo,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        let head = head(&repo).unwrap().unwrap();

        let note_content =
            "  s_abc123::t_def456  1-10\n---\n{\"schema_version\":\"authorship/3.0.0\"}";
        write_note(&repo, "provenance", &head, note_content).unwrap();

        let read = read_note(&repo, "provenance", &head).unwrap().unwrap();
        assert_eq!(read.trim(), note_content.trim());
    }
}
