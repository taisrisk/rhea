//! Spell checking and autocorrect for the chat box.
//!
//! The dictionary is Hunspell's en_US (SCOWL, see `assets/dictionaries/LICENSE-en_US.txt`) read by
//! the pure-Rust `spellbook` crate, so nothing leaves the computer and no system spell checker is
//! needed (that is what makes this work the same on Windows and macOS).
//!
//! What is learned from the user lives in `~/.rhea/spelling.json`: words they added, and how
//! often they undid an autocorrection. A word whose autocorrection was undone three times is never
//! autocorrected again.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ops::Range,
    sync::{Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};

use crate::chat_store;

const AFF: &str = include_str!("../assets/dictionaries/en_US.aff");
const DIC: &str = include_str!("../assets/dictionaries/en_US.dic");

/// How many times a user must undo the autocorrection of a word before it stops being corrected.
pub const NEVER_AFTER: u8 = 3;
const MAX_SUGGESTIONS: usize = 6;

fn dictionary_cell() -> &'static OnceLock<spellbook::Dictionary> {
    static DICTIONARY: OnceLock<spellbook::Dictionary> = OnceLock::new();
    &DICTIONARY
}

/// Load the dictionary in the background so the first keystroke does not wait for it. Until it is
/// ready every word counts as correct.
pub fn warm_up() {
    std::thread::spawn(|| {
        let _ = dictionary_cell().get_or_init(|| spellbook::Dictionary::new(AFF, DIC).expect("the bundled dictionary is valid"));
    });
}

fn checked(word: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let Some(dictionary) = dictionary_cell().get() else { return true };
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = cache.lock().unwrap_or_else(|p| p.into_inner()).get(word) {
        return *known;
    }
    let lower = word.to_lowercase();
    let ok = dictionary.check(word) || dictionary.check(&lower);
    let mut cache = cache.lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() > 20_000 {
        cache.clear();
    }
    cache.insert(word.to_owned(), ok);
    ok
}

/// A checkable word in the text: where it is (in characters) and what it says.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub range: Range<usize>,
    pub text: String,
}

fn is_edge_punctuation(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '“' | '”' | '‘' | '*' | '_')
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '’'
}

/// Every word worth checking. Anything that is not plain prose is left alone: slash commands,
/// code in backticks, links, paths, addresses, identifiers, numbers, acronyms and camelCase.
pub fn words(text: &str) -> Vec<Word> {
    let chars: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        if chars[start].is_whitespace() {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < chars.len() && !chars[end].is_whitespace() {
            end += 1;
        }
        chunk_words(&chars, start, end, &mut found);
        start = end;
    }
    found
}

fn chunk_words(chars: &[char], start: usize, end: usize, out: &mut Vec<Word>) {
    let chunk = &chars[start..end];
    if chunk.first() == Some(&'/') || chunk.contains(&'`') {
        return;
    }
    let mut lo = 0;
    let mut hi = chunk.len();
    while lo < hi && is_edge_punctuation(chunk[lo]) {
        lo += 1;
    }
    while hi > lo && is_edge_punctuation(chunk[hi - 1]) {
        hi -= 1;
    }
    let core = &chunk[lo..hi];
    if core.is_empty() || !core.iter().all(|c| c.is_alphabetic() || is_apostrophe(*c) || *c == '-') {
        return;
    }
    // Hyphenated compounds are checked part by part.
    let mut part_start = 0;
    for index in 0..=core.len() {
        if index == core.len() || core[index] == '-' {
            if index > part_start {
                push_word(&core[part_start..index], start + lo + part_start, out);
            }
            part_start = index + 1;
        }
    }
}

fn push_word(part: &[char], at: usize, out: &mut Vec<Word>) {
    let trimmed_start = part.iter().take_while(|c| is_apostrophe(**c)).count();
    let trimmed_end = part.iter().rev().take_while(|c| is_apostrophe(**c)).count();
    if trimmed_start + trimmed_end >= part.len() {
        return;
    }
    let part = &part[trimmed_start..part.len() - trimmed_end];
    let at = at + trimmed_start;
    let letters = part.iter().filter(|c| c.is_alphabetic()).count();
    let acronym = letters >= 2 && part.iter().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase());
    let inner_capital = part.iter().skip(1).any(|c| c.is_uppercase()) && !acronym;
    if letters < 2 || acronym || inner_capital {
        return;
    }
    out.push(Word { range: at..at + part.len(), text: part.iter().collect() });
}

