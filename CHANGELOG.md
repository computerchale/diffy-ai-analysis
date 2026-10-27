# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `diffy doctor` checks the setup: git, the repo, `hooks.json`, whether the hook script is present, executable and current, the diffy binary, both hook logs, recorded events, and non-relative event paths. It exits 1 if any check fails.
- `diffy init --force` reinstalls over an existing setup, or replaces an unmergeable `.cursor/hooks.json` (the old file is saved as `hooks.json.bak`).
- Hook problem log: `diffy hook` records errors and warnings in `.git/diffy/hook.log`, and `diffy comp` / `diffy report` print a notice while it has entries.
- Module-level provenance: `diffy report --modules` with the `diffy-modules/1.0` JSON export.
- Dual MIT / Apache-2.0 license, CI, and contributor docs.

### Changed

- The package is now named `diffy-ai-analysis`; the installed command is still `diffy`.
- `diffy init` refuses to run when diffy is already set up in the workspace. Use `--force` to reinstall.
- README states that capture is Cursor-only and depends on Cursor's hook payload format, and that attribution is exact only for edits the hook captured.

### Fixed

- Cursor edits whose replacement text appears more than once in the file, isn't found, or is empty (pure deletions) are no longer guessed at. Before, diffy undid the first match, which could credit the model with characters it didn't write. Those changes are now recorded as `untracked`, with a warning in the hook log.
- A hand edit (or any unrecorded change) between two AI edits no longer shifts attribution. Replay assumed each event's `before` was the file's last recorded state; when it wasn't, the AI's diff landed on the wrong characters and the human's text was credited to the model. diffy now records the gap as `untracked` first.
- Files ignored by git (e.g. `.env` in `.gitignore`) are no longer captured. Previously the hook snapshotted any file an agent edited, because snapshots are written straight to git's object store, which `.gitignore` doesn't affect. Skips are logged as warnings with the path only.
- Half-life survival could exceed 100%, and `comp` over-credited models, on logs recorded before capture-side gap bridging (or with files that already had content when tracking started). Replay copied characters past the end of what it had tracked by wrapping back to the start of the file, so old spans were counted twice. Replay now bridges those gaps as `untracked` too, so survival stays between 0 and 1 and pre-existing code shows as `untracked` in `comp` and the report. Existing logs are fixed without re-recording.
- Formatter (transparent) events are now replayed instead of skipped. Their inserted characters keep the surrounding author, so the next edit no longer looks like a gap.
- Events timestamped in the future (clock skew) now count as age 0 in half-life instead of wrapping to a huge age.
- `.unwrap()` is no longer allowed outside tests (enforced by clippy).
- `comp`, `report`, `blame`, `why`, `half-life` and `checkpoint` now fail with "`<path>` is not a git repository" when run outside a repo. Before, most of them reported "no provenance data yet" or wrote an empty report, which looked like a capture problem.
- Recording edits no longer fails on machines without a git `user.name`/`user.email`. diffy's own commits (event-log refs, notes) now use the fixed identity `diffy <diffy@localhost>`, so they also never contain your email.
- The hook no longer needs `jq`. Without it, the old script exited before diffy ran, so no edits were recorded and nothing was logged. `diffy hook --cursor` now finds the repo from the payload's `file_path` itself.
- The hook no longer depends on the binary staying where `diffy init` found it. If that path is gone it falls back to `diffy` on `PATH`. If neither exists, it logs to `~/.local/state/diffy/hook.log` (or `$XDG_STATE_HOME/diffy/hook.log`), which `comp` and `report` also check. Run `diffy init --force` to get the new script.
- Cursor edits reached through a symlink (macOS `/var` vs `/private/var`, a symlinked project folder) are stored with a repo-relative path instead of an absolute one. A file outside the repo is now an error, and `diffy hook --payload` rejects empty, absolute, or `..` paths.

- `diffy init` no longer overwrites an invalid `.cursor/hooks.json`, which deleted any other hooks in it. It now stops with an error and leaves the file untouched.
- `diffy init` no longer reports success without installing the hook when `afterFileEdit` in `hooks.json` isn't a list.

### Removed

- The unused `hooks/diffy-cursor-hook.sh` script. `diffy init` generates the hook itself.

## [0.1.0]

First version: event capture from Cursor's `afterFileEdit` hook, character-level replay, and the `init`, `hook`, `comp`, `report`, `blame`, `why`, `half-life` and `checkpoint` commands.
