//! Replay engine — the exact, char-level attribution core. See SPEC.md §6.
//!
//! Maintains a piece table per file where every span carries a provenance
//! tag (event → session → model). Replaying the event log yields the
//! current tree's composition exactly, surviving moves and partial edits.
//!
//! §6.1: Spans are tracked by position + content, never content alone
//! (content-hash tracking fails on identical lines like `}` or `return nil`).
//!
//! §6.2: Because provenance is content-based rather than line-number-based,
//! the replay engine is naturally robust across rebases/squashes.
//!
//! §6.5: The byte-identity invariant — replaying events must reproduce the
//! file byte-for-byte; mismatch → drift flag, degrade to blame-join or
//! "unknown". Invariant I1: never paper over drift with fuzzy matching.

use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;
use ulid::Ulid;

/// A run of characters sharing one provenance source.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// Byte length of the run in the current document.
    pub len: usize,
    /// The event that introduced these characters.
    pub origin: Ulid,
}

/// Piece table over one file: the span list describing the current
/// document, where each span tags a byte range with its originating event.
#[derive(Debug, Clone)]
pub struct ProvenanceDoc {
    pub spans: Vec<Span>,
}

impl Default for ProvenanceDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl ProvenanceDoc {
    pub fn new() -> Self {
        ProvenanceDoc { spans: Vec::new() }
    }

    /// Total byte length of the document. Used by tests to assert the
    /// §6.1 byte-identity invariant after each replay step.
    #[allow(dead_code)]
    pub fn total_len(&self) -> usize {
        self.spans.iter().map(|s| s.len).sum()
    }

    /// Apply one event's char-level edit script (computed from before/after
    /// content via `similar`) to the piece table. §6.1: edits are positional.
    pub fn apply(&mut self, event_id: Ulid, before: &str, after: &str) {
        self.apply_edit(Some(event_id), before, after);
    }

    /// Apply a transparent edit (§6.3): inserted characters take the origin
    /// of the text they join instead of a new one.
    pub fn apply_transparent(&mut self, before: &str, after: &str) {
        self.apply_edit(None, before, after);
    }

    /// Origin of the byte at `pos` in the current document, if any.
    fn origin_at(&self, pos: usize) -> Option<Ulid> {
        let mut cumulative = 0;
        self.spans.iter().find_map(|s| {
            cumulative += s.len;
            (cumulative > pos).then_some(s.origin)
        })
    }

    fn apply_edit(&mut self, event_id: Option<Ulid>, before: &str, after: &str) {
        let diff = TextDiff::from_chars(before, after);
        let mut new_spans: Vec<Span> = Vec::new();
        // Position in the old document's span list
        let mut old_pos: usize = 0;

        for change in diff.iter_all_changes() {
            match change.tag() {
                ChangeTag::Equal => {
                    // These characters survive — copy spans from old doc
                    let len = change.value().len();
                    self.copy_spans_from_old(&mut new_spans, &mut old_pos, len);
                }
                ChangeTag::Delete => {
                    // These characters are removed — skip them in old spans
                    let len = change.value().len();
                    old_pos += len;
                }
                ChangeTag::Insert => {
                    // New characters — attribute to this event
                    let len = change.value().len();
                    if len > 0 {
                        let event_id = event_id
                            .or_else(|| new_spans.last().map(|s| s.origin))
                            .or_else(|| self.origin_at(old_pos))
                            .unwrap_or(UNTRACKED_ORIGIN);
                        // Merge with the last span if same origin
                        if let Some(last) = new_spans.last_mut()
                            && last.origin == event_id
                        {
                            last.len += len;
                            continue;
                        }
                        new_spans.push(Span {
                            len,
                            origin: event_id,
                        });
                    }
                }
            }
        }

        self.spans = new_spans;
    }

