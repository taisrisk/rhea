//! The run ledger: an append-only record of what each run did, kept in the chat's own folder as
//! `ledger.jsonl` (so archiving or deleting a chat takes it along).
//!
//! One line per event: the request, every model round, every tool call with its outcome and time,
//! each piece of research evidence, the completion gate's decisions, and how the run ended. It
//! answers "what did it actually do?" without re-reading the transcript, and a run that has a
//! start but no end shows up as interrupted.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const FILE: &str = "ledger.jsonl";
/// The file is cut back to its newest half once it grows past this.
const MAX_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Start { model: String, request: String },
    Model { ms: u64, tokens: u64, calls: usize },
    Tool { name: String, ok: bool, ms: u64 },
    Evidence { tool: String, source: String, rows: usize, total: usize },
    Gate { decision: String, have: usize, need: usize },
    End { status: String, ms: u64, answer_chars: usize, error: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub at: u64,
    pub run: String,
    pub agent: String,
    #[serde(flatten)]
    pub event: Event,
}

/// Records one run. `Ledger::off()` does nothing, so callers never check whether it is enabled.
pub struct Ledger {
    path: Option<PathBuf>,
    run: String,
    agent: String,
    started: Instant,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Ledger {
    pub fn off() -> Self {
        Self { path: None, run: String::new(), agent: String::new(), started: Instant::now() }
    }

    /// A ledger for one run of `agent` ("main" or a subagent's id) in the chat `chat_id`.
    pub fn for_chat(chat_id: &str, agent: &str) -> Self {
        Self::at(crate::chat_store::chat_dir(chat_id).map(|dir| dir.join(FILE)), agent)
    }

    pub(crate) fn at(path: Option<PathBuf>, agent: &str) -> Self {
        if let Some(path) = path.as_ref() {
            trim_if_large(path);
        }
        Self { path, run: format!("r{}", now_ms()), agent: agent.to_owned(), started: Instant::now() }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn record(&self, event: Event) {
        let Some(path) = self.path.as_ref() else { return };
        let entry = Entry { at: now_ms(), run: self.run.clone(), agent: self.agent.clone(), event };
        let Ok(mut line) = serde_json::to_string(&entry) else { return };
        line.push('\n');
        let written = path
            .parent()
            .map(fs::create_dir_all)
            .transpose()
            .and_then(|_| OpenOptions::new().create(true).append(true).open(path))
            .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(error) = written {
            // The ledger is a diagnostic aid, so a failure is shown once per run and never stops it.
            if entry.run == self.run && matches!(entry.event, Event::Start { .. }) {
                crate::chat_store::report_save_error("this chat's run ledger", &error);
            }
        }
    }
}

/// Keep the newest half, starting at a whole line.
fn trim_if_large(path: &PathBuf) {
    let Ok(meta) = fs::metadata(path) else { return };
    if meta.len() <= MAX_BYTES {
        return;
    }
    let Ok(bytes) = fs::read(path) else { return };
    let cut = bytes.len() / 2;
    let start = bytes[cut..].iter().position(|b| *b == b'\n').map(|at| cut + at + 1).unwrap_or(bytes.len());
    let _ = crate::chat_store::write_bytes_atomic(path, &bytes[start..]);
}

/// Every readable entry of a chat's ledger, oldest first. A damaged line is skipped.
pub fn read(chat_id: &str) -> Vec<Entry> {
    let Some(path) = crate::chat_store::chat_dir(chat_id).map(|dir| dir.join(FILE)) else { return Vec::new() };
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

/// One run, as listed in Settings.
#[derive(Clone, Debug, PartialEq)]
pub struct RunSummary {
    pub run: String,
    pub agent: String,
    pub at: u64,
    pub request: String,
    pub model: String,
    /// `completed`, `failed`, `cancelled`, or `interrupted` (started, never ended).
    pub status: String,
    pub error: String,
    pub ms: u64,
    pub rounds: usize,
    pub tools: usize,
    pub failed_tools: usize,
    pub evidence_rows: usize,
    pub gate_retries: usize,
}

/// Fold the entries into one summary per run, newest first.
pub fn summarize(entries: &[Entry]) -> Vec<RunSummary> {
    let mut runs: Vec<RunSummary> = Vec::new();
    for entry in entries {
        let at = runs.iter().position(|run| run.run == entry.run && run.agent == entry.agent);
        let run = match at {
            Some(at) => &mut runs[at],
            None => {
                runs.push(RunSummary {
                    run: entry.run.clone(),
                    agent: entry.agent.clone(),
                    at: entry.at,
                    request: String::new(),
                    model: String::new(),
                    status: "interrupted".into(),
                    error: String::new(),
                    ms: 0,
                    rounds: 0,
                    tools: 0,
                    failed_tools: 0,
                    evidence_rows: 0,
                    gate_retries: 0,
                });
                runs.last_mut().expect("just pushed")
            }
        };
        match &entry.event {
            Event::Start { model, request } => {
                run.model = model.clone();
                run.request = request.clone();
            }
            Event::Model { .. } => run.rounds += 1,
            Event::Tool { ok, .. } => {
                run.tools += 1;
                run.failed_tools += usize::from(!ok);
            }
            Event::Evidence { rows, .. } => run.evidence_rows += rows,
            Event::Gate { decision, .. } => run.gate_retries += usize::from(decision == "retry"),
            Event::End { status, ms, error, .. } => {
                run.status = status.clone();
                run.ms = *ms;
                run.error = error.clone();
            }
        }
    }
    runs.reverse();
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rhea-ledger-{name}-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(FILE)
    }

    fn lines(path: &PathBuf) -> Vec<Entry> {
        fs::read_to_string(path).unwrap().lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    #[test]
    fn events_are_appended_one_line_each_and_read_back() {
        let path = temp("append");
        let ledger = Ledger::at(Some(path.clone()), "main");
        ledger.record(Event::Start { model: "gpt".into(), request: "find games".into() });
        ledger.record(Event::Tool { name: "browser__extract".into(), ok: true, ms: 120 });
        ledger.record(Event::End { status: "completed".into(), ms: 900, answer_chars: 40, error: String::new() });
        let entries = lines(&path);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].event, Event::Tool { name: "browser__extract".into(), ok: true, ms: 120 });
        assert!(fs::read_to_string(&path).unwrap().lines().all(|line| line.starts_with('{')));
    }

    #[test]
    fn a_run_is_summarized_with_its_tools_evidence_and_outcome() {
        let path = temp("summary");
        let ledger = Ledger::at(Some(path.clone()), "main");
        ledger.record(Event::Start { model: "gpt".into(), request: "research".into() });
        ledger.record(Event::Model { ms: 800, tokens: 1200, calls: 2 });
        ledger.record(Event::Tool { name: "browser__search".into(), ok: true, ms: 50 });
        ledger.record(Event::Tool { name: "browser__extract".into(), ok: false, ms: 70 });
        ledger.record(Event::Evidence { tool: "browser__search".into(), source: "a.com".into(), rows: 2, total: 2 });
        ledger.record(Event::Gate { decision: "retry".into(), have: 2, need: 3 });
        ledger.record(Event::End { status: "completed".into(), ms: 2000, answer_chars: 10, error: String::new() });
        let runs = summarize(&lines(&path));
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!((run.rounds, run.tools, run.failed_tools, run.evidence_rows, run.gate_retries), (1, 2, 1, 2, 1));
        assert_eq!((run.status.as_str(), run.ms, run.request.as_str()), ("completed", 2000, "research"));
    }

    #[test]
    fn a_run_that_never_ended_is_interrupted_and_newest_runs_come_first() {
        let mut entries = Vec::new();
        for (run, ended) in [("r1", true), ("r2", false)] {
            entries.push(Entry { at: 1, run: run.into(), agent: "main".into(), event: Event::Start { model: "m".into(), request: run.into() } });
            if ended {
                entries.push(Entry { at: 2, run: run.into(), agent: "main".into(), event: Event::End { status: "failed".into(), ms: 5, answer_chars: 0, error: "boom".into() } });
            }
        }
        let runs = summarize(&entries);
        assert_eq!(runs[0].run, "r2");
        assert_eq!(runs[0].status, "interrupted");
        assert_eq!((runs[1].status.as_str(), runs[1].error.as_str()), ("failed", "boom"));
    }

    #[test]
    fn a_damaged_line_is_skipped_and_a_large_file_is_cut_to_whole_lines() {
        let path = temp("trim");
        let ledger = Ledger::at(Some(path.clone()), "main");
        ledger.record(Event::Start { model: "m".into(), request: "x".into() });
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{not json}\n");
        fs::write(&path, &text).unwrap();
        assert_eq!(lines(&path).len(), 1);

        let line = text.lines().next().unwrap().to_owned();
        let big = std::iter::repeat_n(format!("{line}\n"), (MAX_BYTES as usize / line.len()) + 50).collect::<String>();
        fs::write(&path, &big).unwrap();
        let _ = Ledger::at(Some(path.clone()), "main");
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.len() < big.len() && after.len() > 0);
        assert!(after.lines().all(|l| serde_json::from_str::<Entry>(l).is_ok()), "every kept line is whole");
    }

    #[test]
    fn an_off_ledger_writes_nothing() {
        Ledger::off().record(Event::Start { model: "m".into(), request: "x".into() });
    }
}
