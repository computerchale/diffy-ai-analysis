//! Core data model. See SPEC.md §3 for the full schema.
//!
//! Four entities: Prompt, Session, Event, Checkpoint. Events are the atomic
//! unit — one observed file mutation. Blobs are content-addressed by their
//! git blob hash; char-level edit scripts are computed lazily via `similar`.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Which agent tool produced the work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    Cursor,
    ClaudeCode,
    Codex,
    Gemini,
    Other(String),
}

/// How the edit was observed — the evidence ladder (§3, adapted from
/// origin-cli). Higher variants are more trustworthy; feeds the
/// reconciler (§4.3) and the human/unknown composition buckets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// Agent's edit tool call reported via hook — highest confidence.
    ToolCall,
    /// A named command (e.g. sed) observed by the agent hook.
    CommandNamed,
    /// Hook reported an edit but without tool-call-level detail.
    EditHook,
    /// Inferred from journal/log files.
    WriteJournal,
    /// Inferred from turn-window timing only — lowest confidence.
    TurnWindow,
}

/// One user prompt that kicked off an agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)] // schema entity (§3); captured once prompt intake lands
pub struct Prompt {
    pub id: Ulid,
    pub session_id: Ulid,
    /// Full text, or a redacted excerpt/hash when privacy mode is on.
    pub text: String,
    pub redacted: bool,
    pub ts: jiff::Timestamp,
}

/// One agent run/conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Ulid,
    pub tool: Tool,
    /// Exact model slug, e.g. "claude-opus-4-6". This is the unit of
    /// composition stats — the "chemical element" of the assay.
    pub model: String,
    pub agent_instance: Option<String>,
    /// Raw agent conversation/session id (Cursor's `conversation_id`),
    /// preserved verbatim so a span can link back to the originating chat.
    /// `None` for sessions written before this field existed.
    #[serde(default)]
    pub conversation_id: Option<String>,
    pub started: jiff::Timestamp,
    pub ended: Option<jiff::Timestamp>,
    /// Repo HEAD when the session started.
    pub base_commit: Option<String>,
}

/// The atomic unit: one observed mutation of one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Ulid,
    pub session_id: Ulid,
    /// Which prompt's turn produced this edit, when known.
    pub prompt_id: Option<Ulid>,
    /// Agent-reported tool call id, when the hook provides one.
    pub tool_call_id: Option<String>,
    pub path: String,
    /// Git blob hashes of content before/after. Edit scripts are derived
    /// lazily; blobs give free dedup and permanent recomputable diffs.
    pub before_blob: String,
    pub after_blob: String,
    /// Formatter/rename events are transparent: provenance passes through
    /// to the original author instead of reattributing (§6.3).
    pub transparent: bool,
    /// How the edit was observed — the evidence ladder (§3).
    pub evidence: Evidence,
    pub ts: jiff::Timestamp,
}

/// Binds a range of events to a git commit — the bridge between
/// "agent time" and "git time". Written from the post-commit hook.
/// Two-phase: provisional at commit time, finalized at turn end (§3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)] // schema entity (§3); written once checkpoint bookkeeping lands
pub struct Checkpoint {
    pub commit: String,
    pub events: Vec<Ulid>,
    pub ts: jiff::Timestamp,
}
