//! The assistant side of the transcript: no bubble (the reply sits on the background beside the
//! Rhea mark), a compact "Worked for ..." section whose tool calls are grouped by kind
//! ("Ran 4 commands"), and read-only selectable boxes for each call's arguments and result.

use super::*;

/// How a tool call is grouped when several of the same kind run in a row.
struct Kind {
    key: &'static str,
    verb: &'static str,
    plural: &'static str,
}

fn tool_kind(name: &str) -> Kind {
    match name {
        "mcp__rhea_shell__run_command" => Kind { key: "command", verb: "Ran", plural: "commands" },
        "mcp__rhea_shell__read_file" => Kind { key: "read", verb: "Read", plural: "files" },
        "mcp__rhea_shell__write_file" | "mcp__rhea_shell__edit_file" | "mcp__rhea_shell__create_directory" => {
            Kind { key: "edit", verb: "Changed", plural: "files and folders" }
        }
        "mcp__rhea_shell__list_directory" => Kind { key: "list", verb: "Listed", plural: "folders" },
        "mcp__rhea_shell__web_fetch" => Kind { key: "fetch", verb: "Fetched", plural: "pages" },
        "jev_run_plan" | "open_app" => Kind { key: "desktop", verb: "Ran", plural: "desktop plans" },
        n if n.starts_with("desktop_") => Kind { key: "desktop", verb: "Used", plural: "desktop actions" },
        n if n.starts_with("browser__") => Kind { key: "chrome", verb: "Used", plural: "Chrome actions" },
        "message_received" => Kind { key: "message", verb: "Received", plural: "messages" },
        n if n.starts_with("agent_") => Kind { key: "agents", verb: "Made", plural: "agent calls" },
        n if n.starts_with("mcp__") => Kind { key: "mcp", verb: "Made", plural: "MCP calls" },
        _ => Kind { key: "other", verb: "Used", plural: "tools" },
    }
}

/// (one, many, bare) wording for a kind in the one-line summary of a whole turn: "Ran a command" /
/// "Ran 4 commands" / "ran commands".
fn summary_phrase(key: &str) -> (&'static str, &'static str, &'static str) {
    match key {
        "command" => ("Ran a command", "Ran {n} commands", "ran commands"),
        "edit" => ("Edited a file", "Edited {n} files", "edited files"),
        "read" => ("Read a file", "Read {n} files", "read files"),
        "list" => ("Listed a folder", "Listed {n} folders", "listed folders"),
        "fetch" => ("Fetched a page", "Fetched {n} pages", "searched the web"),
        "desktop" => ("Used the desktop once", "Used the desktop {n} times", "used the desktop"),
        "chrome" => ("Used Chrome once", "Used Chrome {n} times", "used Chrome"),
        "message" => ("Received a message", "Received {n} messages", "received messages"),
        "agents" => ("Managed an agent", "Managed agents {n} times", "managed agents"),
        "mcp" => ("Used an MCP tool", "Used MCP tools {n} times", "used MCP tools"),
        _ => ("Used a tool", "Used {n} tools", "used tools"),
    }
}

