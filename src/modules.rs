//! Module-boundary detection and per-module aggregation. See
//! `SPEC.md` §9.5: module resolution + per-module report (B1) and the
//! stable module-provenance export (C1).
//!
//! diffy aggregates at char / file / *top-level-directory* granularity
//! (`stats::affinity` uses the first path segment). This adds the
//! **module** — the architectural unit (a workspace member, a package, a Go
//! module) — by mapping each file to a module id via its nearest
//! package-bearing manifest, then aggregating composition/drift per module.

use crate::stats;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The versioned schema tag emitted by the C1 module export (§9.5 C1).
pub const MODULE_SCHEMA_VERSION: &str = "diffy-modules/1.0";

/// Per-file inputs to module aggregation (§9.5 B1): the subset of a report's
/// per-file data that a module needs. Produced by `report::build_module_export`
/// from the flat per-file report, so the reconciliation invariant
/// (`sum(module total_chars) == repo_total`) holds by construction.
#[derive(Debug, Clone)]
pub struct ModuleFile {
    pub path: String,
    pub total_chars: usize,
    pub composition: HashMap<String, usize>,
    pub drifted: bool,
    pub all_untracked: bool,
}

/// One module's aggregated provenance report (§9.5 B1).
#[derive(Debug, Clone, Serialize)]
pub struct ModuleReport {
    pub id: String,
    /// Member files with `total_chars > 0` (§9.5 B1 — excludes empty replayed
    /// files and empty untracked files alike).
    pub file_count: usize,
    pub total_chars: usize,
    /// model (or `"untracked"`/`"unknown"`) → surviving chars. Stored in a
    /// `BTreeMap` so serialization order is deterministic (§9.5 C1).
    pub composition: BTreeMap<String, usize>,
    /// `stats::dominant_model` over `composition`; `None` for a zero-char
    /// module (omitted from JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dominant_model: Option<String>,
    /// True iff any member file is drifted (§9.5 B1 — OR over members).
    pub drifted: bool,
    /// True iff every member file is `all_untracked` (never had provenance).
    pub fully_untracked: bool,
}

/// The C1 stable module-provenance export (§9.5 C1).
#[derive(Debug, Clone, Serialize)]
pub struct ModuleExport {
    pub schema_version: String,
    pub generated_at: String,
    pub repo_total_chars: usize,
    pub modules: Vec<ModuleReport>,
}

/// Does a `Cargo.toml` contain a top-level `[package]` header (§9.5 B1)?
///
/// Line-exact: a trimmed line equal to `[package]` — not `[package.*]`, not a
/// `#` comment, and no full TOML parsing. A `[workspace]`-only manifest has no
/// `[package]` line and therefore does **not** qualify (the walk continues up).
pub fn cargo_toml_has_package(content: &str) -> bool {
    content.lines().any(|line| line.trim() == "[package]")
}

/// Does a `package.json` declare a module (§9.5 B1)?
///
/// Qualifies when it has a top-level `name` string field and is **not** a
/// workspace root (`private: true` **and** a `workspaces` field). A private
/// package without `workspaces` is still a module; pnpm/Yarn workspace roots
/// without `private: true` are accepted as modules (documented v1
/// simplification).
pub fn package_json_is_module(content: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return false;
    };
    let Some(name) = value.get("name") else {
        return false;
    };
    if !name.is_string() {
        return false;
    }
    let private = value
        .get("private")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_workspaces = value.get("workspaces").is_some();
    !(private && has_workspaces)
}

/// The no-manifest fallback (§9.5 B1): the file's top-level directory (its
/// first path segment), or `(root)` for repo-root files — matching
/// `stats::affinity`.
pub fn top_level_dir(path: &str) -> String {
    path.split_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_else(|| "(root)".to_string())
}