fn normalized(word: &str) -> String {
    word.replace('’', "'")
}

/// Misspelled-word suggestions, best first, in the case the user typed.
pub fn suggestions(word: &str) -> Vec<String> {
    let Some(dictionary) = dictionary_cell().get() else { return Vec::new() };
    let mut raw = Vec::new();
    dictionary.suggest(&normalized(word), &mut raw);
    let mut seen = HashSet::new();
    raw.into_iter().map(|suggestion| match_case(word, &suggestion)).filter(|suggestion| seen.insert(suggestion.clone())).take(MAX_SUGGESTIONS).collect()
}

/// Give a suggestion the capitalization of the word it replaces.
pub fn match_case(original: &str, suggestion: &str) -> String {
    let letters: Vec<char> = original.chars().filter(|c| c.is_alphabetic()).collect();
    let all_caps = letters.len() > 1 && letters.iter().all(|c| c.is_uppercase());
    let capitalized = letters.first().is_some_and(|c| c.is_uppercase());
    if all_caps {
        return suggestion.to_uppercase();
    }
    if capitalized {
        let mut chars = suggestion.chars();
        return chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default();
    }
    suggestion.to_owned()
}

/// What the user has taught the checker.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Learned {
    /// Words added to the dictionary (lowercase).
    #[serde(default)]
    pub added: BTreeSet<String>,
    /// Misspelling (lowercase) to how many times its autocorrection was undone.
    #[serde(default)]
    pub reverted: BTreeMap<String, u8>,
    /// Only what was loaded from disk is written back (tests build their own and stay in memory).
    #[serde(skip)]
    persist: bool,
}

fn learned_path() -> Option<std::path::PathBuf> {
    chat_store::app_data_root().map(|root| root.join("spelling.json"))
}

impl Learned {
    pub fn load() -> Self {
        let mut learned: Self = learned_path().and_then(|path| std::fs::read(path).ok()).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        learned.persist = true;
        learned
    }

    fn save(&self) {
        if !self.persist {
            return;
        }
        if let Some(path) = learned_path() {
            let _ = chat_store::write_json_atomic(&path, self);
        }
    }
}

/// How a word is shown in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Spelled correctly: no underline.
    None,
    /// Not in the dictionary: red underline.
    Misspelled,
    /// Not in the dictionary, but the user kept it after Rhea corrected it: blue underline.
    Intended,
}

/// The checker for one chat box: dictionary lookups plus what happened this session.
#[derive(Debug, Default)]
pub struct Speller {
    pub learned: Learned,
    pub autocorrect: bool,
    /// Misspellings (lowercase) the user insisted on in the message being written.
    intended: HashSet<String>,
    /// Corrections made in the message being written: misspelling (lowercase) to its correction.
    corrected: HashMap<String, String>,
}

impl Speller {
    pub fn new(autocorrect: bool) -> Self {
        Self { learned: Learned::load(), autocorrect, ..Default::default() }
    }

    fn is_known(&self, word: &str) -> bool {
        let normal = normalized(word);
        checked(&normal) || self.learned.added.contains(&normal.to_lowercase())
    }

    fn never_corrected(&self, key: &str) -> bool {
        self.learned.reverted.get(key).is_some_and(|count| *count >= NEVER_AFTER)
    }

    pub fn mark(&self, word: &str) -> Mark {
        if self.is_known(word) {
            return Mark::None;
        }
        let key = normalized(word).to_lowercase();
        if self.intended.contains(&key) || self.never_corrected(&key) {
            Mark::Intended
        } else {
            Mark::Misspelled
        }
    }

