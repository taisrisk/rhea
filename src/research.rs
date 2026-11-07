//! What counts as proof for a research request, and what Rhea may say when it is missing.
//!
//! Every successful search/fetch/extract result is turned into `Evidence` (the tool, where it came
//! from, how many qualifying rows it held). The same page fetched twice is one source. The
//! completion gate reads the log, and when the evidence falls short the retry prompt and the final
//! blocker say exactly what was and was not verified.

use std::collections::HashSet;

use serde_json::Value;

use crate::chat::compact_agent_output;

pub(crate) fn requires_research_evidence(request: &str) -> bool {
    let text = request.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| text.contains(word));
    let web = has(&[
        "online", "website", "web ", "web.", "internet", "google", "http", "www.", ".com", "news", "reddit", "roblox", "youtube",
        "wikipedia", "price", "weather", "players", "games", "rating", "review", "trending", "rolimons", "stock", "store",
    ]);
    let local = has(&["folder", "file", "repo", "project", "directory", "src", "code", "codebase", "function", "cargo", "subagent"]);
    if local && !web {
        return false;
    }
    let strong = has(&[
        "research", "search", "look up", "look for", "find games", "find me", "browse", "sources", "across sites", "statistics",
        "active players", "player count",
    ]);
    let weak = has(&[
        "compare", "summarize", "summary", "current", "latest", "more than", "at least", "over ", "top ", "1k", "1,000",
    ]);
    strong || (weak && web)
}

/// Evidence must come from a successful browser/search/extraction tool and
/// contain substantive returned data. A plan status, navigation result, or
/// assistant promise is never research evidence.
pub(crate) fn research_evidence_rows_from_tool(name: &str, result: &Value, request: &str) -> usize {
    research_evidence_keys(name, result, request).len()
}

/// The distinct pieces of evidence a result holds, each as a key for the record it is about. A
/// metric request ("more than 1K players") yields one key per qualifying record (its name and
/// value), so the same game appearing in two different pages or serializations is one record; any
/// other research result is one record, keyed by the words it contains.
pub(crate) fn research_evidence_keys(name: &str, result: &Value, request: &str) -> Vec<u64> {
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let name = name.to_ascii_lowercase();
    let web_or_search_tool =
        (name.starts_with("browser__") || name.starts_with("web_") || name.starts_with("mcp__"))
            && ["search", "extract", "fetch", "query"]
                .iter()
                .any(|kind| name.contains(kind));
    if !web_or_search_tool {
        return Vec::new();
    }
    let payload = compact_agent_output(result).to_string();
    let normalized = payload.to_ascii_lowercase();
    let useful_chars = payload.chars().filter(|ch| ch.is_alphanumeric()).count();
    if useful_chars < 32
        || ["no results", "0 results", "no matches", "empty result"]
            .iter()
            .any(|text| normalized.contains(text))
    {
        return Vec::new();
    }
    let request = request.to_ascii_lowercase();
    let metric_required = [
        "active player",
        "player count",
        "more than",
        "at least",
        "1k",
        "1,000",
        "over ",
    ]
    .iter()
    .any(|phrase| request.contains(phrase));
    if metric_required {
        let Some(threshold) = requested_threshold(&request) else {
            return Vec::new();
        };
        return qualifying_metric_rows(&payload, threshold, &request);
    }
    vec![evidence_fingerprint(result)]
}

/// Identifies a tool result by its content, so repeating a call does not count as new evidence.
pub(crate) fn evidence_fingerprint(result: &Value) -> u64 {
    // The set of words, so the same content arriving in another order, spacing or JSON shape is
    // still the same evidence.
    let payload = compact_agent_output(result).to_string().to_lowercase();
    let words: std::collections::BTreeSet<&str> = payload.split(|ch: char| !ch.is_alphanumeric()).filter(|word| !word.is_empty()).collect();
    hash_of(&words)
}

