# diffy — Design Spec

**Character-level provenance for AI-written code.**

Like assaying a metal: with steel you read the chemical composition and the
conditions each part was prepared under. diffy does this for a codebase —
which model wrote which characters, under which prompt, in which session —
and how much of each model's work *survives* today.

- Composition → per-character attribution of the current tree, per model
- Preparation conditions → session metadata: agent, model, prompt, timestamps
- Alloy percentages → survival-weighted stats: "75% of surviving output is
  opus-4.6, 25% opus-4.8"

Status: v0.5 (M1 complete, M2 complete — see §16) · Stack: Rust (§13) ·
Prior art: git-ai, Entire CLI, origin-cli, agentdiff (§15)

---

## 1. Goals

1. Record **every individual change** an agent makes to a file, as it happens,
   matched to the **specific agent + exact model** that made it.
2. Compute **composition** of the current tree: what fraction of surviving
   characters came from each model (and from humans, and from unknown sources).
3. **Visualize** provenance: which parts of the codebase tend to be written by
   which model; survival over time.
4. Use **git as the only data store**: metadata lives in refs, notes, and
   blobs; the user's working branch is never touched.

## 2. Non-goals (v1)

- Detecting AI authorship heuristically. Agents *report*; we never guess.
  (Pre-instrumentation history is handled approximately by the blame-join
  engine, §7.)
- Long-running components in v1: no background watcher, no webserver. Both
  are fully designed but deferred — see §18 (post-MVP).
- A hosted/multi-user service. Never a goal; see §18.2.
- Multi-remote sync of provenance refs (push is supported, merging concurrent
  pushed histories is out of scope initially).
- Keystroke-level human attribution.

## 3. Data model

Four entities (see `src/model.rs`):

- **Prompt** — one user message that started an agent turn. Stores its full
  text. **Capture deferred**: neither
  Cursor's `afterFileEdit` payload nor the diffy-native hook payload carries
  prompt text today (Cursor exposes only `transcript_path`), so `prompt_id`
  and `prompt` remain unpopulated until transcript intake lands (§11).
- **Session** — one agent run: `tool` (cursor, claude-code, …), exact `model`
  slug, agent instance id, raw `conversation_id` (preserved verbatim so the
  report can link a span back to the originating chat), start/end, repo HEAD
  at start. The model slug is the unit of composition — the "chemical
  element" of the assay.
- **Event** — the atomic unit: one observed mutation of one file. Carries
  session ref, prompt ref, tool-call id (when the agent reports it), path,
  `before_blob`/`after_blob` (git blob hashes), a `transparent` flag
  (formatters/renames — provenance passes through, see §6.3), timestamp,
  and an **evidence level** recording how the edit was observed:
  `tool_call > command_named > edit_hook > write_journal > turn_window`
  (ladder adapted from origin-cli; feeds the reconciler, §4.3).
- **Checkpoint** — binds a set of events to a git commit. The bridge between
  "agent time" and "git time". Written in two phases (Entire pattern):
  provisional at commit time, finalized at turn end with full session
  context; commits carry a `Diffy-Checkpoint: <id>` trailer for robust
  linking.

Design decision: events store **blob hashes, not diffs**. Char-level edit
scripts are computed lazily from blob pairs (deterministic, recomputable
forever via `similar`/Myers). Blobs give free dedup; the log stays small.

## 4. Capture layer

Dual-mode, with a reconciler. All capture runs as **short-lived CLI
invocations** — no daemon, no webserver.

### 4.1 Hook mode (primary, MVP) ✅ implemented

Agent hooks invoke `diffy hook --payload …` on every edit tool call.
Two payload formats are supported:

1. **diffy native format** — `diffy hook --payload '{"model":…,"before":…,"after":…}'`
   Full before/after file content, model, session_id, path. Used by Claude
   Code PostToolUse and generic integrations.
