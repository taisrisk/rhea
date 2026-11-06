//! Finding the memories that matter for a message: full-text (BM25) and vector similarity
//! combined, then adjusted for how recent, how important, how well verified and how fitting to
//! the question each one is.
//!
//! The combination follows Palace RS's hybrid ranker (MIT): normalized BM25 beside a rescaled
//! cosine, a recency bonus with a 35-day half-life, small intent boosts when the question's
//! wording points at a kind of memory ("why did we choose…", "what failed last time"), and a
//! bonus for memories that match a time phrase such as "yesterday". Rhea adds a verified-outcome
//! bonus, the experience weight and project scoping.

use std::collections::{HashMap, HashSet};

use super::embed::Embedder;
use super::store::{Filter, Memory, Store};

const DAY_MS: i64 = 86_400_000;
/// Recency bonus for a memory made just now, and its decay rate (half-life about 35 days).
const RECENCY_WEIGHT: f64 = 0.06;
const RECENCY_LAMBDA: f64 = 0.02;
const DATE_WEIGHT: f64 = 0.12;
const INTENT_WEIGHT: f64 = 0.15;
/// Below this "related" share a memory is not about the query at all, and no bonus may bring it back.
const MIN_RELATED: f64 = 0.15;

const STOPWORDS: [&str; 48] = [
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "is", "are", "was", "were", "it", "this", "that", "i", "you", "we", "me", "my", "your", "do", "did", "does", "how", "what", "when", "where", "why", "can", "could", "should", "would", "please", "about", "from", "be", "have", "has", "had", "at", "as", "by", "if", "so",
];

#[derive(Debug, Clone)]
pub struct Hit {
    pub memory: Memory,
    /// The ranking score (relatedness plus recency, importance and the other adjustments).
    pub score: f64,
    /// How strongly the memory matches the query, 0 to 1: the embedding similarity rescaled
    /// between "unrelated" and "clearly the same thing" for the model in use, lifted a little by
    /// shared keywords. 0.8 and above is a strong match.
    pub related: f64,
    pub cosine: f64,
    pub bm25: f64,
}

impl Hit {
    /// How much the memory matters in its own right (0 to 1), apart from this query.
    pub fn importance(&self) -> f64 {
        (self.memory.importance * self.memory.weight).clamp(0.0, 1.0)
    }
}

pub struct Query<'a> {
    pub text: &'a str,
    pub project: Option<&'a str>,
    pub now_ms: i64,
    pub filter: Filter,
    pub limit: usize,
}

pub fn tokens(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split(|c: char| !c.is_alphanumeric()).map(str::to_lowercase).filter(|word| word.chars().count() >= 2 && !STOPWORDS.contains(&word.as_str())).filter(|word| seen.insert(word.clone())).collect()
}

fn fts_query(text: &str) -> String {
    tokens(text).iter().take(24).map(|token| format!("\"{}\"", token.replace('"', ""))).collect::<Vec<_>>().join(" OR ")
}

/// Which kinds of memory the wording of the question points at.
fn intent_kinds(query: &str) -> Vec<&'static str> {
    let q = format!(" {} ", query.to_lowercase());
    let has = |words: &[&str]| words.iter().any(|word| q.contains(&format!(" {word}")));
    let mut kinds = Vec::new();
    if has(&["why", "decided", "decision", "chose", "choose", "settled"]) {
        kinds.extend(["decision", "summary"]);
    }
    if has(&["fix", "fixed", "failed", "failure", "broke", "broken", "error", "crash", "last time", "again"]) {
        kinds.extend(["failure", "workflow", "outcome"]);
    }
    if has(&["prefer", "preference", "style", "usually", "always", "never", "like"]) {
        kinds.extend(["preference", "instruction"]);
    }
    if has(&["todo", "unfinished", "continue", "left", "remaining", "next", "pending", "where were"]) {
        kinds.extend(["task", "summary"]);
    }
    if has(&["did", "done", "ran", "installed", "changed", "built", "opened", "yesterday", "earlier", "recently"]) {
        kinds.extend(["outcome", "summary"]);
    }
    kinds
}

