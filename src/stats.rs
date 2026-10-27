//! Survival analytics. See SPEC.md §8.
//!
//! All stats are weighted by *characters*, not lines — line counts
//! systematically overvalue short lines. Composition itself lives in the
//! replay engine (`replay::ProvenanceDoc::composition_by_event`); this
//! module covers the temporal (survival / half-life) and spatial (affinity)
//! analytics on top of it.

use std::collections::{BTreeMap, HashMap};

use crate::model::Event;
use serde::Serialize;
use similar::{ChangeTag, TextDiff};

/// One model's emission record, bucketed by the age of the characters.
#[derive(Debug, Clone, PartialEq)]
pub struct EmissionBucket {
    /// Days between the characters being emitted and the measurement time.
    pub age_days: u32,
    /// Characters the model emitted at this age.
    pub emitted: usize,
    /// Of those emitted characters, how many survive today.
    pub surviving: usize,
}

/// A point on a model's survival curve.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SurvivalPoint {
    pub age_days: u32,
    /// Fraction of the model's emitted chars still alive at this age.
    pub survival: f64,
}

/// Code half-life per model: group emission buckets by age and compute the
/// surviving fraction at each age (`surviving / emitted`). Returns points
/// sorted by age ascending. Buckets with zero emitted chars yield a
/// `survival` of 0.0 rather than NaN.
pub fn half_life(buckets: &[EmissionBucket]) -> Vec<SurvivalPoint> {
    let mut by_age: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
    for b in buckets {
        let (emitted, surviving) = by_age.entry(b.age_days).or_insert((0, 0));
        *emitted += b.emitted;
        *surviving += b.surviving;
    }
    by_age
        .into_iter()
        .map(|(age_days, (emitted, surviving))| SurvivalPoint {
            age_days,
            survival: if emitted == 0 {
                0.0
            } else {
                surviving as f64 / emitted as f64
            },
        })
        .collect()
}

/// The half-life itself: the age at which survival first drops below 50%.
/// Returns `None` when no point ever crosses the threshold.
pub fn half_life_at(points: &[SurvivalPoint]) -> Option<u32> {
    points.iter().find(|p| p.survival < 0.5).map(|p| p.age_days)
}

/// Model × directory affinity: which parts of the tree each model owns.
/// Input is `(path, model, surviving_chars)` triples; output is keyed by
/// `(model, top-level directory)` and sums surviving chars. The top-level
/// directory is the first path segment, or `(root)` for files at the repo
/// root.
pub fn affinity(entries: &[(String, String, usize)]) -> BTreeMap<(String, String), usize> {
    let mut map = BTreeMap::new();
    for (path, model, chars) in entries {
        let dir = path
            .split_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| "(root)".to_string());
        *map.entry((model.clone(), dir)).or_insert(0) += chars;
    }
    map
}

/// Count the characters an event inserted (its emission) via a char diff.
/// §8: emissions are weighted by characters, not lines.
pub fn emitted_chars(before: &str, after: &str) -> usize {
    TextDiff::from_chars(before, after)
        .iter_all_changes()
        .filter(|c| c.tag() == ChangeTag::Insert)
        .map(|c| c.value().len())
        .sum()
}

/// Build per-model emission buckets from the event log (§8).
///
/// Each non-transparent event contributes one `EmissionBucket` keyed by its
/// model: `emitted` = chars the event inserted (char diff of before→after),
/// `surviving` = chars from that event still present in the current tree,
/// `age_days` = whole days between the event and `now_seconds`. Transparent
/// events (§6.3) are skipped — they pass provenance through, they don't emit.
pub fn build_emission_buckets<F>(
    events: &[Event],
    session_models: &HashMap<ulid::Ulid, String>,
    surviving_by_event: &HashMap<ulid::Ulid, usize>,
    blob_reader: F,
    now_seconds: i64,
) -> BTreeMap<String, Vec<EmissionBucket>>
where
    F: Fn(&str) -> Option<Vec<u8>>,
{
    let mut buckets: BTreeMap<String, Vec<EmissionBucket>> = BTreeMap::new();
    for e in events.iter().filter(|e| !e.transparent) {
        let model = session_models
            .get(&e.session_id)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        let before = blob_reader(&e.before_blob)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let after = blob_reader(&e.after_blob)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let emitted = emitted_chars(&before, &after);
        let surviving = surviving_by_event.get(&e.id).copied().unwrap_or(0);
        let age_seconds = now_seconds.saturating_sub(e.ts.as_second()).max(0);
        let age_days = (age_seconds / 86_400) as u32;
        buckets.entry(model).or_default().push(EmissionBucket {
            age_days,
            emitted,
            surviving,
        });
    }
    buckets
}