/// One line for everything a turn did, in the order it first did it: one kind keeps its count
/// ("Ran 4 commands"), two read "Ran 4 commands and edited 2 files", three or more are a plain
/// list ("Edited files, ran commands, searched the web").
fn turn_summary(activities: &[chat::ToolActivity]) -> String {
    let mut kinds: Vec<(&'static str, usize)> = Vec::new();
    for activity in activities {
        let key = tool_kind(&activity.name).key;
        match kinds.iter_mut().find(|(k, _)| *k == key) {
            Some((_, count)) => *count += 1,
            None => kinds.push((key, 1)),
        }
    }
    let counted = |key: &str, count: usize| {
        let (one, many, _) = summary_phrase(key);
        if count == 1 { one.to_owned() } else { many.replace("{n}", &count.to_string()) }
    };
    let lower_first = |text: String| {
        let mut chars = text.chars();
        chars.next().map(|c| c.to_lowercase().collect::<String>() + chars.as_str()).unwrap_or_default()
    };
    match kinds.as_slice() {
        [] => String::new(),
        [(key, count)] => counted(key, *count),
        [(a, an), (b, bn)] => format!("{} and {}", counted(a, *an), lower_first(counted(b, *bn))),
        many => {
            // Four kinds is plenty for one line; the rest are inside.
            let mut parts: Vec<String> = many.iter().take(4).map(|(key, _)| summary_phrase(key).2.to_owned()).collect();
            if many.len() > 4 {
                parts.push("…".to_owned());
            }
            let joined = parts.join(", ");
            let mut chars = joined.chars();
            chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
        }
    }
}

/// "Running command" -> "Ran command" once the call is no longer running.
fn past_tense(label: &str) -> String {
    const PAIRS: [(&str, &str); 24] = [
        ("Running", "Ran"), ("Reading", "Read"), ("Writing", "Wrote"), ("Editing", "Edited"), ("Listing", "Listed"),
        ("Fetching", "Fetched"), ("Opening", "Opened"), ("Going to", "Went to"), ("Clicking", "Clicked"),
        ("Typing", "Typed"), ("Choosing", "Chose"), ("Pressing", "Pressed"), ("Scrolling", "Scrolled"),
        ("Starting", "Started"), ("Messaging", "Messaged"), ("Checking", "Checked"), ("Loading", "Loaded"),
        ("Waiting", "Waited"), ("Stopping", "Stopped"), ("Asking", "Asked"), ("Adding", "Added"),
        ("Creating", "Created"), ("Focusing", "Focused"), ("Using", "Used"),
    ];
    for (present, past) in PAIRS {
        if let Some(rest) = label.strip_prefix(present) {
            return format!("{past}{rest}");
        }
    }
    label.to_owned()
}

fn row_title(activity: &chat::ToolActivity) -> String {
    let label = tool_label(activity);
    if activity.state == "running" { label } else { past_tense(&label) }
}

enum Row<'a> {
    Single(&'a chat::ToolActivity),
    Group { title: String, failed: usize, running: bool, items: Vec<&'a chat::ToolActivity> },
}

fn group_rows(activities: &[chat::ToolActivity]) -> Vec<Row<'_>> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < activities.len() {
        let kind = tool_kind(&activities[index].name);
        let mut end = index + 1;
        while end < activities.len() && tool_kind(&activities[end].name).key == kind.key {
            end += 1;
        }
        let run = &activities[index..end];
        if run.len() >= 2 && kind.key != "other" {
            let running = run.iter().any(|a| a.state == "running");
            let failed = run.iter().filter(|a| a.state == "failed").count();
            let verb = if running { "Running" } else { kind.verb };
            rows.push(Row::Group { title: format!("{verb} {} {}", run.len(), kind.plural), failed, running, items: run.iter().collect() });
        } else {
            rows.extend(run.iter().map(Row::Single));
        }
        index = end;
    }
    rows
}

fn pretty_json(text: &str) -> String {
    serde_json::from_str::<Value>(text).ok().and_then(|value| serde_json::to_string_pretty(&value).ok()).unwrap_or_else(|| text.to_owned())
}

/// A small read-only, selectable, copyable box (it cannot be edited).
fn readonly_box(ui: &mut egui::Ui, id: egui::Id, text: &str, width: f32) {
    egui::Frame::new().fill(SHELL).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(8)).inner_margin(Margin::same(7)).show(ui, |ui| {
        ui.set_width((width - 16.0).max(60.0));
        let rows = text.lines().count().clamp(1, 7);
        egui::ScrollArea::vertical().id_salt(id).max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
            let mut buffer: &str = text;
            ui.add(
                egui::TextEdit::multiline(&mut buffer)
                    .font(egui::FontId::monospace(10.5))
                    .text_color(INK)
                    .desired_rows(rows)
                    .desired_width(f32::INFINITY)
                    .frame(egui::Frame::NONE),
            );
        });
    });
}

fn caret_icon(ui: &mut egui::Ui, openness: f32, response: &egui::Response) {
    let color = if response.hovered() { HOVER_GREEN } else { MUTED };
    paint_icon(ui.painter(), response.rect.shrink(1.0), if openness > 0.5 { "chevron" } else { "chevron_right" }, color);
}

/// "0m 12s": the running clock shown while a command works.
pub(super) fn format_clock(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    format!("{}m {}s", seconds / 60, seconds % 60)
}