/// `[start, end)` in ms for the time phrases in a question ("yesterday", "last week", "3 days ago").
pub fn time_windows(query: &str, now_ms: i64) -> Vec<(i64, i64)> {
    let q = query.to_lowercase();
    let today = now_ms - now_ms.rem_euclid(DAY_MS);
    let mut windows = Vec::new();
    if q.contains("yesterday") {
        windows.push((today - DAY_MS, today));
    }
    if q.contains("today") || q.contains("this morning") {
        windows.push((today, today + DAY_MS));
    }
    if q.contains("last week") {
        windows.push((today - 14 * DAY_MS, today - 6 * DAY_MS));
    }
    if q.contains("this week") {
        windows.push((today - 6 * DAY_MS, today + DAY_MS));
    }
    if q.contains("last month") {
        windows.push((today - 62 * DAY_MS, today - 28 * DAY_MS));
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    for pair in words.windows(3) {
        if let Ok(days) = pair[0].parse::<i64>() {
            if pair[1].starts_with("day") && pair[2].starts_with("ago") {
                windows.push((today - (days + 1) * DAY_MS, today - (days - 1) * DAY_MS));
            }
        }
    }
    windows
}

pub fn recall(store: &Store, embedder: &dyn Embedder, query: &Query<'_>) -> rusqlite::Result<Vec<Hit>> {
    let candidates = 40;
    let mut bm25: HashMap<i64, f64> = HashMap::new();
    let mut cosine: HashMap<i64, f64> = HashMap::new();
    for (id, score) in store.fts_search(&fts_query(query.text), &query.filter, candidates)? {
        bm25.insert(id, score);
    }
    if !tokens(query.text).is_empty() {
        let vector = embedder.embed(query.text);
        for (id, similarity) in store.vector_search(&vector, embedder.name(), &query.filter, candidates)? {
            cosine.insert(id, f64::from(similarity));
        }
    }
    let ids: Vec<i64> = bm25.keys().chain(cosine.keys()).copied().collect::<HashSet<_>>().into_iter().collect();
    let max_bm25 = bm25.values().cloned().fold(0.0, f64::max);
    // How far apart unrelated and matching texts sit differs between embedders; each model rescales
    // from its own calibration.
    let (floor, ceiling) = embedder.calibration();
    let (floor, ceiling) = (f64::from(floor), f64::from(ceiling));
    let wanted_kinds = intent_kinds(query.text);
    let windows = time_windows(query.text, query.now_ms);
    let mut hits = Vec::new();
    for memory in store.get_many(&ids)? {
        let raw = cosine.get(&memory.id).copied();
        let c = raw.map(|raw| ((raw - floor) / (ceiling - floor)).clamp(0.0, 1.0)).unwrap_or(0.0);
        let b = if max_bm25 > 0.0 { bm25.get(&memory.id).copied().unwrap_or(0.0) / max_bm25 } else { 0.0 };
        // Shared keywords can lift a semantic match but never lower it; without a vector a keyword hit counts for a little less.
        let related = if raw.is_some() { (c + 0.25 * b * (1.0 - c)).min(1.0) } else { b * 0.8 };
        if related < MIN_RELATED {
            continue;
        }
        let age_days = ((query.now_ms - memory.updated_at.max(memory.created_at)) as f64 / DAY_MS as f64).max(0.0);
        let mut score = related + RECENCY_WEIGHT * (-RECENCY_LAMBDA * age_days).exp();
        score += memory.importance * 0.10 + if memory.verified { 0.04 } else { 0.0 } + ((memory.weight - 1.0) * 0.05).clamp(-0.05, 0.10);
        if wanted_kinds.contains(&memory.kind.as_str()) {
            score += INTENT_WEIGHT;
        }
        if windows.iter().any(|(start, end)| memory.created_at >= *start && memory.created_at < *end) {
            score += DATE_WEIGHT;
        }
        score += match (memory.scope.as_str(), query.project) {
            ("global", _) => 0.03,
            (scope, Some(project)) if scope == format!("project:{project}") => 0.10,
            _ => -0.05,
        };
        hits.push(Hit { memory, score, related, cosine: c, bm25: b });
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    // Several memories of the same kind that say the same thing crowd out everything else.
    let mut kept: Vec<Hit> = Vec::new();
    for hit in hits {
        let words: HashSet<String> = tokens(&hit.memory.text).into_iter().collect();
        let repeats = kept.iter().any(|other| {
            let other_words: HashSet<String> = tokens(&other.memory.text).into_iter().collect();
            let shared = words.intersection(&other_words).count();
            shared * 10 >= words.len().max(other_words.len()).max(1) * 8
        });
        if !repeats {
            kept.push(hit);
        }
        if kept.len() >= query.limit {
            break;
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::super::embed::Hashed;
    use super::super::store::{NewMemory, now_ms};
    use super::*;

    fn put(store: &mut Store, kind: &str, scope: &str, text: &str) -> i64 {
        let vector = Hashed.embed(text);
        store.insert(&NewMemory::new(kind, scope, text, "tool"), Some((Hashed.name(), &vector))).unwrap().id()
    }

    fn ask(store: &Store, text: &str, project: Option<&str>) -> Vec<Hit> {
        recall(store, &Hashed, &Query { text, project, now_ms: now_ms(), filter: Filter::default(), limit: 5 }).unwrap()
    }

    #[test]
    fn the_memory_about_the_question_comes_first() {
        let mut store = Store::in_memory().unwrap();
        put(&mut store, "preference", "global", "I prefer oat milk in my coffee");
        let build = put(&mut store, "failure", "global", "`cargo build --release` failed (exit 1): linker link.exe not found");
        put(&mut store, "outcome", "global", "Opened the app Discord");
        let hits = ask(&store, "why did the cargo build fail?", None);
        assert_eq!(hits[0].memory.id, build);
        assert!(ask(&store, "something completely unrelated about gardening tomatoes", None).is_empty(), "no match means no memory, not a random one");
    }

    #[test]
    fn the_current_project_outranks_another_one() {
        let mut store = Store::in_memory().unwrap();
        put(&mut store, "outcome", "project:other", "Ran `npm test` successfully for the release build");
        let mine = put(&mut store, "outcome", "project:rhea", "Ran `cargo test` successfully for the release build");
        let hits = ask(&store, "release build test run", Some("rhea"));
        assert_eq!(hits[0].memory.id, mine);
    }

    #[test]
    fn question_wording_points_at_the_kind_of_memory() {
        assert!(intent_kinds("why did we choose sqlite").contains(&"decision"));
        assert!(intent_kinds("what failed last time").contains(&"failure"));
        assert!(intent_kinds("what do I prefer").contains(&"preference"));
        assert!(intent_kinds("what is left to do").contains(&"task"));
        assert!(intent_kinds("hello").is_empty());
    }

    #[test]
    fn time_phrases_become_windows() {
        let now = 1_800_000_000_000i64;
        let yesterday = time_windows("what did we do yesterday", now);
        assert_eq!(yesterday.len(), 1);
        assert_eq!(yesterday[0].1 - yesterday[0].0, DAY_MS);
        assert!(yesterday[0].1 <= now);
        assert_eq!(time_windows("3 days ago we fixed it", now).len(), 1);
        assert!(time_windows("hello", now).is_empty());
    }

    #[test]
    fn near_identical_memories_do_not_crowd_the_results() {
        let mut store = Store::in_memory().unwrap();
        put(&mut store, "outcome", "global", "Ran `cargo test` successfully in the release folder today");
        put(&mut store, "summary", "global", "Ran `cargo test` successfully in the release folder today again");
        put(&mut store, "failure", "global", "`cargo test` failed (exit 101): a test panicked in the release folder");
        let hits = ask(&store, "cargo test release folder", None);
        assert_eq!(hits.len(), 2, "{:?}", hits.iter().map(|h| &h.memory.text).collect::<Vec<_>>());
    }

    #[test]
    fn the_query_becomes_a_safe_full_text_expression() {
        assert_eq!(fts_query("How do I fix \"the\" build?"), "\"fix\" OR \"build\"");
        assert_eq!(fts_query("???"), "");
    }
}