/// Per-model half-life curves: apply [`half_life`] to each model's buckets.
pub fn half_life_by_model(
    buckets: &BTreeMap<String, Vec<EmissionBucket>>,
) -> BTreeMap<String, Vec<SurvivalPoint>> {
    buckets
        .iter()
        .map(|(model, bs)| (model.clone(), half_life(bs)))
        .collect()
}

/// One point on the stratigraphy chart (§9.4): the cumulative surviving
/// chars per model up to a point in time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StratigraphyPoint {
    /// Unix seconds of the event that advanced the timeline.
    pub ts: i64,
    /// model → cumulative surviving chars up to and including this time.
    pub layers: BTreeMap<String, usize>,
}

/// Stratigraphy (§9.4): accumulate surviving chars per model over time,
/// producing one point per input row carrying the running totals. Rows must
/// be sorted by time ascending.
pub fn stratigraphy(rows: &[(i64, String, usize)]) -> Vec<StratigraphyPoint> {
    let mut running: BTreeMap<String, usize> = BTreeMap::new();
    let mut points = Vec::with_capacity(rows.len());
    for (ts, model, chars) in rows {
        *running.entry(model.clone()).or_insert(0) += chars;
        points.push(StratigraphyPoint {
            ts: *ts,
            layers: running.clone(),
        });
    }
    points
}