fn hash_of(value: &impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// How many distinct pieces of evidence the request actually asked for. "Find 10 games" means 10,
/// not however many happen to qualify; a plural noun with no number attached still means "more than
/// one," which was the old floor and stays as a fallback.
pub(crate) fn required_research_evidence_rows(request: &str) -> usize {
    let text = request.to_ascii_lowercase();
    let words: Vec<&str> = text.split_whitespace().collect();
    for (index, word) in words.iter().enumerate() {
        let clean = word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
        if !matches!(clean, "games" | "options" | "items" | "sources" | "results") {
            continue;
        }
        // A count up to a few words back ("find me 10 games", "the top 5 options") is the real
        // target; anything further back is unrelated and falls back to the "more than one" floor.
        let start = index.saturating_sub(3);
        if let Some(count) = words[start..index].iter().rev().find_map(|token| parse_threshold_token(token)) {
            if count >= 1.0 && count.is_finite() {
                return count as usize;
            }
        }
        return 2;
    }
    1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResearchCompletion {
    Accept,
    Retry,
    Block,
}

pub(crate) fn research_completion_gate(
    research_task: bool,
    evidence_rows: usize,
    required_rows: usize,
    retry_count: u8,
) -> ResearchCompletion {
    if !research_task || evidence_rows >= required_rows {
        return ResearchCompletion::Accept;
    }
    if retry_count == 0 {
        ResearchCompletion::Retry
    } else {
        ResearchCompletion::Block
    }
}

pub(crate) fn requested_threshold(request: &str) -> Option<f64> {
    let tokens: Vec<&str> = request.split_whitespace().collect();
    // Prefer a number attached to the requested comparison. A prompt such as
    // "find 10 games with more than 1K players" contains two numbers; using
    // the first one would silently weaken the actual player threshold to 10.
    let mut comparisons: Vec<(bool, usize, f64)> = Vec::new();
    for phrase in [
        "more than",
        "greater than",
        "at least",
        "over",
        "above",
        "no less than",
        "at most",
        "no more than",
        "less than",
        "under",
        "below",
    ] {
        for (position, _) in request.match_indices(phrase) {
            let suffix = &request[position + phrase.len()..];
            let suffix_tokens: Vec<&str> = suffix.split_whitespace().collect();
            if let Some((number_index, value)) = suffix_tokens
                .iter()
                .enumerate()
                .find_map(|(index, token)| parse_threshold_token(token).map(|value| (index, value)))
            {
                let metric_context =
                    suffix_tokens
                        .iter()
                        .skip(number_index + 1)
                        .take(5)
                        .any(|token| {
                            let token = token.to_ascii_lowercase();
                            [
                                "player", "playing", "active", "count", "user", "download",
                                "visit", "member",
                            ]
                            .iter()
                            .any(|metric| token.contains(metric))
                        });
                comparisons.push((metric_context, position, value));
            }
        }
    }
    if let Some((_, _, value)) = comparisons
        .iter()
        .filter(|(metric, _, _)| *metric)
        .max_by_key(|(_, position, _)| *position)
    {
        return Some(*value);
    }
    if comparisons.len() == 1 {
        return comparisons.first().map(|(_, _, value)| *value);
    }
    // If there is no comparison wording, use a sole numeric value. Ambiguous
    // multi-number requests remain blocked until the model gathers clearer evidence.
    let numbers: Vec<f64> = tokens
        .iter()
        .filter_map(|token| parse_threshold_token(token))
        .collect();
    if numbers.len() == 1 {
        numbers.first().copied()
    } else {
        None
    }
}

pub(crate) fn parse_threshold_token(token: &str) -> Option<f64> {
    let candidate =
        token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != ',');
    parse_compact_number(candidate)
}

pub(crate) fn parse_compact_number(value: &str) -> Option<f64> {
    let normalized = value.trim().trim_end_matches('+').replace(',', "");
    if normalized.is_empty() {
        return None;
    }
    let (digits, scale) = match normalized.chars().last()?.to_ascii_lowercase() {
        'k' => (&normalized[..normalized.len() - 1], 1_000.0),
        'm' => (&normalized[..normalized.len() - 1], 1_000_000.0),
        'b' => (&normalized[..normalized.len() - 1], 1_000_000_000.0),
        _ => (normalized.as_str(), 1.0),
    };
    let number = digits.parse::<f64>().ok()?;
    Some(number * scale)
}

pub(crate) fn qualifying_metric_rows(payload: &str, threshold: f64, request: &str) -> Vec<u64> {
    let words: Vec<&str> = payload.split_whitespace().collect();
    let lower: Vec<String> = words.iter().map(|word| word.to_ascii_lowercase()).collect();
    let strict_more = request.contains("more than")
        || request.contains("greater than")
        || request.contains("over ")
        || request.contains('>');
    let strict_less =
        request.contains("less than") || request.contains("under ") || request.contains('<');
    let at_most = request.contains("at most") || request.contains("no more than");
    let mut rows: Vec<u64> = Vec::new();
    for (index, token) in words.iter().enumerate() {
        let candidate =
            token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != ',');
        let Some(value) = parse_compact_number(candidate) else {
            continue;
        };
        let start = index.saturating_sub(4);
        let end = (index + 5).min(lower.len());
        let context = lower[start..end].join(" ");
        if !["player", "playing", "active", "count"]
            .iter()
            .any(|metric| context.contains(metric))
        {
            continue;
        }
        let meets = if strict_more {
            value > threshold
        } else if strict_less {
            value < threshold
        } else if at_most {
            value <= threshold
        } else {
            value >= threshold
        };
        if meets {
            // The record is its name (the nearest real word before the number) and its value. Layout,
            // punctuation, JSON keys and neighbouring records around it do not matter.
            let name: Vec<&str> = lower[index.saturating_sub(8)..index]
                .iter()
                .rev()
                .flat_map(|word| word.rsplit(|ch: char| !ch.is_alphanumeric()))
                .filter(|run| run.chars().any(char::is_alphabetic) && !METRIC_FILLER.contains(run))
                .take(1)
                .collect();
            let key = hash_of(&(name, value.to_bits()));
            if !rows.contains(&key) {
                rows.push(key);
            }
        }
    }
    rows
}

