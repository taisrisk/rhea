//! Settings > Usage > Recent runs: the run ledger of the open chat, newest first.

use super::*;

/// How long the list is reused before the ledger file is read again.
const REFRESH: Duration = Duration::from_secs(2);
const SHOWN: usize = 8;

impl RheaApp {
    fn recent_runs(&mut self, chat_id: &str) -> Vec<ledger::RunSummary> {
        let fresh = matches!(&self.recent_runs, Some((id, at, _)) if id == chat_id && at.elapsed() < REFRESH);
        if !fresh {
            let runs = ledger::summarize(&ledger::read(chat_id));
            self.recent_runs = Some((chat_id.to_owned(), Instant::now(), runs));
        }
        self.recent_runs.as_ref().map(|(_, _, runs)| runs.clone()).unwrap_or_default()
    }

    pub(super) fn recent_runs_ui(&mut self, ui: &mut egui::Ui) {
        let Some(chat_id) = self.conversations.get(self.active_conversation).map(|chat| chat.id.clone()) else { return };
        let runs = self.recent_runs(&chat_id);
        settings_group(ui, Some("Recent runs in this chat"), |ui| {
            if runs.is_empty() {
                settings_row(ui, Some(MUTED), "No runs yet", "Each message you send is recorded here: tools used, evidence found, how it ended.", |_| {});
            }
            for (position, run) in runs.iter().take(SHOWN).enumerate() {
                if position > 0 {
                    row_divider(ui);
                }
                let dot = match run.status.as_str() {
                    "completed" => HOVER_GREEN,
                    "failed" => DANGER,
                    _ => MUTED,
                };
                let title: String = run.request.chars().take(70).collect();
                let title = if run.agent == "main" { title } else { format!("{title} (agent)") };
                settings_row(ui, Some(dot), if title.is_empty() { "(no request)" } else { &title }, &run_detail(run), |_| {});
            }
        });
    }
}

/// One line: how it ended, what it did, how long it took.
fn run_detail(run: &ledger::RunSummary) -> String {
    let mut parts = vec![match run.status.as_str() {
        "interrupted" => "interrupted (no end recorded)".to_owned(),
        other => other.to_owned(),
    }];
    parts.push(format!("{} model round{}", run.rounds, if run.rounds == 1 { "" } else { "s" }));
    if run.tools > 0 {
        parts.push(if run.failed_tools > 0 { format!("{} tools ({} failed)", run.tools, run.failed_tools) } else { format!("{} tools", run.tools) });
    }
    if run.evidence_rows > 0 {
        parts.push(format!("{} evidence", run.evidence_rows));
    }
    if run.gate_retries > 0 {
        parts.push("sent back for more evidence".into());
    }
    if run.ms > 0 {
        parts.push(format!("{:.1} s", run.ms as f64 / 1000.0));
    }
    if !run.error.is_empty() {
        parts.push(run.error.chars().take(80).collect());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> ledger::RunSummary {
        ledger::RunSummary {
            run: "r1".into(),
            agent: "main".into(),
            at: 0,
            request: "find games".into(),
            model: "m".into(),
            status: "completed".into(),
            error: String::new(),
            ms: 12_400,
            rounds: 3,
            tools: 4,
            failed_tools: 1,
            evidence_rows: 2,
            gate_retries: 1,
        }
    }

    #[test]
    fn a_run_reads_as_one_line() {
        assert_eq!(run_detail(&run()), "completed · 3 model rounds · 4 tools (1 failed) · 2 evidence · sent back for more evidence · 12.4 s");
    }

    #[test]
    fn an_interrupted_run_says_so() {
        let mut interrupted = run();
        interrupted.status = "interrupted".into();
        interrupted.ms = 0;
        assert!(run_detail(&interrupted).starts_with("interrupted (no end recorded)"));
    }
}