/// Dominant model in a composition: the model with the most surviving chars.
/// Ties break alphabetically for deterministic output. Empty → `None`.
pub fn dominant_model(composition: &HashMap<String, usize>) -> Option<String> {
    let mut pairs: Vec<(&String, &usize)> = composition.iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    pairs.first().map(|(m, _)| (*m).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Event, Evidence};
    use std::collections::HashMap;

    fn ulid() -> ulid::Ulid {
        ulid::Ulid::generate()
    }

    #[test]
    fn half_life_buckets_survival_by_age_8() {
        // §8: survival per age = surviving chars / emitted chars, bucketed
        let buckets = vec![
            EmissionBucket {
                age_days: 0,
                emitted: 100,
                surviving: 90,
            },
            EmissionBucket {
                age_days: 0,
                emitted: 50,
                surviving: 10,
            },
            EmissionBucket {
                age_days: 10,
                emitted: 100,
                surviving: 30,
            },
        ];
        let points = half_life(&buckets);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].age_days, 0);
        assert!((points[0].survival - 100.0 / 150.0).abs() < 1e-9);
        assert_eq!(points[1].age_days, 10);
        assert!((points[1].survival - 0.3).abs() < 1e-9);
    }

    #[test]
    fn half_life_empty_input_yields_empty_curve_8() {
        // §8: no emissions → no curve
        assert!(half_life(&[]).is_empty());
    }

    #[test]
    fn half_life_zero_emitted_avoids_nan_8() {
        // §8: zero emitted chars at an age must not produce NaN
        let buckets = vec![EmissionBucket {
            age_days: 0,
            emitted: 0,
            surviving: 0,
        }];
        let points = half_life(&buckets);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].survival, 0.0);
    }

    #[test]
    fn half_life_at_first_age_below_half_8() {
        // §8: half-life is the age at which survival first drops below 50%
        let points = vec![
            SurvivalPoint {
                age_days: 0,
                survival: 1.0,
            },
            SurvivalPoint {
                age_days: 7,
                survival: 0.6,
            },
            SurvivalPoint {
                age_days: 14,
                survival: 0.4,
            },
        ];
        assert_eq!(half_life_at(&points), Some(14));
    }

    #[test]
    fn half_life_at_none_if_never_below_half_8() {
        // §8: if survival never drops below 50%, there is no half-life
        let points = vec![SurvivalPoint {
            age_days: 0,
            survival: 0.9,
        }];
        assert_eq!(half_life_at(&points), None);
    }

    #[test]
    fn affinity_groups_by_model_and_top_dir_8() {
        // §8: model × directory affinity sums surviving chars per top-level dir
        let entries = vec![
            ("src/main.rs".to_string(), "opus".to_string(), 10),
            ("src/lib.rs".to_string(), "opus".to_string(), 20),
            ("tests/main.rs".to_string(), "gpt".to_string(), 5),
            ("src/lib.rs".to_string(), "gpt".to_string(), 7),
        ];
        let map = affinity(&entries);
        assert_eq!(map.get(&("opus".to_string(), "src".to_string())), Some(&30));
        assert_eq!(map.get(&("gpt".to_string(), "tests".to_string())), Some(&5));
        assert_eq!(map.get(&("gpt".to_string(), "src".to_string())), Some(&7));
    }

    // --- §8 / §9.4 analytics wired into the report ---

    #[test]
    fn emitted_chars_counts_insertions_8() {
        // §8: emission = chars an event inserted (char-level insert count)
        assert_eq!(emitted_chars("abc", "abXXc"), 2);
        assert_eq!(emitted_chars("", "hello"), 5);
        assert_eq!(emitted_chars("hello", "hello"), 0);
        assert_eq!(emitted_chars("hello", ""), 0);
    }

    #[test]
    fn build_emission_buckets_groups_by_model_8() {
        // §8: per-model emission buckets — emitted vs surviving at an age;
        // transparent events are skipped.
        let session = ulid();
        let e1 = ulid();
        let mut events = vec![Event {
            id: e1,
            session_id: session,
            prompt_id: None,
            tool_call_id: None,
            path: "a.rs".to_string(),
            before_blob: "b1".to_string(),
            after_blob: "a1".to_string(),
            transparent: false,
            evidence: Evidence::ToolCall,
            ts: jiff::Timestamp::from_second(1_000_000).unwrap(),
        }];
        // A transparent event must not produce a bucket.
        let mut e2 = events[0].clone();
        e2.id = ulid();
        e2.transparent = true;
        e2.after_blob = "a2".to_string();
        events.push(e2);

        let mut session_models = HashMap::new();
        session_models.insert(session, "opus".to_string());
        let mut surviving = HashMap::new();
        surviving.insert(e1, 1usize);

        let blob_reader = |hash: &str| -> Option<Vec<u8>> {
            match hash {
                "b1" => Some(b"abc".to_vec()),
                "a1" => Some(b"abXXc".to_vec()),
                "a2" => Some(b"whatever".to_vec()),
                _ => None,
            }
        };

        let buckets =
            build_emission_buckets(&events, &session_models, &surviving, blob_reader, 1_000_000);
        assert_eq!(
            buckets.len(),
            1,
            "only the non-transparent event emits a bucket"
        );
        let b = &buckets["opus"];
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].age_days, 0);
        assert_eq!(b[0].emitted, 2, "\"abc\" → \"abXXc\" inserts 2 chars");
        assert_eq!(b[0].surviving, 1);
    }

    // --- §8 half-life end to end: event log → replay → curves ---

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    /// One step of a history: `(model, path, before, after, age_days)`.
    /// Steps must be given oldest first.
    type Step<'a> = (&'a str, &'a str, &'a str, &'a str, i64);

    fn events_for(steps: &[Step]) -> (Vec<Event>, HashMap<ulid::Ulid, String>) {
        let mut sessions: HashMap<String, ulid::Ulid> = HashMap::new();
        let mut session_models = HashMap::new();
        let mut id = ulid::Ulid::nil();
        let mut events = Vec::new();
        for (model, path, before, after, age) in steps {
            let session = *sessions.entry(model.to_string()).or_insert_with(ulid);
            session_models.insert(session, model.to_string());
            id = id.increment().unwrap();
            events.push(Event {
                id,
                session_id: session,
                prompt_id: None,
                tool_call_id: None,
                path: path.to_string(),
                before_blob: format!("b:{before}"),
                after_blob: format!("b:{after}"),
                transparent: false,
                evidence: Evidence::ToolCall,
                ts: jiff::Timestamp::from_second(NOW - age * DAY).unwrap(),
            });
        }
        (events, session_models)
    }

    fn read_blob(hash: &str) -> Option<Vec<u8>> {
        hash.strip_prefix("b:").map(|s| s.as_bytes().to_vec())
    }

    fn buckets_for(
        events: &[Event],
        models: &HashMap<ulid::Ulid, String>,
    ) -> BTreeMap<String, Vec<EmissionBucket>> {
        let files = crate::replay::replay_all(events, &read_blob);
        let surviving = crate::replay::surviving_by_event(&files);
        build_emission_buckets(events, models, &surviving, read_blob, NOW)
    }

    fn curves(steps: &[Step]) -> BTreeMap<String, Vec<SurvivalPoint>> {
        let (events, models) = events_for(steps);
        half_life_by_model(&buckets_for(&events, &models))
    }

    fn survival(curves: &BTreeMap<String, Vec<SurvivalPoint>>, model: &str, age: u32) -> f64 {
        curves[model]
            .iter()
            .find(|p| p.age_days == age)
            .unwrap_or_else(|| panic!("no {model} point at age {age}: {curves:?}"))
            .survival
    }

    #[test]
    fn survival_is_one_when_nothing_is_rewritten_8() {
        let c = curves(&[
            ("opus", "a.rs", "", "fn a() {}\n", 2),
            ("opus", "a.rs", "fn a() {}\n", "fn a() {}\nfn b() {}\n", 1),
        ]);
        assert_eq!(survival(&c, "opus", 2), 1.0);
        assert_eq!(survival(&c, "opus", 1), 1.0);
    }

    #[test]
    fn survival_is_zero_when_fully_rewritten_8() {
        let c = curves(&[
            ("opus", "a.rs", "", "aaaa", 5),
            ("gpt", "a.rs", "aaaa", "zzzz", 0),
        ]);
        assert_eq!(survival(&c, "opus", 5), 0.0);
        assert_eq!(survival(&c, "gpt", 0), 1.0);
    }

    #[test]
    fn survival_is_half_when_half_is_deleted_8() {
        let c = curves(&[
            ("opus", "a.rs", "", "abcdefgh", 3),
            ("gpt", "a.rs", "abcdefgh", "abcd", 0),
        ]);
        assert_eq!(survival(&c, "opus", 3), 0.5);
        // gpt only deleted: it emitted nothing.
        assert_eq!(survival(&c, "gpt", 0), 0.0);
    }

    #[test]
    fn rewriting_another_models_code_lowers_only_its_survival_8() {
        let c = curves(&[
            ("opus", "a.rs", "", "opus code", 4),
            ("gpt", "b.rs", "", "gpt code", 4),
            ("gpt", "a.rs", "opus code", "opus", 0),
        ]);
        assert!(survival(&c, "opus", 4) < 1.0);
        assert_eq!(survival(&c, "gpt", 4), 1.0);
    }

    #[test]
    fn survival_stays_within_bounds_with_preexisting_content_8() {
        // A file that already had content when tracking started.
        let c = curves(&[
            (
                "opus",
                "a.rs",
                "legacy line\nlegacy line\n",
                "legacy line\nlegacy line\nnew\n",
                1,
            ),
            (
                "opus",
                "a.rs",
                "legacy line\nlegacy line\nnew\n",
                "legacy line\nlegacy line\nnew\nmore\n",
                0,
            ),
        ]);
        assert_eq!(survival(&c, "opus", 1), 1.0, "{c:?}");
        assert_eq!(survival(&c, "opus", 0), 1.0, "{c:?}");
    }

    #[test]
    fn survival_stays_within_bounds_across_hand_edit_gaps_8() {
        // Hand edits between events: each `before` differs from the last `after`.
        let c = curves(&[
            ("opus", "a.rs", "", "one\n", 3),
            (
                "opus",
                "a.rs",
                "one\nhand\nhand\nhand\n",
                "one\nhand\nhand\nhand\ntwo\n",
                2,
            ),
            (
                "gpt",
                "a.rs",
                "one\nhand\nhand\nhand\ntwo\nhand again\n",
                "one\nhand\nhand\nhand\ntwo\nhand again\nthree\n",
                1,
            ),
        ]);
        for (model, points) in &c {
            for p in points {
                assert!(
                    (0.0..=1.0).contains(&p.survival),
                    "{model} at {}: {}",
                    p.age_days,
                    p.survival
                );
            }
        }
        assert_eq!(survival(&c, "opus", 3), 1.0);
        assert_eq!(survival(&c, "opus", 2), 1.0);
        assert_eq!(survival(&c, "gpt", 1), 1.0);
    }

    #[test]
    fn untracked_content_never_gets_a_curve_8() {
        let c = curves(&[
            ("opus", "a.rs", "pre-existing\n", "pre-existing\nopus\n", 1),
            (
                "opus",
                "a.rs",
                "pre-existing\nopus\nhand edit\n",
                "pre-existing\nopus\nhand edit\nopus\n",
                0,
            ),
        ]);
        assert_eq!(c.keys().collect::<Vec<_>>(), vec!["opus"]);
    }

    #[test]
    fn emitted_ignores_content_that_was_already_there_8() {
        let (events, models) = events_for(&[("opus", "a.rs", "0123456789", "0123456789ab", 0)]);
        let b = &buckets_for(&events, &models)["opus"];
        assert_eq!((b[0].emitted, b[0].surviving), (2, 2));
    }

    #[test]
    fn multibyte_characters_count_consistently_8() {
        let c = curves(&[
            ("opus", "a.rs", "", "héllo 🎉\n", 1),
            (
                "opus",
                "a.rs",
                "héllo 🎉\nwörld 🎉\n",
                "héllo 🎉\nwörld 🎉\n✓\n",
                0,
            ),
        ]);
        assert_eq!(survival(&c, "opus", 1), 1.0);
        assert_eq!(survival(&c, "opus", 0), 1.0);
    }

    #[test]
    fn events_are_bucketed_by_whole_days_of_age_8() {
        let c = curves(&[
            ("opus", "a.rs", "", "old", 10),
            ("opus", "b.rs", "", "mid", 3),
            ("opus", "c.rs", "", "new", 0),
        ]);
        let ages: Vec<u32> = c["opus"].iter().map(|p| p.age_days).collect();
        assert_eq!(ages, vec![0, 3, 10]);
    }

    #[test]
    fn age_rounds_down_to_whole_days_8() {
        let (mut events, models) = events_for(&[("opus", "a.rs", "", "x", 0)]);
        events[0].ts = jiff::Timestamp::from_second(NOW - DAY - DAY / 2).unwrap();
        assert_eq!(buckets_for(&events, &models)["opus"][0].age_days, 1);
    }

    #[test]
    fn events_from_the_future_have_age_zero_8() {
        // Clock skew between machines must not wrap the age around.
        let (mut events, models) = events_for(&[("opus", "a.rs", "", "x", 0)]);
        events[0].ts = jiff::Timestamp::from_second(NOW + 3 * DAY).unwrap();
        assert_eq!(buckets_for(&events, &models)["opus"][0].age_days, 0);
    }

    #[test]
    fn half_life_is_the_first_age_where_less_than_half_survives_8() {
        let c = curves(&[
            ("opus", "a.rs", "", "aaaaaaaaaa", 20),
            ("opus", "b.rs", "", "bbbbbbbbbb", 10),
            ("opus", "c.rs", "", "cccccccccc", 0),
            // Keep 3 of a.rs, 6 of b.rs, all of c.rs.
            ("gpt", "a.rs", "aaaaaaaaaa", "aaa", 0),
            ("gpt", "b.rs", "bbbbbbbbbb", "bbbbbb", 0),
        ]);
        assert_eq!(survival(&c, "opus", 10), 0.6);
        assert_eq!(survival(&c, "opus", 20), 0.3);
        assert_eq!(half_life_at(&c["opus"]), Some(20));
    }

    #[test]
    fn survival_is_bounded_on_random_histories_with_gaps_8() {
        let mut state: u64 = 0x5eed;
        let mut next = |n: usize| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as usize) % n.max(1)
        };
        for _ in 0..30 {
            let mut disk: HashMap<&str, String> = HashMap::new();
            let mut steps: Vec<(String, String, String, String, i64)> = Vec::new();
            for i in 0..40 {
                let path = ["a.rs", "b.rs"][next(2)];
                let mut before = disk
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| "x".repeat(next(4)));
                if next(3) == 0 {
                    before.push_str(&"h".repeat(1 + next(5))); // hand edit
                }
                let mut after = before.clone();
                let cut = next(after.len() + 1);
                after.truncate(cut);
                after.push_str(&"m".repeat(next(6)));
                disk.insert(path, after.clone());
                let model = ["opus", "gpt"][next(2)];
                steps.push((model.into(), path.into(), before, after, 40 - i));
            }
            let steps: Vec<Step> = steps
                .iter()
                .map(|(m, p, b, a, age)| (m.as_str(), p.as_str(), b.as_str(), a.as_str(), *age))
                .collect();
            for (model, points) in curves(&steps) {
                for p in points {
                    assert!(
                        (0.0..=1.0).contains(&p.survival),
                        "{model} at {}: {}",
                        p.age_days,
                        p.survival
                    );
                }
            }
        }
    }

    #[test]
    fn half_life_by_model_returns_per_model_curves_8() {
        // §8: half-life curves grouped by model
        let mut buckets = BTreeMap::new();
        buckets.insert(
            "opus".to_string(),
            vec![EmissionBucket {
                age_days: 0,
                emitted: 100,
                surviving: 90,
            }],
        );
        buckets.insert(
            "gpt".to_string(),
            vec![EmissionBucket {
                age_days: 0,
                emitted: 10,
                surviving: 5,
            }],
        );
        let curves = half_life_by_model(&buckets);
        assert_eq!(curves.len(), 2);
        assert!((curves["opus"][0].survival - 0.9).abs() < 1e-9);
        assert!((curves["gpt"][0].survival - 0.5).abs() < 1e-9);
    }

    #[test]
    fn stratigraphy_cumulates_surviving_by_model_9_4() {
        // §9.4 stratigraphy: stacked layers = cumulative surviving chars
        // per model, accumulated over time (geological layers).
        let rows = vec![
            (0i64, "opus".to_string(), 10usize),
            (1i64, "gpt".to_string(), 5usize),
            (2i64, "opus".to_string(), 3usize),
        ];
        let points = stratigraphy(&rows);
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].ts, 0);
        assert_eq!(points[0].layers.get("opus"), Some(&10));
        assert_eq!(points[1].layers.get("opus"), Some(&10));
        assert_eq!(points[1].layers.get("gpt"), Some(&5));
        assert_eq!(points[2].layers.get("opus"), Some(&13));
        assert_eq!(points[2].layers.get("gpt"), Some(&5));
    }

    #[test]
    fn dominant_model_picks_most_chars_with_deterministic_tiebreak_9_4() {
        // §9.4 treemap: dominant model = most surviving chars, ties broken
        // alphabetically for determinism.
        let mut comp = HashMap::new();
        comp.insert("opus".to_string(), 10);
        comp.insert("gpt".to_string(), 30);
        assert_eq!(dominant_model(&comp), Some("gpt".to_string()));

        let mut tie = HashMap::new();
        tie.insert("zebra".to_string(), 5);
        tie.insert("alpha".to_string(), 5);
        assert_eq!(dominant_model(&tie), Some("alpha".to_string()));

        assert_eq!(dominant_model(&HashMap::new()), None);
    }
}