/// Words around a number that describe the metric, not the record.
const METRIC_FILLER: [&str; 20] = [
    "active", "player", "players", "playing", "count", "with", "has", "have", "had", "is", "are", "a", "the", "and", "of", "now", "right", "currently", "current", "concurrent",
];

/// One piece of proof: a successful search/fetch/extract result that held qualifying data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub tool: String,
    /// The site the data came from (its host), or the tool's name when the result names none.
    pub source: String,
    /// How many results in it meet the request (1 for a plain source, more for a metric table).
    pub rows: usize,
}

/// Everything a run has verified so far. The same result arriving twice counts once.
#[derive(Default)]
pub(crate) struct EvidenceLog {
    items: Vec<Evidence>,
    /// Records already counted: the same record in another page or format is not new evidence.
    seen: HashSet<u64>,
}

impl EvidenceLog {
    /// Record a tool result; returns the evidence it added (None when it proves nothing new).
    pub fn add(&mut self, tool: &str, result: &Value, request: &str) -> Option<&Evidence> {
        let fresh: Vec<u64> = research_evidence_keys(tool, result, request).into_iter().filter(|key| self.seen.insert(*key)).collect();
        let rows = fresh.len();
        if rows == 0 {
            return None;
        }
        self.items.push(Evidence { tool: tool.to_owned(), source: source_of(tool, result), rows });
        self.items.last()
    }

    pub fn rows(&self) -> usize {
        self.items.iter().map(|item| item.rows).sum()
    }

    /// Short list of what was verified, for the model's retry prompt and the final blocker.
    pub fn digest(&self) -> String {
        if self.items.is_empty() {
            return "nothing yet".into();
        }
        self.items.iter().map(|item| format!("{} ({} result{} via {})", item.source, item.rows, if item.rows == 1 { "" } else { "s" }, item.tool)).collect::<Vec<_>>().join("; ")
    }
}

/// The host of the first web address in a result, so the evidence says where it came from.
fn source_of(tool: &str, result: &Value) -> String {
    let text = result.to_string();
    let host = text.find("http").and_then(|at| {
        let rest = &text[at..];
        let rest = rest.strip_prefix("https://").or_else(|| rest.strip_prefix("http://"))?;
        let host: String = rest.chars().take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-')).collect();
        let host = host.trim_start_matches("www.").trim_end_matches('.').to_owned();
        (host.contains('.')).then_some(host)
    });
    host.unwrap_or_else(|| tool.to_owned())
}