    /// Copy `len` bytes worth of spans from the current document starting
    /// at `old_pos`, preserving their original provenance.
    fn copy_spans_from_old(
        &self,
        new_spans: &mut Vec<Span>,
        old_pos: &mut usize,
        mut remaining: usize,
    ) {
        let mut span_offset = 0usize;
        // Past the end of the document there is nothing to copy.
        let mut span_idx = self.spans.len();

        // Find the span containing old_pos
        let mut cumulative = 0usize;
        for (i, span) in self.spans.iter().enumerate() {
            if cumulative + span.len > *old_pos {
                span_idx = i;
                span_offset = *old_pos - cumulative;
                break;
            }
            cumulative += span.len;
        }

        while remaining > 0 && span_idx < self.spans.len() {
            let span = &self.spans[span_idx];
            let available = span.len - span_offset;
            let take = remaining.min(available);

            // Merge with last span if same origin
            if let Some(last) = new_spans.last_mut() {
                if last.origin == span.origin {
                    last.len += take;
                } else {
                    new_spans.push(Span {
                        len: take,
                        origin: span.origin,
                    });
                }
            } else {
                new_spans.push(Span {
                    len: take,
                    origin: span.origin,
                });
            }

            remaining -= take;
            *old_pos += take;
            span_offset = 0;
            span_idx += 1;
        }
    }

    /// Aggregate surviving characters per event.
    pub fn composition_by_event(&self) -> HashMap<Ulid, usize> {
        let mut map = HashMap::new();
        for span in &self.spans {
            *map.entry(span.origin).or_insert(0) += span.len;
        }
        map
    }
}

/// Origin of characters replay couldn't attribute to any event: gaps between
/// events and content that predates tracking (§6.5). The nil ULID, which no
/// real event has. Consumers report it as `untracked`.
pub const UNTRACKED_ORIGIN: Ulid = Ulid::nil();

/// Model name for the characters of one origin: `untracked` for
/// [`UNTRACKED_ORIGIN`], the event's model, or `unknown` if the event has
/// no known session.
pub fn model_for_origin(origin: Ulid, event_models: &HashMap<Ulid, String>) -> String {
    if origin == UNTRACKED_ORIGIN {
        return "untracked".to_string();
    }
    event_models
        .get(&origin)
        .cloned()
        .unwrap_or_else(|| "unknown".to_string())
}

/// Surviving characters per origin across all replayed files (§8).
pub fn surviving_by_event(files: &[FileComposition]) -> HashMap<Ulid, usize> {
    let mut surviving = HashMap::new();
    for fc in files {
        for (origin, chars) in fc.doc.composition_by_event() {
            *surviving.entry(origin).or_insert(0) += chars;
        }
    }
    surviving
}

/// Result of replaying events for one file.
#[derive(Debug)]
pub struct FileComposition {
    pub path: String,
    pub doc: ProvenanceDoc,
}