2. **Cursor native format** — `diffy hook --cursor --payload @-`
   Parses Cursor's `afterFileEdit` payload directly in Rust: `{file_path,
   edits: [{old_string, new_string}], model, conversation_id, …}`.
   Before-content is reconstructed by reverse-applying the edits array
   against the file on disk (which is already in the after-state).
   **Reconstruction fails closed (I4).** Edits are undone last-to-first,
   and each edit's `new_string` must occur exactly once in the content at
   that step. If it occurs zero times (the file changed again before the
   hook read it) or more than once (e.g. `}` or `return Ok(());`), or is
   empty (a pure deletion — there is nothing to locate), the edit can't be
   placed without guessing. diffy then doesn't credit the model: it records
   the event under the model `untracked`, with `before` set to the file's
   last recorded after-state (empty if it has none) and `after` set to the
   disk. The changed chars land in the same `untracked` bucket as drift
   (§6.5), and the replay chain stays consistent, so later events aren't
   shifted onto the wrong chars. A warning goes to the hook log (below).

**Event paths are always relative to the repo root.** For the Cursor
format, `file_path` and the repo are both resolved (`canonicalize`,
following symlinks) before the repo prefix is stripped, so `/var/…` vs
`/private/var/…` or a symlinked project folder still yields `src/lib.rs`.
A file outside the repo is an error, never stored with an absolute path.
For the diffy native format, a `path` that is empty, absolute, or
contains `..` is rejected.

**Files git ignores are never captured.** Before anything is written, the
path is checked with `git check-ignore`; if git ignores it (e.g. `.env`
listed in `.gitignore`), the edit is skipped: no blobs, no event. Snapshots
are written straight to the object store, which `.gitignore` alone would
not stop, so without this check an ignored secret file would persist in
`refs/provenance/*` even after being deleted. The skip is logged as a
warning (path and reason only, never contents). Files that are tracked
despite matching an ignore pattern are captured, as git itself does.

Both formats also accept `@<file>` (read from file) or `@-` (read from
stdin). The `@-` form is required for Cursor hooks — see implementation
notes below.

**Cursor hook implementation** (`diffy init` installs automatically):

- Registers an `afterFileEdit` hook in `.cursor/hooks.json` pointing at
  `.cursor/hooks/diffy-hook.sh`.
- Cursor's payload provides: `model` (exact slug, e.g. `kimi-k3`,
  `gpt-5.3-codex`), `conversation_id` (session), `generation_id` (turn),
  `file_path` (absolute), `edits[]` (search-replace diffs), and
  `user_email`, `cursor_version`, `workspace_roots`, `transcript_path`.
  No need to read model slugs from `~/.cursor/ai-tracking/` — the hook
  payload is the source of truth.
- **The hook must be synchronous** (no backgrounding with `&`). Cursor
  reaps the hook's process group when the script exits, killing
  backgrounded child processes before they run. diffy hook takes ~150 ms,
  imperceptible to the agent loop.
- **Payload goes via stdin**: the script passes its stdin straight to
  `diffy hook --cursor --payload @-`. Passing via temp-file + background
  process fails because the script's cleanup deletes the file before diffy
  reads it.
- **The script needs only bash.** It doesn't parse the payload (no `jq`):
  `diffy hook --cursor` without `--repo` finds the repo itself from the
  payload's `file_path` (`git rev-parse --show-toplevel` in its directory),
  so it works for repos that are subfolders of the Cursor workspace. A file
  outside any git repo is skipped silently — there's nothing to record.
- **Binary lookup**: the script runs the diffy binary `init` was run with
  (baked in from `current_exe()`), and if that's gone, `diffy` on `PATH`.
- **Fail-open** (`|| true`, always `exit 0`): a diffy error must never
  break the editor.
- **…but never silently.** `diffy hook` appends every error, and every
  warning such as an unplaceable edit, to `<git-dir>/diffy/hook.log`
  (inside `.git/`, so it's never committed), one line each:
  `<RFC 3339 timestamp> <error|warning> <message>`. Messages carry paths
  and reasons, never file contents. `diffy comp` and `diffy report` print
  a one-line notice on stderr when the log is non-empty, with the count and
  the log's path; deleting the file clears it.
- **Fallback log** for problems that can't be tied to a repo:
  `${XDG_STATE_HOME:-~/.local/state}/diffy/hook.log`, same line format.
  The script writes it when no diffy binary can be found; `diffy hook`
  writes it when the payload is unreadable or has no `file_path`. `comp`
  and `report` print a notice for it too.
- `diffy init --workspace <path>` supports explicit workspace specification
  when the repo isn't at the Cursor project root. Without `--workspace`,
  walks up from the repo looking for an existing `.cursor/` directory
  (stops at `$HOME` to avoid confusing `~/.cursor/` with a workspace).
- `diffy init` merges into an existing `.cursor/hooks.json`, preserving
  every non-diffy hook. If the file isn't valid JSON or doesn't have the
  expected shape (an object whose `hooks` is an object and whose
  `afterFileEdit` is an array), init fails closed: it errors and leaves
  the file untouched. `--force` overrides this: the existing file is
  copied to `hooks.json.bak` and replaced with a fresh config.
- `diffy init` also refuses when diffy is already set up in the workspace
  (a diffy `afterFileEdit` entry in hooks.json, or
  `.cursor/hooks/diffy-hook.sh` exists), and changes nothing. `--force`
  reinstalls: the script is regenerated and diffy's entry is replaced,
  never duplicated.

**`diffy doctor [--repo R] [--workspace W]`** checks that capture is set up
and working, so a silent failure doesn't go unnoticed for weeks. Workspace
resolution is the same as `init`. Read-only: it never changes anything.
One line per check, `ok`, `warn` or `fail`, each with a fix:

| Check | fail | warn |
|-------|------|------|
| git | `git` can't run | — |
| repo | not a git work tree | — |
| hooks.json | missing, not JSON, or no diffy `afterFileEdit` entry | — |
| hook script | missing or not executable | not what this diffy would generate (outdated) |
| diffy binary | baked path gone and no `diffy` on `PATH` | baked path gone, hook falls back to `PATH` |
| hook log | — | `<git-dir>/diffy/hook.log` has entries |
| fallback log | — | the per-user fallback log has entries |
| events | refs unreadable | none recorded yet |
| event paths | — | events with paths that aren't repo-relative (recorded before that was enforced) |

When `git` or `repo` fails the remaining checks are skipped, and the binary
check is skipped when there's no script to read the baked path from (or an
outdated one without it). Exits 1 if
any check fails, 0 otherwise (warnings don't fail). `PATH` is the one
`doctor` runs with, which may differ from Cursor's.

Other hooks:
- Claude Code `PostToolUse` → uses diffy native format (planned).
- **Pre/post edit pairs**: capture before-state at edit start and
  after-state at edit end (git-ai's checkpoint-pair pattern) — the pair
  isolates exactly what the agent changed, even in a dirty worktree. This
  is the capture-side contract behind `before_blob`/`after_blob`.
- Bash edits (agent-run commands that write files): bridge via pre/post
  **stat-tuple snapshots** (mtime/size walk, ~1.5 s budget — git-ai's
  approach) attributing changed files to the active turn, until watcher
  mode (§18.1) provides real coverage.
- Git `post-commit` hook → `diffy checkpoint`: binds events since the last
  checkpoint to the new commit and writes the provenance note.
- Git `post-rewrite` hook → triggers note remapping (§10).

### 4.2 Watcher mode (post-MVP — fully specified in §18.1)

`diffy watch` — an optional background process that snapshots the worktree
to catch what hooks can't see: `sed -i` from bash, code generators, human
typing in the editor. Not built in MVP; hook mode (§4.1) is the sole
capture path until then. Full design: §18.1.

### 4.3 Reconciler

Within each capture window, watcher-observed diffs must equal the union of
hook-reported events. Discrepancies are classified **human** (typed between
agent turns) or **unknown**. Composition stats always report three buckets —
per-model, human, unknown — instead of silently misattributing.

## 5. Storage layer (git-native) ✅ implemented

Everything lives in the repo's object database; nothing touches the user's
branch history:

```
refs/provenance/events/<session-shard>   append-only JSONL event logs
refs/notes/provenance                    per-commit attribution notes
git blobs                                before/after file contents
```

- **Event log**: one commit per flush onto a per-session shard ref (avoids
  write contention between concurrent agents). Events are JSONL. Shard
  ref naming: first 10 characters of the session's ULID, e.g.
  `refs/provenance/events/5AEQQWM8M2`. Session ULIDs are derived
  deterministically from the agent's session/conversation ID (hash-based,
  not time-based) so the same session always maps to the same shard ref
  across invocations. Each shard's tree contains `events.jsonl` (one JSON
  line per event) and `sessions.jsonl` (session metadata). Ref updates use
  CAS via `git update-ref <ref> <new> <old>` (Entire's proven pattern —
  library-level CAS does not interoperate with git's own locking).
  Cross-shard merge: read all shard refs, parse all events, sort by ULID
  timestamp for chronological ordering.
- **Notes**: at checkpoint time, write a per-commit summary to
  `refs/notes/provenance` using git-ai's **`authorship/3.0.0` schema
  verbatim** (attestation section: `s_<id>::t_<id>` / `h_<id>` keys →
  line ranges, then `---`, then JSON metadata) so git-ai tooling reads our
  repos and vice versa. Char-level precision stays in the event log;
  notes are the line-level export. Prompts are **not** stored in notes —
  ids/pointers only; full text lives in the local event store (git-ai's
  privacy precedent, §11).
- **Blobs**: before/after contents stored as ordinary git objects,
  referenced by hash, kept alive by the refs.
- **Identity**: diffy's own commits (event-log flushes, notes) are
  authored and committed as `diffy <diffy@localhost>`, never the user's
  git identity. Capture works on machines with no `user.name`/`user.email`
  (fresh installs, CI), and shared provenance refs don't carry the user's
  email.
- **Compaction**: periodically fold events older than N days into a baseline
  snapshot + provenance vector, keeping recent history granular. Bounds
  growth on busy repos.

## 6. Replay engine (the differentiator)

Existing tools are blame-join systems: approximate, line-level, commit-
granular. With a complete event log we can do strictly better.

### 6.1 Piece table with provenance tags ✅ implemented

Per file, maintain a piece table (`ProvenanceDoc` in `src/replay.rs`) where
every span of characters carries an origin tag (event → session → model).
Replaying the event log yields the current tree's composition **exactly** —
character by character — surviving partial edits and moves. Spans are tracked
by **position + content, never content alone** — content-hash tracking fails
on identical lines (`}`, `return nil`), a trap Entire documented the hard way.

The replay engine uses `similar::TextDiff::from_chars()` (Myers algorithm)
to compute char-level diffs between consecutive `before_blob`/`after_blob`
pairs. Equal chars preserve their existing provenance spans; insertions are
attributed to the new event. Span splitting handles insertions in the
middle of existing spans correctly.

### 6.2 Content-based, not line-number-based

Because provenance is attached to content spans rather than line ranges, the
replay engine is naturally robust across rebases/squashes that invalidate
line numbers (§10).

### 6.3 Transparency

Events flagged `transparent` (formatter runs, mass renames, pure refactors)
pass provenance through to the original author instead of reattributing.
Without this, one prettier commit "reauthors" the whole repo. Detection is
heuristic (whitespace-only diff, rename detection) plus manual tagging.

### 6.4 Cross-file moves

When an agent copies block A from file X to file Y, token-level fingerprinting
(winnowing-style) carries provenance across files.

### 6.5 The byte-identity invariant & drift reconciliation ✅ implemented

Replaying a file's events must reproduce the worktree **byte-for-byte**
(origin-cli's validation pattern). When the replay output differs from the
file on disk (drift), the report engine reconciles rather than discarding:

1. Run a char-level diff between the replay's output and the disk content.
2. Chars that are equal in both preserve their replay provenance (model
   attribution carries through).
3. Chars present on disk but not in the replay are marked **"untracked"**
   (rendered in muted gray in the report).
4. The file is flagged `drifted: true` in the report JSON.

This is strictly better than discarding the entire file's attribution: known
provenance is preserved for surviving chars, and only the gap is marked
honestly. Invariant I1 (§10) is maintained — we never guess; untracked chars
are never attributed to a model.

Common drift causes: edits made while the hook was broken or not yet
installed, manual edits by the user, edits from agents without hook
integration. The next captured edit to a drifted file re-syncs the
`after_blob`, shrinking the untracked gap.

**Gaps between events are bridged at capture time.** Replay applies each
event's before→after diff to the replayed state, which is only correct if
`before` equals the file's last recorded `after`. When it doesn't (a hand
edit, another tool, or an edit the hook missed happened in between),
capture first records a bridge event under the model `untracked`
(last recorded `after` → this event's `before`, in a separate
`untracked:<session>` session), then the real event. The bridge sorts
strictly before the event. So the gap's chars are untracked and the
model is credited only with its own diff — without the bridge, replay
would shift the model's diff onto the wrong chars. A file's first event
has no prior state and is never bridged. Bridging is normal and isn't
logged.

**…and again at replay time.** Event logs recorded before capture-side
bridging existed have gaps, and a file's first event can carry content
that predates tracking (a non-empty `before`). So replay checks each event
the same way: if its `before` differs from the file's last replayed
`after` (`""` for a file's first event), it first applies that difference
under the reserved origin `UNTRACKED_ORIGIN` (the nil ULID, never a real
event id), then the event. Every consumer maps that origin to
`untracked`. The resulting guarantees:
- After each event, the replayed document has exactly the length of that
  event's `after`.
- No event is credited with more surviving characters than it inserted.
  This keeps survival (§8) within 0–1, and stops composition from
  counting characters twice.
- Copying provenance from the old document never reads past its end. A
  gap can't wrap around and duplicate earlier spans.

For a gap-free log, replay-time bridging changes nothing.

## 7. Blame-join engine (compatibility layer)

For history predating instrumentation: join `git blame --porcelain -M -C -w`
against per-commit provenance notes. Approximate and line-level — clearly
labeled as such in output. Also serves as the export path: our notes stay
readable by git-ai-style tooling and vice versa (char-level replay degrades
to line ranges on export — the same internal-vs-persisted split git-ai
uses).

**Drift recovery** (origin-cli's pattern): when a blamed line's commit has
no note or low confidence, walk `git log -L` (bounded, ~6 commits) toward
the *introducing* commit before giving up.

## 8. Analytics ✅ implemented

All composition stats are weighted by **characters, not lines** (line counts
overvalue short lines).

- **Composition** ✅ — per file/dir/repo: "75% opus-4.6, 25% opus-4.8", plus
  human/unknown/untracked buckets. `diffy comp [--repo R] [path] [--json]`.
- **Code half-life per model** ✅ — survival curves: fraction of a model's
  emitted characters still alive after N days. An empirical model-quality
  metric that falls out of provenance × survival for free.
  `diffy half-life [--model M]`.
  - Each event's *emitted* count is the bytes its before→after diff
    inserted. Its *surviving* count is how many of those bytes the replay
    (§6.5) still attributes to it. Both use the same unit and the same diff.
  - Events are bucketed by whole days since they were recorded, and
    survival at an age is Σsurviving / Σemitted over that age's events. It
    is always within [0, 1]; an age with nothing emitted reports 0.
  - The half-life is the first age whose survival is below 0.5; `None`
    (">N days") if survival never drops below it.
  - Gap and pre-existing characters belong to `untracked`, never to a
    model, so they don't affect any model's curve.
- **Model × directory affinity** ✅ — which parts of the tree each model tends
  to own. Rendered as the report heatmap (§9.4).

## 9. Visualization ✅ core implemented

`diffy report` → one **self-contained HTML file** (all data + JS + CSS
embedded via `include_str!`). No webserver. All data comes from the git
repo — `diffy report --repo /path/to/repo` reads the provenance refs and
renders everything. The report is inspired by Python coverage.py HTML
output but operates at character granularity.

The report reads files from **disk** (source of truth), not just from
provenance blobs. Files with drift (§6.5) show untracked chars in muted
gray. Files in the repo with no provenance events at all appear as fully
untracked. JSON in the HTML is escaped to prevent `</script>` injection.
Non-source files (`.html`, images, binaries, lockfiles) are excluded from
the file scan.

### 9.1 Layout: file explorer + character grid ✅ implemented

Two-pane layout:

- **Left pane — file explorer.** Directory tree mirroring the repo
  structure. Each file/directory shows a composition summary bar (colored
  segments proportional to model %). Click a file to open it in the main
  pane. Click a directory to see its aggregate composition. Styled like
  coverage.py's file listing.
- **Right pane — character grid.** The selected file rendered as a grid
  where **every character (including whitespace) is one cell**. Each row is
  one line of the file; each cell in the row is one character position.
  Cells are colored by the model that authored that character (or
  human/unknown). Hovering a cell shows a tooltip: model, session id
  (the raw agent `conversation_id`, e.g. Cursor's `conversation_id`,
  preserved verbatim on the `Session`; falls back to the derived ULID for
  sessions recorded before this field existed), timestamp (prompt excerpt
  deferred — see §3, prompt capture is not yet built). Monospace font;
  line numbers in a gutter.

### 9.2 Playback: time-travel through changes ✅ implemented

A **play/pause button** and a **timeline slider** at the top of the grid
pane. Pressing play replays the event log chronologically:

- Each event's edit is applied to the grid: inserted characters appear,
  deleted characters disappear, and the affected cells animate to the
  color of the model that made the change.
- Default speed: **1 event per second**. A slider adjusts from 0.1×
  (slow-motion, 10 s per event) to 20× (50 ms per event). A step
  button advances one event at a time for inspection.
- The timeline slider doubles as a scrubber — drag to any point in
  history to see the grid's state at that moment.
- The file explorer's composition bars update in sync as playback
  progresses, so you can watch the repo's composition shift over time.

### 9.3 Data source: git + disk ✅ implemented

The report reads from the git repo's provenance refs for attribution history
and from files on disk for current content (reconciling drift per §6.5). It
(`refs/provenance/events/*`, `refs/notes/provenance`, blobs) and the
worktree. No external database, no local cache files. The CLI serializes
everything into a single embedded JSON blob inside the HTML:

```
diffy report                          # current repo, writes diffy-report.html
diffy report --repo /path/to/repo     # different repo
diffy report --out custom-name.html   # custom output path
diffy report --json                   # emit raw JSON to stdout (for piping)
diffy report --no-session             # omit per-char session/conversation ids (§11)
```

The `--no-session` flag is the privacy switch for session ids: it drops the
per-character `char_sessions` array from the report JSON entirely (not just
hidden in the UI), so conversation identifiers never leave the machine.
Session data captured before the `conversation_id` field existed falls back
to the derived session ULID; either way `--no-session` removes it from
output.

### 9.4 Additional views ✅ implemented

Beyond the character grid, the report includes tabs/views for:

- **Repo treemap** ✅ — area = chars, color = dominant model per file.
- **Stratigraphy chart** ✅ — stacked area of surviving chars per model
  over time (geological layers).
- **Model × directory heatmap** ✅ — which models own which parts of the
  tree.
- **Half-life curves** ✅ — per-model survival over time.
- **Composition summary** ✅ — repo-wide percentages in the header bar,
  per-file percentages in the explorer and grid header.

### 9.5 Module report (`report --modules`) ✅ implemented

Tests for this section are suffixed `_B1` (module resolution and
aggregation) or `_C1` (the JSON export).

**Module resolution (B1).** Each file maps to one module: the directory of
the nearest ancestor manifest that declares a package.
- Rust: `Cargo.toml` containing a `[package]` line. A `[workspace]`-only
  manifest is skipped.
- JS/TS: `package.json` with a `name`. A workspace root (`private: true`
  plus `workspaces`) is skipped.
- Go: `go.mod`. Python: `pyproject.toml` or `setup.py`. All of these count
  by presence alone.
- On a tie in one directory: `Cargo.toml` > `package.json` > `go.mod` >
  `pyproject.toml` > `setup.py`.
- A root manifest maps to `"."`. With no manifest, the file's top-level
  directory is used, or `"(root)"` for files at the repo root.
- Files deleted from disk resolve by their recorded path against today's
  manifests.

The mapping is deterministic for a given on-disk state. It reads live
manifests, not committed ones. Its input is every file counted in the
repo total (replayed files plus untracked files on disk), so
`sum(module total_chars) == repo_total_chars`.

**Per-module fields.**
- `composition`: model → surviving characters, plus `untracked` and
  `unknown`.
- `total_chars`.
- `file_count`: files with more than zero characters.
- `dominant_model`: most surviving characters, ties broken
  alphabetically; omitted for a module with zero characters.
- `drifted`: true if any member file is drifted.
- `fully_untracked`: true if no member file has any provenance.

**Export (C1).** `diffy report --modules` needs exactly one of `--json`
(stdout) or `--out <file>`; both or neither is a usage error. The schema is
versioned:

```json
{
  "schema_version": "diffy-modules/1.0",
  "generated_at": "2026-10-26T19:30:00Z",
  "repo_total_chars": 4000,
  "modules": [
    { "id": "crates/foo", "file_count": 12, "total_chars": 4000,
      "composition": { "deepseek-v4-pro": 3000, "untracked": 1000 },
      "dominant_model": "deepseek-v4-pro", "drifted": true,
      "fully_untracked": false }
  ]
}
```

- The output carries no session or conversation ids, so `--no-session`
  has no effect here.
- Ordering is deterministic: modules are sorted by `id`, and composition
  keys are sorted.
- Changes to the schema are additive only; new fields never break
  existing readers.
- A repo with no events and no source files emits `modules: []`.

## 10. History rewrite survival

The hardest storage problem: rebase/squash/cherry-pick invalidate line
numbers and commit hashes.

**Invariants (adopted verbatim from git-ai, battle-tested in production):**

- **I1 — evidence only.** No fuzzy matching to "recover" attribution;
  better a gap than a guess.
- **I2 — conservation via git's own diffs.** Attribution moves are derived
  from `git diff-tree` output, never from re-reading file contents.
- **I3 — immutable inputs.** Rewrite handling reads committed objects only,
  never the live worktree.
- **I4 — fail-closed.** Gaps in attribution are acceptable; misattribution
  is not.

**Mechanism.**

1. The replay engine mostly doesn't care — provenance is content-based.
2. For notes: git-ai's **note-shift algorithm**, reused wholesale:
   batch-read source/destination notes → one
   `git diff-tree --stdin -p -U0 -M -r` over all tree pairs → shift ranges
   through preserved segments (regions outside hunks, cumulative offsets) →
   **drop any range overlapping a hunk** → renames carry attribution to new
   paths. Rebase old→new mapping via `git range-diff`; cherry-pick pairing
   via `git patch-id --stable` then positional fill; reset unwinds notes
   back into the event log as initial attributions.
3. Worst case, attribution degrades to "unknown" honestly (I4).

**Hooks vs. daemon (decided for v1, revisit at M3).** git-ai eventually
sunset git hooks for a trace2-event-ingestion daemon with reflog cursors,
citing reliability. diffy v1 stays with hooks (`post-rewrite` + friends):
simpler, short-lived, no process management. At M3 — alongside the watcher
(§18.1), when we already accept a long-running process — we evaluate
migrating rewrite detection to trace2 ingestion.

## 11. Privacy & governance

- **Prompts never go into notes** (git-ai's precedent, their Errata E-002):
  notes carry session/prompt ids only; full prompt text lives in the local
  event store. Pushing notes therefore never leaks prompt content.
- **Session ids in reports are opt-out.** `diffy report` embeds the raw
  agent `conversation_id` per character (shown in the grid tooltip) so spans
  trace back to their originating chat. These identifiers can correlate to
  an account, so `--no-session` strips them from the report JSON entirely —
  recommended before sharing a report outside the machine.
- Provenance refs are namespaced (`refs/provenance/*`) so they can be pushed,
  kept local, or excluded per remote.
- Optional signing of attribution records (agentdiff does ed25519) is a v2
  consideration.

## 12. CLI surface

```
# ✅ implemented:
diffy init [--repo R] [--workspace W] [--force]  install Cursor hooks
diffy doctor [--repo R] [--workspace W]  check that capture is set up and working
diffy hook --payload … [--cursor] [--repo R]  process one edit event
diffy comp [--repo R] [path] [--json]     composition of file/dir/repo
diffy report [--repo R] [--out F] [--json] [--no-session]  static HTML report / raw JSON
diffy checkpoint [--repo R]               bind events to HEAD via a provenance note
diffy blame <file> [--repo R]             annotated source, per-span model tags
diffy why <file:line> [--repo R]          the model + session behind a span (prompt capture deferred, §3/§11)
diffy half-life [--model M] [--repo R]    survival curves

# post-MVP (fully specified in §18, not scheduled for v1):
diffy watch [--install|--stop]   background worktree watcher (§18.1)
diffy serve [--port N] [--open]  localhost live-report server (§18.2)
```

Payload argument (`--payload`) supports three forms: literal JSON string,
`@<file>` (read from file), `@-` (read from stdin). The `--cursor` flag
switches to Cursor's native `afterFileEdit` schema.

Everything emits `--json` for piping into viz.

Repo requirement: no command except `doctor` needs `init`. `comp`,
`report`, `blame`, `why`, `half-life` and `checkpoint` read
`refs/provenance/*` directly, so they work on any repo, including one where
`init` never ran. But each one first checks that `--repo` (default `.`) is
inside a git work tree. If it isn't, the command exits 1 with
`<path> is not a git repository — run inside one or pass --repo <path>`,
and writes nothing. Before this check, a wrong `--repo` looked exactly
like a repo with no recorded edits: "no provenance data yet", or an
empty report.

## 13. Stack

Rust, single static binary (hooks need zero-runtime installs). Key choices:

- **Shell out to the real `git` binary** for all plumbing (notes, refs,
  blame, cat-file). git2/libgit2 has gaps in exactly the areas we need most;
  the CLI never does. Thin, testable exec wrapper in `src/git.rs`.
- `clap` (CLI) · `serde`/`serde_json` (JSONL) · `similar` (char-level Myers
  diffs) · hand-rolled piece table (replay) · `notify` (watcher, later) ·
  `jiff` (time) · `ulid` (time-sortable ids) · `anyhow`/`thiserror`

## 14. Engineering process: strict TDD

All development is test-driven. No exceptions, no "tests after".

**The loop.**

1. **Spec first.** Every behavior traces to a section of this document. If
   the spec is silent, amend the spec *before* writing the test.
2. **Red.** Write a failing test that encodes the spec'd behavior. Test
   names cite the spec: `replay_preserves_spans_through_partial_edit_6_1`.
3. **Green.** Write the minimum implementation that passes.
4. **Refactor.** Tests stay green throughout.
5. **Document.** Code comments land with the implementation, never after.

**Test taxonomy.**

- **Unit** — piece table replay, char-diff application, transparency
  detection, note-shift segment math. Fast, no filesystem.
- **Integration** — real git repos in tempdirs: simulate agent sessions
  (hook payloads + file writes), commit, rebase, squash, cherry-pick;
  assert composition, note survival, and the byte-identity invariant
  (§6.5) end to end. Never mock git — the point is proving our plumbing
  against the real thing.
- **Golden files** — blame-join output and notes export against
  hand-verified fixtures, including git-ai `authorship/3.0.0`
  compatibility samples.

**No panics outside tests.** `.unwrap()` is denied in non-test code
(`clippy::unwrap_used`, enforced in CI). Handle the error with `?` and
context; where a failure is genuinely impossible, use `.expect("…")`
stating why. A panic in the hook path is lost data that nobody sees.

**Code comments.**

- Every module has `//!` docs stating its job and its spec section.
- Every public type/function has `///` docs; non-obvious logic explains
  *why*, citing spec sections (`§10 I4`).
- Invariants are stated where they are enforced: I1–I4 at the rewrite
  code, byte-identity at the replay engine's public entry.

**Definition of done (per milestone).**

- All tests green; every new behavior's failing test predates its
  implementation in the commit history.
- Spec sections for the milestone's behaviors marked implemented.
- `cargo doc` renders without warnings.

## 15. Prior art

- **git-ai** — Authorship Log standard in `refs/notes/ai`; moves/merges
  attribution across rebase/squash/stash. Our notes format and rewrite
  survival borrow heavily.
- **Entire CLI** — session checkpoints on a side branch or per-checkpoint
  refs; shadow-repo snapshots at prompt boundaries; human/agent interleaving
  resolution. Our checkpoint + reconciler design borrows heavily.
- **origin-cli** — `blame`/`why` UX, prompts stored as notes and refs.
- **agentdiff** — ed25519-signed, commit-scoped attribution records.

## 16. Milestones

Every milestone follows §14: failing tests written from the spec sections
in scope first, then implementation, then docs.

- **M0** ✅ — scaffold + this spec + prior-art analysis
- **M1 (MVP)** ✅ — hook-mode capture (Cursor), event log storage, replay
  engine, `comp` + `blame`, HTML report
  - ✅ `git.rs` — plumbing: blobs, refs, mktree, commit-tree, CAS, notes
  - ✅ `store.rs` — shard refs, JSONL append, read_all_events merge
  - ✅ `capture.rs` — HookPayload + CursorPayload parsing, blob storage
  - ✅ `replay.rs` — piece table with provenance spans, composition
  - ✅ `report.rs` — self-contained HTML: file explorer, char grid, playback,
    drift reconciliation, untracked detection
  - ✅ `main.rs` — CLI: `init`, `hook` (with `--cursor`), `comp`, `report`
  - ✅ Cursor hook: auto-installed by `init`, tested live with real models
  - ✅ 69 tests (7 git, 10 store, 10 capture, 10 replay, 5 CLI, 11 report,
    5 blame, 11 stats)
  - ✅ `blame` — annotated source view (§7)
  - ✅ `checkpoint` — post-commit binding via provenance note (§5)
  - ✅ `why` — per-line model + session attribution (§12)
  - ✅ `half-life` — survival curve CLI (§8)
- **M2** ✅ — checkpoints + notes, full report, module alignment
  - ✅ checkpoints + `authorship/3.0.0` notes (§5)
  - ✅ full report: treemap, stratigraphy, heatmap, half-life curves (§9.4)
  - ✅ session-id tooltip + `--no-session` privacy flag (§9.3)
  - ✅ **B1** — module-boundary detection + per-module aggregation
    (`modules.rs`; manifest-based resolver for Cargo/JS/Go/Python, workspace-
    root skip, `BTreeMap` aggregation — see §9.5)
  - ✅ **C1** — stable module-provenance export: `diffy report --modules --json`
    emits a versioned `diffy-modules/1.0` JSON schema with per-module
    composition, drift, `fully_untracked`, `dominant_model`
    (see §9.5)
  - Deferred: blame-join backfill for pre-instrumentation history (accepted
    as a limitation — noteless lines stay `None` per I1)
- **M3** — watcher mode (`diffy watch`, §18.1) + reconciler, rewrite
  survival, compaction; evaluate trace2 ingestion vs hooks (§10)
- **M4** — `diffy serve` (§18.2), Cursor canvas integration, cross-file
  move tracking, signing

## 17. Open questions

- ~~Notes schema~~ — **resolved**: adopt git-ai's `authorship/3.0.0`
  verbatim for the notes layer (§5); char-level precision stays in the
  event log.
- ~~Hooks vs daemon for rewrite detection~~ — **resolved for v1**: hooks;
  trace2 ingestion evaluated at M3 (§10).
- ~~Cursor model detection~~ — **resolved**: Cursor's `afterFileEdit`
  payload provides `model`, `conversation_id`, and other metadata
  directly. No need to read from SQLite databases or transcripts.
- ~~Cursor hook backgrounding~~ — **resolved**: hooks must be synchronous.
  Cursor reaps the hook's process group on script exit, killing
  backgrounded processes. ~150 ms is imperceptible.
- ~~Cursor payload delivery~~ — **resolved**: payload via stdin (`@-`),
  not temp files. Avoids race between script cleanup and diffy reading.
- Transparency heuristics: how aggressive before precision suffers?
- Event log sharding granularity under many parallel agents.
- Should `diffy blame` merge replay + blame-join results at the
  instrumentation boundary, and how do we render that seam?
- Shadow tree snapshots (Entire-style) as compaction baselines vs. pure
  event-log compaction (§5) — decide at M3.

---

## 18. Post-MVP components (fully specified, not scheduled for v1)

These two components are the only long-running processes diffy will ever
have. Both are optional add-ons around the same append-only store; neither
changes the data model, the storage layout, or the capture contract. v1
works completely without them.

### 18.1 `diffy watch` — background worktree watcher

**Purpose.** Capture edits that hooks cannot see: agent edits made through
bash (`sed -i`, code generators, build scripts that write source), human
typing in the editor, and agents with no hook integration. Feeds the
reconciler (§4.3), which classifies watcher-observed diffs against
hook-reported events.

**Process model.**

- One watcher process per repo worktree. Started manually (`diffy watch`,
  foreground, for debugging) or installed as a user service
  (`diffy watch --install` → launchd agent on macOS, systemd user unit on
  Linux). `diffy watch --stop` stops it; `diffy status` reports liveness.
- Lifecycle state lives in `.git/diffy/`: pidfile (`watch.pid`), log
  (`watch.log`), and the last-snapshot manifest (`watch.state`).
- **No sockets, no HTTP, no IPC.** The watcher communicates exclusively by
  writing events to the store (§5) through the same code path hooks use.
  It is a "daemon" only in the sense of being long-lived.

**Snapshot strategy.**

- fs notifications via the `notify` crate, honoring `.gitignore` plus a
  built-in ignore list (`.git/`, `node_modules/`, `target/`, lockfiles).
- Debounce: a change burst settles after a 300 ms quiet period, then the
  affected files are hashed and compared against the last-snapshot
  manifest. Only content-changed files produce events.
- Turn-boundary alignment: hook events (§4.1) carry timestamps and session
  context; the watcher buckets its snapshots into the same time windows so
  the reconciler can subtract hook-reported diffs from watcher-observed
  diffs per window.
- Watcher-originated events enter the log with `origin = unknown` (or
  `human` when no agent turn was active in the window) and are upgraded by
  the reconciler, never by guessing.

**Resource bounds and failure modes.**

- Max file size for snapshotting (default 1 MiB; larger files recorded as
  "changed, content untracked").
- Backpressure: if events arrive faster than the store flush interval,
  snapshots coalesce — provenance granularity degrades, never blocks the
  user's filesystem.
- Crash safety: snapshots are content hashes, so a restart simply
  re-derives state from the worktree; at most one debounce window of
  granularity is lost, never data.

### 18.2 `diffy serve` — localhost live-report server

**Purpose.** A live-updating sibling of `diffy report` (§9): watch the
treemap / stratigraphy / heatmaps update while agents work, instead of
regenerating static HTML.

**Design.**

- Serves the **same stats JSON** as `diffy report` over HTTP, plus the same
  embedded D3 assets — one visualization codebase, two delivery modes.
- Implementation: `axum`, binding `127.0.0.1` only, random port by default
  (`--port` to pin), `--open` to launch a browser. No auth — the threat
  model is local-only; binding to localhost is the security boundary.
- Live updates via Server-Sent Events: the server watches the event-log
  refs for new commits (cheap ref-tip polling on a 1 s interval) and
  pushes a recompute-and-reload signal to connected pages.
- **Read-only.** No mutation endpoints exist. The store remains append-only
  via CLI and hooks; `serve` is a pure projection.

**Why post-MVP.** Static HTML reports plus the Cursor canvas cover the
analysis use cases; `serve` adds process-lifecycle and port-management
surface for marginal benefit until live multi-session monitoring matters.

**Never a goal:** a hosted or multi-user service. diffy's store is the
user's git repo; sharing provenance means pushing refs, not running a
server.