impl RheaApp {
    /// A slash command in the transcript: the command as the user's message, then Rhea's side
    /// showing the running clock (`running_since`) or the outcome.
    pub(super) fn command_note(&mut self, ui: &mut egui::Ui, position: usize, note: &ChatNote, running_since: Option<Instant>) {
        ui.add_space(14.0);
        ui.horizontal(|row| {
            row.with_layout(egui::Layout::right_to_left(egui::Align::Min), |row| {
                egui::Frame::new()
                    .fill(Color32::from_rgb(35, 37, 34))
                    .stroke(egui::Stroke::new(1.0, LINE))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin { left: 15, right: 15, top: 11, bottom: 11 })
                    .show(row, |bubble| {
                        bubble.label(egui::RichText::new(&note.command).color(INK));
                    });
            });
        });
        ui.add_space(14.0);
        ui.push_id(("command-note", position), |ui| {
            ui.horizontal(|ui| {
                brand_mark(ui, 22.0);
            });
            ui.add_space(5.0);
            match running_since {
                Some(started) => {
                    let elapsed = started.elapsed().as_millis() as u64;
                    ui.label(egui::RichText::new(format!("Compacting… it’s been {}", format_clock(elapsed))).color(HOVER_GREEN));
                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                }
                None => {
                    ui.add(egui::Label::new(egui::RichText::new(&note.text).color(if note.failed { DANGER } else { INK })).wrap().selectable(true));
                    if !note.detail.is_empty() {
                        ui.add(egui::Label::new(egui::RichText::new(&note.detail).size(11.0).color(MUTED)).wrap().selectable(true));
                    }
                }
            }
        });
    }

    pub(super) fn assistant_message(&mut self, ui: &mut egui::Ui, index: usize, turn: &Turn, last_turn: bool, chat_width: f32) {
        let detail_width = (chat_width * 0.5).clamp(260.0, 640.0);
        ui.vertical(|message| {
            message.set_max_width(message.available_width().min(760.0));
            message.horizontal(|ui| {
                brand_mark(ui, 22.0);
                if self.streaming && last_turn {
                    let activity = turn.tool_activity.iter().rev().find(|item| item.state == "running").map(tool_label)
                        .or_else(|| self.active_tool.as_deref().map(|name| tool_activity(name).trim_end_matches('…').to_owned()))
                        .unwrap_or_else(|| "Thinking".to_owned());
                    ui.add(egui::Label::new(egui::RichText::new(format!("{activity}…")).size(11.0).color(MUTED)).truncate());
                }
            });
            message.add_space(5.0);
            if !turn.content.is_empty() {
                message.scope(|ui| {
                    ui.style_mut().visuals.override_text_color = Some(INK);
                    ui.style_mut().interaction.selectable_labels = true;
                    CommonMarkViewer::new().show(ui, &mut self.markdown_cache, &preserve_line_breaks(&plain_dashes(&turn.content)));
                });
            }

            if !turn.tool_activity.is_empty() {
                message.add_space(4.0);
                let live = self.streaming && last_turn;
                let header = if live {
                    let elapsed = self.assistant_started_at.map(|started| started.elapsed().as_millis() as u64).unwrap_or(0);
                    format!("Working for {}", format_duration(elapsed))
                } else {
                    let summary = turn_summary(&turn.tool_activity);
                    match turn.elapsed_ms.map(format_duration) {
                        Some(duration) => format!("{summary} · {duration}"),
                        None => summary,
                    }
                };
                egui::CollapsingHeader::new(egui::RichText::new(header).size(12.0).color(MUTED))
                    .id_salt(("tool-history", index))
                    .default_open(false)
                    .icon(caret_icon)
                    .show(message, |details| {
                        details.spacing_mut().item_spacing.y = 3.0;
                        for row in group_rows(&turn.tool_activity) {
                            match row {
                                Row::Single(activity) => self.tool_call(details, activity, detail_width),
                                Row::Group { title, failed, running, items } => {
                                    let mut text = egui::RichText::new(title).size(11.5).color(if failed > 0 { DANGER } else { INK });
                                    if running {
                                        text = text.color(HOVER_GREEN);
                                    }
                                    let id = items.first().map(|a| a.id.clone()).unwrap_or_default();
                                    egui::CollapsingHeader::new(text).id_salt(("tool-group", index, id)).icon(caret_icon).default_open(false).show(details, |group| {
                                        group.spacing_mut().item_spacing.y = 2.0;
                                        for activity in items {
                                            self.tool_call(group, activity, detail_width);
                                        }
                                    });
                                    if failed > 0 {
                                        details.label(egui::RichText::new(format!("{failed} failed")).size(10.5).color(DANGER));
                                    }
                                }
                            }
                        }
                    });
            }

            if !turn.content.is_empty() {
                message.add_space(2.0);
                message.horizontal(|actions| {
                    let copy = icon_button(actions, "copy", "Copy response", 34.0);
                    if copy.clicked() { actions.ctx().copy_text(plain_dashes(&turn.content)); self.notice = "Response copied to clipboard.".into(); }
                    let use_prompt = icon_button(actions, "use", "Use response as a prompt", 34.0);
                    if use_prompt.clicked() { self.draft = turn.content.clone(); }
                    if last_turn {
                        let regenerate = actions.add_enabled_ui(!self.streaming, |ui| icon_button(ui, "refresh", "Regenerate response", 34.0)).inner;
                        if regenerate.clicked() { self.regenerate_response(); }
                    }
                });
            }
        });
    }

    /// One tool call: a compact title; its arguments and result open in small read-only boxes.
    fn tool_call(&mut self, ui: &mut egui::Ui, activity: &chat::ToolActivity, width: f32) {
        let failed = activity.state == "failed";
        let running = activity.state == "running";
        let mut title = row_title(activity);
        if failed {
            title.push_str(" · failed");
        }
        let color = if failed { DANGER } else if running { HOVER_GREEN } else { INK };
        egui::CollapsingHeader::new(egui::RichText::new(title).size(11.5).color(color))
            .id_salt(activity.id.as_str())
            .icon(caret_icon)
            .default_open(failed)
            .show(ui, |tool| {
                tool.set_max_width(width);
                tool.spacing_mut().item_spacing.y = 4.0;
                if !activity.arguments_detail.is_empty() {
                    tool.label(egui::RichText::new("Arguments").size(10.5).strong().color(MUTED));
                    readonly_box(tool, tool.make_persistent_id(("tool-args", activity.id.as_str())), &pretty_json(&activity.arguments_detail), width);
                }
                tool.label(egui::RichText::new(if failed { "Error" } else { "Result" }).size(10.5).strong().color(if failed { DANGER } else { MUTED }));
                let detail = if !activity.result_detail.is_empty() {
                    pretty_json(&activity.result_detail)
                } else if activity.result_summary.trim().is_empty() && failed {
                    "No error details were reported.".to_owned()
                } else {
                    activity.result_summary.clone()
                };
                readonly_box(tool, tool.make_persistent_id(("tool-result", activity.id.as_str())), &detail, width);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> chat::ToolActivity {
        chat::ToolActivity { id: name.into(), name: name.into(), state: "complete".into(), ..Default::default() }
    }

    #[test]
    fn one_kind_keeps_its_count() {
        assert_eq!(turn_summary(&[call("mcp__rhea_shell__run_command")]), "Ran a command");
        let four: Vec<_> = (0..4).map(|_| call("mcp__rhea_shell__run_command")).collect();
        assert_eq!(turn_summary(&four), "Ran 4 commands");
    }

    #[test]
    fn two_kinds_read_as_one_sentence_with_counts() {
        let mut calls: Vec<_> = (0..4).map(|_| call("mcp__rhea_shell__run_command")).collect();
        calls.push(call("mcp__rhea_shell__edit_file"));
        calls.push(call("mcp__rhea_shell__write_file"));
        assert_eq!(turn_summary(&calls), "Ran 4 commands and edited 2 files");
    }

    #[test]
    fn three_or_more_kinds_are_a_plain_comma_list_in_order_of_first_use() {
        let calls = [call("mcp__rhea_shell__edit_file"), call("mcp__rhea_shell__run_command"), call("mcp__rhea_shell__web_fetch"), call("mcp__rhea_shell__run_command")];
        assert_eq!(turn_summary(&calls), "Edited files, ran commands, searched the web");
        assert_eq!(turn_summary(&[]), "");
    }
}