/// The reply when the evidence never reached the requested amount: what was found, what was not.
pub(crate) fn research_blocker_reply(log: &EvidenceLog, required: usize) -> String {
    format!(
        "I couldn’t verify enough source results to complete the requested research criteria. I haven’t confirmed that the requested threshold or result count was met. Verified so far: {} of the {} needed ({}).",
        log.rows(),
        required,
        log.digest()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(host: &str, players: u32) -> Value {
        json!({"ok": true, "text": format!("https://{host}/games Game has {players} active players right now")})
    }

    #[test]
    fn evidence_remembers_where_it_came_from_and_counts_a_repeat_once() {
        let request = "find games with more than 1K active players";
        let mut log = EvidenceLog::default();
        assert_eq!(log.add("browser__extract", &page("www.rolimons.com", 5000), request).map(|e| e.source.clone()), Some("rolimons.com".into()));
        assert!(log.add("browser__extract", &page("www.rolimons.com", 5000), request).is_none(), "the same page twice");
        assert_eq!(log.rows(), 1);
        assert!(log.add("browser__extract", &page("roblox.com", 9000), request).is_some());
        assert_eq!(log.rows(), 2);
        assert!(log.digest().contains("rolimons.com") && log.digest().contains("roblox.com"));
    }

    #[test]
    fn the_same_records_in_another_format_are_not_new_evidence() {
        let request = "find games with more than 1K active players";
        let mut log = EvidenceLog::default();
        let table = json!({"ok": true, "text": "Adopt Me has 52,000 active players. Brookhaven has 31,000 active players."});
        assert_eq!(log.add("browser__extract", &table, request).map(|e| e.rows), Some(2));
        // The same two games, listed again in a different layout by another tool.
        let list = json!({"ok": true, "text": "1. Brookhaven - 31,000 active players 2. Adopt Me - 52,000 active players"});
        assert!(log.add("mcp__search__extract", &list, request).is_none(), "same records, different serialization");
        assert_eq!(log.rows(), 2);
        // A genuinely new game is new evidence.
        let more = json!({"ok": true, "text": "Blox Fruits has 44,000 active players."});
        assert_eq!(log.add("browser__extract", &more, request).map(|e| e.rows), Some(1));
        assert_eq!(log.rows(), 3);
    }

    #[test]
    fn required_rows_follow_the_count_actually_requested() {
        assert_eq!(required_research_evidence_rows("find me 10 games with more than 1K players"), 10, "the requested count, not the old flat floor of 2");
        assert_eq!(required_research_evidence_rows("list the top 5 options"), 5);
        assert_eq!(required_research_evidence_rows("find 3 sources for this"), 3);
        assert_eq!(required_research_evidence_rows("find games with more than 1K players"), 2, "a plural noun with no count still means more than one");
        assert_eq!(required_research_evidence_rows("what is the weather in Tokyo"), 1);
        assert_eq!(required_research_evidence_rows("give me 4 results"), 4);
    }

    #[test]
    fn results_that_prove_nothing_are_not_evidence() {
        let request = "find games with more than 1K active players";
        let mut log = EvidenceLog::default();
        assert!(log.add("browser__extract", &page("a.com", 12), request).is_none(), "below the threshold");
        assert!(log.add("jev_run_plan", &page("a.com", 5000), request).is_none(), "not a research tool");
        assert!(log.add("browser__extract", &json!({"ok": false, "error": "timeout"}), request).is_none());
        assert_eq!(log.digest(), "nothing yet");
    }

    #[test]
    fn a_result_without_an_address_names_its_tool() {
        let mut log = EvidenceLog::default();
        let result = json!({"ok": true, "text": "Rust 1.90 was released with new features and a long list of changes"});
        assert_eq!(log.add("mcp__search__query", &result, "research the latest rust news").map(|e| e.source.clone()), Some("mcp__search__query".into()));
    }

    #[test]
    fn the_blocker_says_what_was_verified() {
        let mut log = EvidenceLog::default();
        log.add("browser__extract", &page("rolimons.com", 5000), "find games with more than 1K active players");
        let reply = research_blocker_reply(&log, 2);
        assert!(reply.contains("1 of the 2 needed") && reply.contains("rolimons.com"), "{reply}");
        assert!(research_blocker_reply(&EvidenceLog::default(), 2).contains("nothing yet"));
    }
}