/// Replay a full event stream, returning one ProvenanceDoc per path.
/// `blob_reader` resolves git blob hashes to content.
/// Events must be sorted by timestamp (oldest first).
///
/// §6.5: whenever an event's `before` differs from the file as replay last
/// left it (content that predates tracking, or edits made without an
/// event), the difference is applied first as [`UNTRACKED_ORIGIN`], so each
/// document always matches the latest `after` exactly.
pub fn replay_all(
    events: &[crate::model::Event],
    blob_reader: &impl Fn(&str) -> Option<Vec<u8>>,
) -> Vec<FileComposition> {
    let mut docs: HashMap<String, ProvenanceDoc> = HashMap::new();
    let mut last_after: HashMap<String, String> = HashMap::new();

    for event in events {
        let before = blob_reader(&event.before_blob)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let after = blob_reader(&event.after_blob)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();

        let doc = docs.entry(event.path.clone()).or_default();
        let replayed = last_after.get(&event.path).map_or("", String::as_str);
        if replayed != before {
            doc.apply(UNTRACKED_ORIGIN, replayed, &before);
        }
        if event.transparent {
            doc.apply_transparent(&before, &after);
        } else {
            doc.apply(event.id, &before, &after);
        }
        last_after.insert(event.path.clone(), after);
    }

    docs.into_iter()
        .map(|(path, doc)| FileComposition { path, doc })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ulid() -> Ulid {
        Ulid::generate()
    }

    #[test]
    fn new_file_all_chars_attributed_to_event_6_1() {
        // §6.1: creating a file attributes all characters to that event
        let mut doc = ProvenanceDoc::new();
        let eid = ulid();
        doc.apply(eid, "", "hello world");
        assert_eq!(doc.total_len(), 11);
        assert_eq!(doc.spans.len(), 1);
        assert_eq!(doc.spans[0].origin, eid);
        assert_eq!(doc.spans[0].len, 11);
    }

    #[test]
    fn append_preserves_original_spans_6_1() {
        // §6.1: appending text preserves original provenance for existing chars
        let mut doc = ProvenanceDoc::new();
        let e1 = ulid();
        doc.apply(e1, "", "hello");
        std::thread::sleep(std::time::Duration::from_millis(2));

        let e2 = ulid();
        doc.apply(e2, "hello", "hello world");
        assert_eq!(doc.total_len(), 11);
        assert_eq!(doc.spans.len(), 2);
        assert_eq!(doc.spans[0], Span { len: 5, origin: e1 }); // "hello" kept
        assert_eq!(doc.spans[1], Span { len: 6, origin: e2 }); // " world" new
    }

    #[test]
    fn delete_removes_chars_preserves_surrounding_6_1() {
        // §6.1: deleting chars removes them; surrounding provenance unchanged
        let mut doc = ProvenanceDoc::new();
        let e1 = ulid();
        doc.apply(e1, "", "abcdef");
        std::thread::sleep(std::time::Duration::from_millis(2));

        let e2 = ulid();
        doc.apply(e2, "abcdef", "abef"); // delete "cd"
        assert_eq!(doc.total_len(), 4);
        // Remaining chars are all from e1
        assert_eq!(doc.spans.len(), 1);
        assert_eq!(doc.spans[0], Span { len: 4, origin: e1 });
    }

    #[test]
    fn replace_middle_splits_spans_6_1() {
        // §6.1: replacing chars in the middle creates three spans
        let mut doc = ProvenanceDoc::new();
        let e1 = ulid();
        doc.apply(e1, "", "aaabbbccc");
        std::thread::sleep(std::time::Duration::from_millis(2));

        let e2 = ulid();
        doc.apply(e2, "aaabbbccc", "aaaXXXccc"); // replace "bbb" with "XXX"
        assert_eq!(doc.total_len(), 9);
        assert_eq!(doc.spans.len(), 3);
        assert_eq!(doc.spans[0], Span { len: 3, origin: e1 }); // "aaa"
        assert_eq!(doc.spans[1], Span { len: 3, origin: e2 }); // "XXX"
        assert_eq!(doc.spans[2], Span { len: 3, origin: e1 }); // "ccc"
    }

    #[test]
    fn composition_by_event_counts_chars_8() {
        // §8: composition weighted by characters
        let mut doc = ProvenanceDoc::new();
        let e1 = ulid();
        doc.apply(e1, "", "aaaa"); // 4 chars
        std::thread::sleep(std::time::Duration::from_millis(2));

        let e2 = ulid();
        doc.apply(e2, "aaaa", "aaaabb"); // append "bb" (2 chars)

        let comp = doc.composition_by_event();
        assert_eq!(comp[&e1], 4);
        assert_eq!(comp[&e2], 2);
    }

    #[test]
    fn three_models_interleave_on_one_file_8() {
        // §8: three different events (representing three models) on one file
        let mut doc = ProvenanceDoc::new();

        let e1 = ulid();
        doc.apply(e1, "", "fn main() {}");
        std::thread::sleep(std::time::Duration::from_millis(2));

        let e2 = ulid();
        doc.apply(
            e2,
            "fn main() {}",
            "fn main() {\n    println!(\"hello\");\n}",
        );
        std::thread::sleep(std::time::Duration::from_millis(2));

        let e3 = ulid();
        doc.apply(
            e3,
            "fn main() {\n    println!(\"hello\");\n}",
            "fn main() {\n    println!(\"hello world\");\n}",
        );

        let comp = doc.composition_by_event();
        // e1 wrote "fn main() {" and "}" — but e2 rearranged things
        // The important assertion: all three events should have some surviving chars
        assert!(comp.contains_key(&e1), "e1 should have surviving chars");
        assert!(comp.contains_key(&e2), "e2 should have surviving chars");
        assert!(comp.contains_key(&e3), "e3 should have surviving chars");
        // Total should match current file length
        let total: usize = comp.values().sum();
        assert_eq!(
            total,
            "fn main() {\n    println!(\"hello world\");\n}".len()
        );
    }

    #[test]
    fn delete_entire_file_yields_empty_doc_6_1() {
        // §6.1: deleting all content empties the piece table
        let mut doc = ProvenanceDoc::new();
        let e1 = ulid();
        doc.apply(e1, "", "content");
        std::thread::sleep(std::time::Duration::from_millis(2));

        let e2 = ulid();
        doc.apply(e2, "content", "");
        assert_eq!(doc.total_len(), 0);
        assert!(doc.spans.is_empty());
    }

    #[test]
    fn rewrite_entire_file_all_new_provenance_6_1() {
        // §6.1: rewriting with fully disjoint characters → all new provenance.
        // Use strings with zero char overlap so the diff is a pure delete+insert.
        let mut doc = ProvenanceDoc::new();
        let e1 = ulid();
        doc.apply(e1, "", "aaaa");
        std::thread::sleep(std::time::Duration::from_millis(2));

        let e2 = ulid();
        doc.apply(e2, "aaaa", "zzzz");
        let comp = doc.composition_by_event();
        // No chars from e1 survive — 'a' and 'z' share nothing
        assert!(!comp.contains_key(&e1));
        assert_eq!(comp[&e2], 4);
    }

    #[test]
    fn transparent_events_skipped_in_replay_6_3() {
        // §6.3: transparent events (formatters) don't reattribute
        let events = vec![crate::model::Event {
            id: ulid(),
            session_id: ulid(),
            prompt_id: None,
            tool_call_id: None,
            path: "a.rs".to_string(),
            before_blob: "blob1".to_string(),
            after_blob: "blob2".to_string(),
            transparent: true, // formatter run
            evidence: crate::model::Evidence::ToolCall,
            ts: jiff::Timestamp::now(),
        }];

        let results = replay_all(&events, &|_| Some(b"content".to_vec()));
        // No event is credited with the formatter's content.
        for f in &results {
            assert!(!f.doc.composition_by_event().contains_key(&events[0].id));
        }
    }

    #[test]
    fn transparent_reformat_keeps_the_original_author_6_3() {
        let mut log = Log::default();
        let e1 = log.push("a.rs", "", "fn a(){}");
        let fmt = log.push("a.rs", "fn a(){}", "fn a() {\n}\n");
        log.events[1].transparent = true;
        let files = log.replay();
        let doc = doc_for(&files, "a.rs");

        assert_eq!(doc.total_len(), "fn a() {\n}\n".len());
        let comp = doc.composition_by_event();
        assert_eq!(comp.get(&e1), Some(&"fn a() {\n}\n".len()), "{comp:?}");
        assert!(!comp.contains_key(&fmt));
        assert!(!comp.contains_key(&UNTRACKED_ORIGIN));
    }

    #[test]
    fn edit_after_a_transparent_event_is_not_a_gap_6_3() {
        let mut log = Log::default();
        let e1 = log.push("a.rs", "", "x=1");
        log.push("a.rs", "x=1", "x = 1");
        log.events[1].transparent = true;
        let e3 = log.push("a.rs", "x = 1", "x = 1\ny = 2");
        let files = log.replay();
        let comp = doc_for(&files, "a.rs").composition_by_event();

        assert_eq!(comp.get(&e1), Some(&5), "{comp:?}");
        assert_eq!(comp.get(&e3), Some(&6));
        assert!(!comp.contains_key(&UNTRACKED_ORIGIN));
    }

    #[test]
    fn transparent_insert_at_start_takes_the_following_origin_6_3() {
        let mut log = Log::default();
        let e1 = log.push("a.rs", "", "body");
        log.push("a.rs", "body", "  body");
        log.events[1].transparent = true;
        let comp = doc_for(&log.replay(), "a.rs").composition_by_event();
        assert_eq!(comp.get(&e1), Some(&6), "{comp:?}");
    }

    #[test]
    fn replay_all_multiple_files_6_1() {
        // §6.1: replay handles multiple files independently
        let s = ulid(); // session
        let e1 = Ulid::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let e2 = Ulid::generate();

        let events = vec![
            crate::model::Event {
                id: e1,
                session_id: s,
                prompt_id: None,
                tool_call_id: None,
                path: "a.rs".to_string(),
                before_blob: "empty".to_string(),
                after_blob: "a_content".to_string(),
                transparent: false,
                evidence: crate::model::Evidence::ToolCall,
                ts: jiff::Timestamp::now(),
            },
            crate::model::Event {
                id: e2,
                session_id: s,
                prompt_id: None,
                tool_call_id: None,
                path: "b.rs".to_string(),
                before_blob: "empty".to_string(),
                after_blob: "b_content".to_string(),
                transparent: false,
                evidence: crate::model::Evidence::ToolCall,
                ts: jiff::Timestamp::now(),
            },
        ];

        let blob_reader = |hash: &str| -> Option<Vec<u8>> {
            match hash {
                "empty" => Some(b"".to_vec()),
                "a_content" => Some(b"hello from a".to_vec()),
                "b_content" => Some(b"hello from b".to_vec()),
                _ => None,
            }
        };

        let results = replay_all(&events, &blob_reader);
        assert_eq!(results.len(), 2);
    }

    // --- §6.5 replay-time gap bridging ---

    /// An in-memory event log: blobs are keyed by their own content.
    #[derive(Default)]
    struct Log {
        events: Vec<crate::model::Event>,
        last_id: Option<Ulid>,
    }

    impl Log {
        fn push(&mut self, path: &str, before: &str, after: &str) -> Ulid {
            let id = match self.last_id {
                Some(prev) => prev.increment().unwrap(),
                None => Ulid::generate(),
            };
            self.last_id = Some(id);
            self.events.push(crate::model::Event {
                id,
                session_id: Ulid::generate(),
                prompt_id: None,
                tool_call_id: None,
                path: path.to_string(),
                before_blob: format!("b:{before}"),
                after_blob: format!("b:{after}"),
                transparent: false,
                evidence: crate::model::Evidence::ToolCall,
                ts: jiff::Timestamp::now(),
            });
            id
        }

        fn replay(&self) -> Vec<FileComposition> {
            replay_all(&self.events, &|hash: &str| {
                hash.strip_prefix("b:").map(|s| s.as_bytes().to_vec())
            })
        }
    }

    fn doc_for<'a>(files: &'a [FileComposition], path: &str) -> &'a ProvenanceDoc {
        &files.iter().find(|f| f.path == path).unwrap().doc
    }

    #[test]
    fn untracked_origin_is_the_nil_ulid_6_5() {
        assert_eq!(UNTRACKED_ORIGIN, Ulid::nil());
        assert_ne!(UNTRACKED_ORIGIN, Ulid::generate());
    }

    #[test]
    fn apply_never_reads_past_the_end_of_the_doc_6_5() {
        // Doc tracks 3 bytes, but the event's `before` claims 8. The 5 bytes
        // beyond the end must not be copied again from the start.
        let mut doc = ProvenanceDoc::new();
        let e1 = ulid();
        doc.apply(e1, "", "abc");
        let e2 = ulid();
        doc.apply(e2, "abcdefgh", "abcdefghX");

        let comp = doc.composition_by_event();
        assert_eq!(comp.get(&e1), Some(&3), "e1 inserted 3 bytes: {comp:?}");
        assert_eq!(comp.get(&e2), Some(&1));
    }

    #[test]
    fn preexisting_content_is_untracked_6_5() {
        let mut log = Log::default();
        let e = log.push("a.rs", "old\n", "old\nnew\n");
        let files = log.replay();
        let doc = doc_for(&files, "a.rs");

        assert_eq!(doc.total_len(), "old\nnew\n".len());
        let comp = doc.composition_by_event();
        assert_eq!(comp.get(&UNTRACKED_ORIGIN), Some(&4), "{comp:?}");
        assert_eq!(comp.get(&e), Some(&4));
    }

    #[test]
    fn gap_between_events_is_untracked_6_5() {
        let mut log = Log::default();
        let e1 = log.push("a.rs", "", "hello");
        // Hand edit appended " world" without an event.
        let e2 = log.push("a.rs", "hello world", "hello world!");
        let files = log.replay();
        let comp = doc_for(&files, "a.rs").composition_by_event();

        assert_eq!(comp.get(&e1), Some(&5), "{comp:?}");
        assert_eq!(comp.get(&UNTRACKED_ORIGIN), Some(&6));
        assert_eq!(comp.get(&e2), Some(&1));
    }

    #[test]
    fn gap_that_only_deletes_adds_no_untracked_chars_6_5() {
        let mut log = Log::default();
        let e1 = log.push("a.rs", "", "hello world");
        // Hand edit deleted " world".
        let e2 = log.push("a.rs", "hello", "hello!");
        let files = log.replay();
        let comp = doc_for(&files, "a.rs").composition_by_event();

        assert_eq!(comp.get(&e1), Some(&5), "{comp:?}");
        assert_eq!(comp.get(&e2), Some(&1));
        assert!(!comp.contains_key(&UNTRACKED_ORIGIN));
    }

    #[test]
    fn gap_free_log_has_no_untracked_chars_6_5() {
        let mut log = Log::default();
        log.push("a.rs", "", "fn a() {}\n");
        log.push("a.rs", "fn a() {}\n", "fn a() {}\nfn b() {}\n");
        log.push("b.rs", "", "x");
        let files = log.replay();
        for f in &files {
            assert!(
                !f.doc.composition_by_event().contains_key(&UNTRACKED_ORIGIN),
                "{}",
                f.path
            );
        }
    }

    #[test]
    fn gaps_are_tracked_per_file_6_5() {
        let mut log = Log::default();
        log.push("a.rs", "", "aaa");
        log.push("b.rs", "", "bbb");
        // a.rs continues cleanly; b.rs had a hand edit.
        log.push("a.rs", "aaa", "aaaA");
        log.push("b.rs", "bbbHAND", "bbbHANDB");
        let files = log.replay();
        assert!(
            !doc_for(&files, "a.rs")
                .composition_by_event()
                .contains_key(&UNTRACKED_ORIGIN)
        );
        assert_eq!(
            doc_for(&files, "b.rs")
                .composition_by_event()
                .get(&UNTRACKED_ORIGIN),
            Some(&4)
        );
    }

    #[test]
    fn surviving_by_event_sums_across_files_6_5() {
        let mut log = Log::default();
        log.push("a.rs", "pre", "pre1");
        log.push("b.rs", "pre", "pre2");
        let surviving = surviving_by_event(&log.replay());
        assert_eq!(surviving.get(&UNTRACKED_ORIGIN), Some(&6));
    }

    #[test]
    fn model_for_origin_maps_untracked_and_unknown_6_5() {
        let known = ulid();
        let models = HashMap::from([(known, "opus".to_string())]);
        assert_eq!(model_for_origin(known, &models), "opus");
        assert_eq!(model_for_origin(UNTRACKED_ORIGIN, &models), "untracked");
        assert_eq!(model_for_origin(ulid(), &models), "unknown");
    }

    /// Small deterministic PRNG so the property tests need no extra crate.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                (self.next() as usize) % n
            }
        }
        fn text(&mut self) -> String {
            const ALPHABET: [char; 10] = ['a', 'b', '}', '\n', ' ', 'x', 'é', '🎉', '{', ';'];
            (0..1 + self.below(6))
                .map(|_| ALPHABET[self.below(ALPHABET.len())])
                .collect()
        }
        /// Insert, delete or replace a random range of `s`.
        fn mutate(&mut self, s: &str) -> String {
            let mut chars: Vec<char> = s.chars().collect();
            let at = self.below(chars.len() + 1);
            let len = self.below(chars.len() - at + 1).min(4);
            match self.below(3) {
                0 => {
                    chars.splice(at..at, self.text().chars().collect::<Vec<_>>());
                }
                1 => {
                    chars.drain(at..at + len);
                }
                _ => {
                    chars.splice(at..at + len, self.text().chars().collect::<Vec<_>>());
                }
            }
            chars.into_iter().collect()
        }
    }

    /// Random multi-file histories with hand edits between events, and
    /// files that already had content when tracking started.
    fn random_log(seed: u64, steps: usize) -> (Log, HashMap<String, String>) {
        let mut rng = Lcg(seed);
        let mut log = Log::default();
        let mut disk: HashMap<String, String> = HashMap::new();
        let mut last_after: HashMap<String, String> = HashMap::new();
        for _ in 0..steps {
            let path = ["a.rs", "b.rs", "c.rs"][rng.below(3)].to_string();
            let mut current = disk.get(&path).cloned().unwrap_or_else(|| {
                if rng.below(2) == 0 {
                    rng.text()
                } else {
                    String::new()
                }
            });
            if rng.below(10) < 3 {
                current = rng.mutate(&current); // hand edit, no event
            }
            let after = rng.mutate(&current);
            log.push(&path, &current, &after);
            disk.insert(path.clone(), after.clone());
            last_after.insert(path, after);
        }
        (log, last_after)
    }

    #[test]
    fn doc_length_always_matches_the_last_after_6_5() {
        for seed in 1..=40 {
            let (log, last_after) = random_log(seed, 60);
            for f in log.replay() {
                assert_eq!(
                    f.doc.total_len(),
                    last_after[&f.path].len(),
                    "seed {seed}, {}",
                    f.path
                );
            }
        }
    }

    #[test]
    fn no_event_survives_more_than_it_inserted_6_5() {
        for seed in 1..=40 {
            let (log, _) = random_log(seed, 60);
            let surviving = surviving_by_event(&log.replay());
            for e in &log.events {
                let before = e.before_blob.strip_prefix("b:").unwrap();
                let after = e.after_blob.strip_prefix("b:").unwrap();
                let inserted = crate::stats::emitted_chars(before, after);
                let alive = surviving.get(&e.id).copied().unwrap_or(0);
                assert!(
                    alive <= inserted,
                    "seed {seed}: {alive} > {inserted} for {before:?} → {after:?}"
                );
            }
        }
    }

    #[test]
    fn every_prefix_of_a_log_keeps_the_invariants_6_5() {
        let (log, _) = random_log(7, 80);
        for n in (1..=log.events.len()).step_by(7) {
            let prefix = Log {
                events: log.events[..n].to_vec(),
                last_id: None,
            };
            let mut last_after: HashMap<&str, &str> = HashMap::new();
            for e in &prefix.events {
                last_after.insert(&e.path, e.after_blob.strip_prefix("b:").unwrap());
            }
            let files = prefix.replay();
            let tracked: usize = files.iter().map(|f| f.doc.total_len()).sum();
            let expected: usize = last_after.values().map(|s| s.len()).sum();
            assert_eq!(tracked, expected, "prefix of {n} events");
        }
    }
}