/// Map a file path to a module id (§9.5 B1).
///
/// Walks up from the file's directory to `repo`, returning the first
/// package-bearing manifest's directory as the module id (`"."` for a
/// repo-root manifest). Workspace-root manifests are skipped. Falls back to
/// [`top_level_dir`] when no manifest is found.
///
/// Deterministic per on-disk state: it reads *live worktree* manifests, not
/// committed objects (§9.5 B1 — a deliberate simplicity choice, analogous to
/// SPEC.md I3's preference, not a violation).
pub fn resolve_module(repo: &Path, path: &str) -> String {
    let file_path = Path::new(path);
    let mut dir: PathBuf = match file_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => repo.join(p),
        _ => repo.to_path_buf(),
    };
    loop {
        if let Some(module_id) = detect_manifest(&dir, repo) {
            return module_id;
        }
        if dir == repo {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    top_level_dir(path)
}

/// Look for a package-bearing manifest in `dir`, returning its module id.
///
/// Checks manifest types in the declared precedence order (§9.5 B1 scope):
/// `Cargo.toml` > `package.json` > `go.mod` > `pyproject.toml` > `setup.py`.
fn detect_manifest(dir: &Path, repo: &Path) -> Option<String> {
    if let Ok(content) = std::fs::read_to_string(dir.join("Cargo.toml"))
        && cargo_toml_has_package(&content)
    {
        return Some(module_id_for(dir, repo));
    }
    if let Ok(content) = std::fs::read_to_string(dir.join("package.json"))
        && package_json_is_module(&content)
    {
        return Some(module_id_for(dir, repo));
    }
    if dir.join("go.mod").is_file() {
        return Some(module_id_for(dir, repo));
    }
    if dir.join("pyproject.toml").is_file() {
        return Some(module_id_for(dir, repo));
    }
    if dir.join("setup.py").is_file() {
        return Some(module_id_for(dir, repo));
    }
    None
}

/// Module id for a manifest directory: `"."` for the repo root, else the
/// directory's repo-relative path (§9.5 B1).
fn module_id_for(dir: &Path, repo: &Path) -> String {
    if dir == repo {
        ".".to_string()
    } else {
        dir.strip_prefix(repo)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string())
    }
}