    /// The user finished typing `word` (a space or punctuation followed it). Returns what should
    /// replace it, when autocorrect applies.
    pub fn word_finished(&mut self, word: &str) -> Option<String> {
        if self.is_known(word) {
            return None;
        }
        let key = normalized(word).to_lowercase();
        if self.corrected.contains_key(&key) {
            // Rhea corrected this exact spelling earlier in this message and the user wrote it
            // again: they deleted the correction and mean it.
            self.keep_as_written(&key);
            return None;
        }
        if !self.autocorrect || self.intended.contains(&key) || self.never_corrected(&key) {
            return None;
        }
        let fixed = suggestions(word).into_iter().next()?;
        self.corrected.insert(key, fixed.to_lowercase());
        Some(fixed)
    }

    /// The user undid an autocorrection (by retyping the word or choosing "Change back"): keep the
    /// word as written for the rest of the message and remember it; after `NEVER_AFTER` times it
    /// is never autocorrected again.
    pub fn keep_as_written(&mut self, key: &str) {
        self.intended.insert(key.to_owned());
        let count = self.learned.reverted.entry(key.to_owned()).or_insert(0);
        *count = count.saturating_add(1);
        self.learned.save();
    }

    /// The misspelling `fixed` was produced from in this message, if Rhea corrected it.
    pub fn original_of(&self, fixed: &str) -> Option<String> {
        let fixed = normalized(fixed).to_lowercase();
        self.corrected.iter().find(|(_, correction)| **correction == fixed).map(|(original, _)| original.clone())
    }

    pub fn add_to_dictionary(&mut self, word: &str) {
        self.learned.added.insert(normalized(word).to_lowercase());
        self.learned.save();
    }

    pub fn ignore_for_now(&mut self, word: &str) {
        self.intended.insert(normalized(word).to_lowercase());
    }

    /// A message was sent: what was decided about its words does not carry over to the next one.
    pub fn message_sent(&mut self) {
        self.intended.clear();
        self.corrected.clear();
    }
}

/// When `after` is `before` plus exactly one typed character (the cursor sits right behind it), and
/// that character ends a word, the word that was just finished.
pub fn finished_word(before: &str, after: &str, cursor: usize) -> Option<Word> {
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();
    if after_chars.len() != before_chars.len() + 1 || cursor == 0 || cursor > after_chars.len() {
        return None;
    }
    let typed = after_chars[cursor - 1];
    let mut without = after_chars.clone();
    without.remove(cursor - 1);
    if without != before_chars {
        return None;
    }
    if !(typed.is_whitespace() || matches!(typed, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '”')) {
        return None;
    }
    words(after).into_iter().find(|word| word.range.end == cursor - 1)
}