/// Aggregate per-file provenance into per-module reports (§9.5 B1).
///
/// `files` is the union that feeds `repo_total` (replayed ∪ untracked), so
/// summing the returned modules' `total_chars` equals the repo total exactly
/// (the reconciliation invariant). Modules are returned sorted by `id`
/// ascending (a `BTreeMap` key iteration).
pub fn build_module_reports(repo: &Path, files: &[ModuleFile]) -> Vec<ModuleReport> {
    struct Accum {
        file_count: usize,
        total_chars: usize,
        composition: BTreeMap<String, usize>,
        drifted: bool,
        fully_untracked: bool,
    }

    let mut accums: BTreeMap<String, Accum> = BTreeMap::new();
    for f in files {
        let id = resolve_module(repo, &f.path);
        let acc = accums.entry(id).or_insert(Accum {
            file_count: 0,
            total_chars: 0,
            composition: BTreeMap::new(),
            drifted: false,
            fully_untracked: true,
        });
        if f.total_chars > 0 {
            acc.file_count += 1;
        }
        acc.total_chars += f.total_chars;
        for (model, chars) in &f.composition {
            *acc.composition.entry(model.clone()).or_insert(0) += chars;
        }
        acc.drifted |= f.drifted;
        acc.fully_untracked &= f.all_untracked;
    }

    accums
        .into_iter()
        .map(|(id, acc)| {
            let comp_hash: HashMap<String, usize> = acc
                .composition
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect();
            ModuleReport {
                id,
                file_count: acc.file_count,
                total_chars: acc.total_chars,
                dominant_model: stats::dominant_model(&comp_hash),
                composition: acc.composition,
                drifted: acc.drifted,
                fully_untracked: acc.fully_untracked,
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(non_snake_case)] // test names cite the feature id (`_B1`) per SPEC.md §9.5
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    // --- §9.5 B1 manifest detection predicates ---

    #[test]
    fn cargo_toml_package_header_is_line_exact_B1() {
        // §9.5 B1: a trimmed line exactly equal to `[package]` qualifies; a
        // `[package.*]` header, a comment, or a workspace-only manifest does not.
        assert!(cargo_toml_has_package("[package]\nname = \"x\"\n"));
        assert!(cargo_toml_has_package("  [package]  \n"));
        assert!(cargo_toml_has_package(
            "[workspace]\nmembers = []\n[package]\nname = \"x\"\n"
        ));
        assert!(!cargo_toml_has_package("[package.metadata]\n"));
        assert!(!cargo_toml_has_package("# [package]\n"));
        assert!(!cargo_toml_has_package("[workspace]\nmembers = []\n"));
    }

    #[test]
    fn package_json_workspace_root_detection_B1() {
        // §9.5 B1: name string + not a workspace root qualifies; private without
        // workspaces still qualifies; a workspace root (private + workspaces)
        // does not; a non-string name does not.
        assert!(package_json_is_module(r#"{"name": "x"}"#));
        assert!(package_json_is_module(r#"{"name": "x", "private": true}"#));
        assert!(package_json_is_module(
            r#"{"name": "x", "workspaces": ["p/*"]}"#
        ));
        assert!(!package_json_is_module(
            r#"{"private": true, "workspaces": ["p/*"]}"#
        ));
        assert!(!package_json_is_module(
            r#"{"name": "x", "private": true, "workspaces": ["p/*"]}"#
        ));
        assert!(!package_json_is_module(r#"{"name": 42}"#));
    }

    // --- §9.5 B1 module resolver ---

    #[test]
    fn module_resolver_uses_nearest_manifest_B1() {
        // §9.5 B1: nearest package-bearing manifest wins, not the root.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(&root.join("Cargo.toml"), "[package]\nname = \"root\"\n");
        write(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        write(&root.join("crates/foo/src/lib.rs"), "pub fn x() {}\n");
        assert_eq!(resolve_module(root, "crates/foo/src/lib.rs"), "crates/foo");
    }

    #[test]
    fn module_resolver_skips_workspace_only_manifest_B1() {
        // §9.5 B1: a `[workspace]`-only root manifest does not capture members.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/foo\"]\n",
        );
        write(&root.join("crates/foo/src/lib.rs"), "x\n");
        assert_eq!(resolve_module(root, "crates/foo/src/lib.rs"), "crates");
    }

    #[test]
    fn module_resolver_falls_back_to_top_dir_B1() {
        // §9.5 B1: no manifest → top-level directory; repo-root files → `(root)`.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(&root.join("src/main.rs"), "fn main() {}\n");
        assert_eq!(resolve_module(root, "src/main.rs"), "src");
        write(&root.join("README.md"), "# hi\n");
        assert_eq!(resolve_module(root, "README.md"), "(root)");
    }

    #[test]
    fn module_resolver_is_deterministic_B1() {
        // §9.5 B1: same on-disk state → same module id.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(&root.join("Cargo.toml"), "[package]\nname = \"x\"\n");
        write(&root.join("a/b/c.rs"), "x\n");
        assert_eq!(
            resolve_module(root, "a/b/c.rs"),
            resolve_module(root, "a/b/c.rs")
        );
    }

    #[test]
    fn module_resolver_js_workspace_root_skipped_B1() {
        // §9.5 B1: a `package.json` with `private: true` + `workspaces` is skipped.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("package.json"),
            r#"{"private": true, "workspaces": ["packages/*"]}"#,
        );
        write(&root.join("packages/a/index.js"), "x\n");
        assert_eq!(resolve_module(root, "packages/a/index.js"), "packages");
    }

    #[test]
    fn module_resolver_go_mod_bare_presence_B1() {
        // §9.5 B1: `go.mod` qualifies by bare file presence.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(&root.join("go.mod"), "module example.com/m\n");
        write(&root.join("cmd/app/main.go"), "package main\n");
        assert_eq!(resolve_module(root, "cmd/app/main.go"), ".");
    }

    #[test]
    fn module_resolver_python_pyproject_bare_presence_B1() {
        // §9.5 B1: `pyproject.toml` qualifies by bare file presence.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(&root.join("pyproject.toml"), "[build-system]\n");
        write(&root.join("src/pkg/__init__.py"), "x\n");
        assert_eq!(resolve_module(root, "src/pkg/__init__.py"), ".");
    }

    #[test]
    fn module_resolver_same_dir_tiebreak_B1() {
        // §9.5 B1 scope: two manifest types in the same directory resolve by the
        // declared precedence. Here a workspace-only Cargo.toml (higher
        // precedence but non-package-bearing) is skipped, and the lower-
        // precedence package.json (with a name) resolves the directory.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = []\n");
        write(&root.join("package.json"), r#"{"name": "pkg"}"#);
        write(&root.join("src/a.js"), "x\n");
        assert_eq!(resolve_module(root, "src/a.js"), ".");
    }

    #[test]
    fn module_resolver_root_manifest_maps_to_dot_B1() {
        // §9.5 B1: a root `Cargo.toml` with `[package]` maps to `"."`, not `(root)`.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(&root.join("Cargo.toml"), "[package]\nname = \"root\"\n");
        write(&root.join("src/lib.rs"), "x\n");
        assert_eq!(resolve_module(root, "src/lib.rs"), ".");
    }

    #[test]
    fn module_resolver_deleted_file_uses_surviving_parent_manifest_B1() {
        // §9.5 B1: a deleted file's path resolves lexically against live worktree
        // manifests — no live file is required, only the parent manifest.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        assert_eq!(resolve_module(root, "crates/foo/src/lib.rs"), "crates/foo");
    }

    // --- §9.5 B1 aggregation ---

    #[test]
    fn module_report_aggregates_composition_B1() {
        // §9.5 B1: composition/total/file_count aggregate across a module's files,
        // and dominant_model is the max-surviving-chars model.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let files = vec![
            ModuleFile {
                path: "crates/foo/a.rs".to_string(),
                total_chars: 5,
                composition: HashMap::from([("opus".to_string(), 5)]),
                drifted: false,
                all_untracked: false,
            },
            ModuleFile {
                path: "crates/foo/b.rs".to_string(),
                total_chars: 6,
                composition: HashMap::from([("gpt".to_string(), 6)]),
                drifted: false,
                all_untracked: false,
            },
        ];
        write(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        let modules = build_module_reports(root, &files);
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.id, "crates/foo");
        assert_eq!(m.total_chars, 11);
        assert_eq!(m.file_count, 2);
        assert_eq!(m.composition.get("opus"), Some(&5));
        assert_eq!(m.composition.get("gpt"), Some(&6));
        assert_eq!(m.dominant_model.as_deref(), Some("gpt"));
        assert!(!m.drifted);
        assert!(!m.fully_untracked);
    }

    #[test]
    fn module_drifted_is_any_member_B1() {
        // §9.5 B1: drifted is OR over members; a clean module reports false.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        write(
            &root.join("crates/bar/Cargo.toml"),
            "[package]\nname = \"bar\"\n",
        );
        let files = vec![
            ModuleFile {
                path: "crates/foo/a.rs".to_string(),
                total_chars: 1,
                composition: HashMap::from([("opus".to_string(), 1)]),
                drifted: false,
                all_untracked: false,
            },
            ModuleFile {
                path: "crates/foo/b.rs".to_string(),
                total_chars: 1,
                composition: HashMap::from([("opus".to_string(), 1)]),
                drifted: true,
                all_untracked: false,
            },
            ModuleFile {
                path: "crates/bar/c.rs".to_string(),
                total_chars: 1,
                composition: HashMap::from([("opus".to_string(), 1)]),
                drifted: false,
                all_untracked: false,
            },
        ];
        let modules = build_module_reports(root, &files);
        let foo = modules.iter().find(|m| m.id == "crates/foo").unwrap();
        let bar = modules.iter().find(|m| m.id == "crates/bar").unwrap();
        assert!(foo.drifted, "one drifted member → module drifted");
        assert!(!bar.drifted, "no drifted members → module not drifted");
    }

    #[test]
    fn module_report_all_untracked_module_B1() {
        // §9.5 B1: an all-untracked module reports dominant "untracked",
        // drifted true, fully_untracked true.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        let files = vec![ModuleFile {
            path: "crates/foo/src/lib.rs".to_string(),
            total_chars: 11,
            composition: HashMap::from([("untracked".to_string(), 11)]),
            drifted: true,
            all_untracked: true,
        }];
        let modules = build_module_reports(root, &files);
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.dominant_model.as_deref(), Some("untracked"));
        assert!(m.drifted);
        assert!(m.fully_untracked);
    }

    #[test]
    fn module_report_counts_files_B1() {
        // §9.5 B1: file_count counts members with total_chars > 0 only.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        let files = vec![
            ModuleFile {
                path: "crates/foo/a.rs".to_string(),
                total_chars: 2,
                composition: HashMap::from([("opus".to_string(), 2)]),
                drifted: false,
                all_untracked: false,
            },
            ModuleFile {
                path: "crates/foo/empty.rs".to_string(),
                total_chars: 0,
                composition: HashMap::new(),
                drifted: false,
                all_untracked: false,
            },
        ];
        let modules = build_module_reports(root, &files);
        assert_eq!(modules[0].file_count, 1);
        assert_eq!(modules[0].total_chars, 2);
    }

    #[test]
    fn module_report_emits_zero_char_module_B1() {
        // §9.5 B1: a zero-char module is still emitted with file_count 0,
        // total_chars 0, drifted false, fully_untracked false, dominant omitted.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let files = vec![ModuleFile {
            path: "src/a.rs".to_string(),
            total_chars: 0,
            composition: HashMap::new(),
            drifted: false,
            all_untracked: false,
        }];
        let modules = build_module_reports(root, &files);
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.id, "src");
        assert_eq!(m.file_count, 0);
        assert_eq!(m.total_chars, 0);
        assert!(!m.drifted);
        assert!(!m.fully_untracked);
        assert_eq!(m.dominant_model, None);
        assert!(m.composition.is_empty());
    }
}