/// `text` with the characters in `range` replaced by `replacement`.
pub fn replace_range(text: &str, range: Range<usize>, replacement: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: String = chars[..range.start.min(chars.len())].iter().collect();
    out.push_str(replacement);
    out.extend(chars[range.end.min(chars.len())..].iter());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() {
        let _ = dictionary_cell().get_or_init(|| spellbook::Dictionary::new(AFF, DIC).expect("valid dictionary"));
    }

    fn texts(text: &str) -> Vec<String> {
        words(text).into_iter().map(|word| word.text).collect()
    }

    #[test]
    fn only_plain_prose_words_are_checked() {
        assert_eq!(texts("Hello, world! It's fine."), ["Hello", "world", "It's", "fine"]);
        assert_eq!(texts("/compact now"), ["now"]);
        assert_eq!(texts("run `cargo check` then look at src/main.rs and a@b.com"), ["run", "then", "look", "at", "and"]);
        assert_eq!(texts("NASA uses camelCase and snake_case 42 times"), ["uses", "and", "times"]);
        assert_eq!(texts("well-known state-of-the-art"), ["well", "known", "state", "of", "the", "art"]);
        assert_eq!(texts("a I x"), Vec::<String>::new());
    }

    #[test]
    fn word_ranges_are_character_offsets() {
        let found = words("héllo wörld, teh end");
        assert_eq!(found[2], Word { range: 13..16, text: "teh".into() });
        assert_eq!(replace_range("héllo wörld, teh end", 13..16, "the"), "héllo wörld, the end");
    }

    #[test]
    fn misspellings_are_found_and_suggestions_fix_them() {
        ready();
        let speller = Speller::new(true);
        assert_eq!(speller.mark("hello"), Mark::None);
        assert_eq!(speller.mark("Hello"), Mark::None);
        assert_eq!(speller.mark("don't"), Mark::None);
        assert_eq!(speller.mark("don’t"), Mark::None);
        assert_eq!(speller.mark("recieve"), Mark::Misspelled);
        assert_eq!(speller.mark("teh"), Mark::Misspelled);
        assert!(suggestions("recieve").contains(&"receive".to_owned()), "{:?}", suggestions("recieve"));
        assert_eq!(suggestions("teh").first().map(String::as_str), Some("the"));
    }

    #[test]
    fn corrections_keep_the_case_of_the_word() {
        assert_eq!(match_case("Teh", "the"), "The");
        assert_eq!(match_case("TEH", "the"), "THE");
        assert_eq!(match_case("teh", "the"), "the");
    }

    #[test]
    fn a_finished_word_needs_exactly_one_typed_delimiter() {
        let found = finished_word("this is teh", "this is teh ", 12).unwrap();
        assert_eq!(found.text, "teh");
        assert_eq!(finished_word("teh", "teh.", 4).unwrap().text, "teh");
        assert!(finished_word("this is te", "this is teh", 11).is_none(), "a letter does not finish a word");
        assert!(finished_word("teh", "teh  ", 5).is_none(), "more than one character changed");
        assert!(finished_word("teh x", "teh  x", 4).is_some(), "typing a space in the middle also ends the first word");
        assert!(finished_word("abc", "abx ", 4).is_none(), "not a pure insertion");
    }

    fn isolated() -> Speller {
        // never touches the real ~/.rhea: tests only use the in-memory parts of `Learned`
        Speller { learned: Learned::default(), autocorrect: true, ..Default::default() }
    }

    #[test]
    fn autocorrect_replaces_a_misspelling_with_the_first_suggestion_once() {
        ready();
        let mut speller = isolated();
        assert_eq!(speller.word_finished("teh").as_deref(), Some("the"));
        assert_eq!(speller.word_finished("Teh"), None, "a second time means the user wrote it again");
        assert_eq!(speller.word_finished("hello"), None);
        speller.autocorrect = false;
        assert_eq!(speller.word_finished("recieve"), None, "autocorrect off");
    }

    #[test]
    fn rewriting_a_corrected_word_turns_it_blue_and_stops_correcting_it() {
        ready();
        let mut speller = isolated();
        assert_eq!(speller.mark("zorbo"), Mark::Misspelled);
        let fixed = speller.word_finished("zorbo");
        assert!(fixed.is_some());
        // the user deletes the correction and writes their word again
        assert_eq!(speller.word_finished("zorbo"), None);
        assert_eq!(speller.mark("zorbo"), Mark::Intended);
        assert_eq!(speller.learned.reverted.get("zorbo"), Some(&1));
        speller.message_sent();
        assert_eq!(speller.mark("zorbo"), Mark::Misspelled, "a new message starts fresh");
    }

    #[test]
    fn after_three_undone_corrections_a_word_is_never_corrected() {
        ready();
        let mut speller = isolated();
        for time in 1..=NEVER_AFTER {
            assert!(speller.word_finished("zorbo").is_some(), "correction {time}");
            assert_eq!(speller.word_finished("zorbo"), None);
            speller.message_sent();
        }
        assert_eq!(speller.learned.reverted.get("zorbo"), Some(&NEVER_AFTER));
        assert_eq!(speller.word_finished("zorbo"), None, "no more corrections for it");
        assert_eq!(speller.mark("zorbo"), Mark::Intended, "and it shows as intended, not as an error");
    }

    #[test]
    fn corrected_words_can_be_traced_back_for_change_back() {
        ready();
        let mut speller = isolated();
        speller.word_finished("teh");
        assert_eq!(speller.original_of("the").as_deref(), Some("teh"));
        assert_eq!(speller.original_of("hello"), None);
    }
}
