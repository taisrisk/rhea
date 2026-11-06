//! Settings > Usage: lifetime totals across every chat (deleted and archived ones included), the
//! last three months as one box per day, where the tokens went, and a few habits.

use super::*;
use crate::usage::{self, MONTHS, Usage};

/// What the model wrote (the other pieces use the context panel's colors).
const OUTPUT_SEGMENT: Color32 = Color32::from_rgb(125, 168, 255);
/// A day with no usage.
const EMPTY_DAY: Color32 = Color32::from_rgb(38, 40, 38);
/// Light use: dark green. Heavy use: lime.
const LOW_DAY: Color32 = Color32::from_rgb(22, 62, 40);
const HIGH_DAY: Color32 = MEMORY_SEGMENT;
/// How many days the activity grid covers.
const GRID_DAYS: usize = 91;

impl RheaApp {
    fn usage_record(&mut self) -> Usage {
        let stale = self.usage_view.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(2));
        if stale {
            self.usage_view = Some((Instant::now(), usage::snapshot()));
        }
        self.usage_view.as_ref().map(|(_, usage)| usage.clone()).unwrap_or_default()
    }

    pub(super) fn usage_totals_ui(&mut self, ui: &mut egui::Ui) {
        let record = self.usage_record();
        let today = usage::today();
        ui.ctx().request_repaint_after(Duration::from_secs(2));
        profile_card(ui, &record, &today);
        activity_card(ui, &record, &today);
        breakdown_card(ui, &record);
        insights_card(ui, &record);
    }
}

/// Lifetime tokens, the best day, time worked and streaks.
fn profile_card(ui: &mut egui::Ui, record: &Usage, today: &str) {
    let (longest_streak, current_streak) = record.streaks(today);
    let cells = [
        (format_tokens(record.lifetime_tokens()), "Lifetime tokens"),
        (format_tokens(record.peak_day().map_or(0, |(_, tokens)| tokens)), "Peak tokens in a day"),
        (format_worked(record.most_worked_day_ms()), "Most worked in a day"),
        (format_worked(record.longest_task_ms), "Longest task"),
        (format_days(longest_streak), "Longest streak"),
        (format_days(current_streak), "Current streak"),
    ];
    settings_group(ui, None, |ui| {
        let columns = 3;
        let rows = cells.len().div_ceil(columns);
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, rows as f32 * 62.0), Sense::hover());
        let cell_width = width / columns as f32;
        for (index, (value, label)) in cells.iter().enumerate() {
            let (column, row) = (index % columns, index / columns);
            let center_x = rect.left() + cell_width * (column as f32 + 0.5);
            let top = rect.top() + row as f32 * 62.0;
            ui.painter().text(pos2(center_x, top + 22.0), egui::Align2::CENTER_CENTER, value, egui::FontId::proportional(18.0), INK);
            ui.painter().text(pos2(center_x, top + 44.0), egui::Align2::CENTER_CENTER, *label, egui::FontId::proportional(11.0), MUTED);
            if column > 0 {
                let x = rect.left() + cell_width * column as f32;
                ui.painter().line_segment([pos2(x, top + 10.0), pos2(x, top + 54.0)], egui::Stroke::new(1.0, LINE));
            }
        }
        if rows > 1 {
            let y = rect.top() + 62.0;
            ui.painter().line_segment([pos2(rect.left() + 8.0, y), pos2(rect.right() - 8.0, y)], egui::Stroke::new(1.0, LINE));
        }
    });
}

/// Dark green for a light day through lime for the busiest one; grey when nothing was used.
fn activity_color(tokens: u64, peak: u64) -> Color32 {
    if tokens == 0 || peak == 0 {
        return EMPTY_DAY;
    }
    // A square root keeps one huge day from turning every ordinary day the same dark green.
    let share = (tokens as f32 / peak as f32).sqrt().clamp(0.0, 1.0);
    lerp_color(LOW_DAY, HIGH_DAY, share)
}

/// The last three months, one box per day, a month name under each month and a divider between them.
fn activity_card(ui: &mut egui::Ui, record: &Usage, today: &str) {
    let blocks = record.heatmap(today, GRID_DAYS);
    settings_group(ui, Some("Token activity · last 3 months"), |ui| {
        if blocks.is_empty() {
            return;
        }
        let peak = blocks.iter().flat_map(|block| block.columns.iter().flatten().flatten()).map(|cell| cell.tokens).max().unwrap_or(0);
        let columns: usize = blocks.iter().map(|block| block.columns.len()).sum();
        let (gap, block_gap) = (3.0, 22.0);
        let dividers = (blocks.len() - 1) as f32;
        let room = (ui.available_width() - 6.0 - block_gap * dividers).max(40.0);
        let cell = (room / columns.max(1) as f32 - gap).clamp(8.0, 22.0);
        let step = cell + gap;
        let grid_height = 7.0 * step - gap;
        let width = columns as f32 * step - gap + block_gap * dividers + dividers * gap;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), grid_height + 22.0), Sense::hover());
        let hover = response.hover_pos();
        let mut hovered: Option<usage::Cell> = None;
        // Centered in the card.
        let mut x = rect.left() + ((rect.width() - width) / 2.0).max(0.0);
        for (index, block) in blocks.iter().enumerate() {
            let block_width = block.columns.len() as f32 * step - gap;
            if index > 0 {
                let divider_x = x - block_gap / 2.0;
                ui.painter().line_segment([pos2(divider_x, rect.top() + 2.0), pos2(divider_x, rect.top() + grid_height - 2.0)], egui::Stroke::new(1.0, LINE));
            }
            for (column_index, column) in block.columns.iter().enumerate() {
                for (row, entry) in column.iter().enumerate() {
                    let Some(entry) = entry else { continue };
                    let cell_rect = Rect::from_min_size(pos2(x + column_index as f32 * step, rect.top() + row as f32 * step), Vec2::splat(cell));
                    ui.painter().rect_filled(cell_rect, CornerRadius::same(3), activity_color(entry.tokens, peak));
                    if entry.key == today {
                        ui.painter().rect_stroke(cell_rect, CornerRadius::same(3), egui::Stroke::new(1.0, INK), egui::StrokeKind::Outside);
                    }
                    if hover.is_some_and(|pointer| cell_rect.expand(gap / 2.0).contains(pointer)) {
                        hovered = Some(entry.clone());
                    }
                }
            }
            ui.painter().text(pos2(x + block_width / 2.0, rect.top() + grid_height + 11.0), egui::Align2::CENTER_CENTER, block.label, egui::FontId::proportional(10.5), MUTED);
            x += block_width + block_gap + gap;
        }
        if let Some(entry) = hovered {
            response.on_hover_ui_at_pointer(|ui| {
                ui.label(egui::RichText::new(date_text(&entry.key)).strong().color(INK));
                ui.label(egui::RichText::new(if entry.tokens == 0 { "No usage".to_owned() } else { format!("{} tokens", format_tokens(entry.tokens)) }).color(INK));
                if entry.work_ms > 0 {
                    ui.label(egui::RichText::new(format!("Worked {}", format_worked(entry.work_ms))).size(11.0).color(MUTED));
                }
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new("More").size(10.5).color(MUTED));
                for share in [1.0_f32, 0.6, 0.3, 0.1] {
                    let (swatch, _) = ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
                    ui.painter().rect_filled(swatch, CornerRadius::same(3), lerp_color(LOW_DAY, HIGH_DAY, share.sqrt()));
                }
                let (swatch, _) = ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
                ui.painter().rect_filled(swatch, CornerRadius::same(3), EMPTY_DAY);
                ui.label(egui::RichText::new("Less").size(10.5).color(MUTED));
            });
        });
    });
}

/// "Nov 6, 2026" from "2026-11-06".
fn date_text(key: &str) -> String {
    let mut parts = key.split('-');
    let (year, month, day) = (parts.next().unwrap_or(""), parts.next().and_then(|m| m.parse::<usize>().ok()), parts.next().and_then(|d| d.parse::<u32>().ok()));
    match (month, day) {
        (Some(month), Some(day)) if (1..=12).contains(&month) => format!("{} {day}, {year}", MONTHS[month - 1]),
        _ => key.to_owned(),
    }
}

/// A tall bar split like the context panel's ring: system prompt, tools, memory, messages, replies.
fn breakdown_card(ui: &mut egui::Ui, record: &Usage) {
    let parts = record.parts;
    let total = parts.total();
    let rows = [
        (SYSTEM_PROMPT_SEGMENT, "System prompts", parts.system),
        (TOOLSET_SEGMENT, "Tools & MCP", parts.tools),
        (MEMORY_SEGMENT, "Memory", parts.memory),
        (MINT_DEEP, "Messages & results", parts.messages),
        (OUTPUT_SEGMENT, "Replies", parts.output),
    ];
    settings_group(ui, Some("Where your usage went"), |ui| {
        if total == 0 {
            ui.label(egui::RichText::new("Nothing to split yet. Send a message and the next request shows up here.").size(11.0).color(MUTED));
            return;
        }
        let bar_height = 170.0;
        ui.horizontal(|ui| {
            let (bar, response) = ui.allocate_exact_size(Vec2::new(30.0, bar_height), Sense::hover());
            let shown: Vec<&(Color32, &str, u64)> = rows.iter().filter(|(_, _, tokens)| *tokens > 0).collect();
            let gap = 2.0;
            let room = bar_height - gap * shown.len().saturating_sub(1) as f32;
            let mut y = bar.top();
            for (index, (color, _, tokens)) in shown.iter().enumerate() {
                let height = (room * *tokens as f32 / total as f32).max(2.0);
                let radius = 9;
                let corners = CornerRadius {
                    nw: if index == 0 { radius } else { 2 },
                    ne: if index == 0 { radius } else { 2 },
                    sw: if index + 1 == shown.len() { radius } else { 2 },
                    se: if index + 1 == shown.len() { radius } else { 2 },
                };
                ui.painter().rect_filled(Rect::from_min_size(pos2(bar.left(), y), Vec2::new(bar.width(), height)), corners, *color);
                y += height + gap;
            }
            response.on_hover_text(format!("{} tokens tracked", format_tokens(total)));
            ui.add_space(16.0);
            let width = ui.available_width();
            ui.allocate_ui_with_layout(Vec2::new(width, bar_height), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.add_space(14.0);
                for (color, label, tokens) in rows {
                    let share = tokens as f32 / total as f32 * 100.0;
                    context_segment_row(ui, color, label, format!("{} · {}", format_tokens(tokens), percent_text(share)));
                    ui.add_space(4.0);
                }
            });
        });
        if record.before_tracking > 0 {
            ui.add_space(6.0);
            ui.add(egui::Label::new(egui::RichText::new(format!("{} tokens from before this was tracked are in the lifetime total but not split here.", format_tokens(record.before_tracking))).size(10.5).color(MUTED)).wrap());
        }
    });
}

fn insights_card(ui: &mut egui::Ui, record: &Usage) {
    settings_group(ui, Some("Insights"), |ui| {
        let fast = record.fast_percent().map_or("No runs yet".to_owned(), |percent| format!("{percent}% of runs"));
        let reasoning = record.top_effort().map_or("No runs yet".to_owned(), |(effort, percent)| format!("{} · {percent}%", effort_label(&effort)));
        context_row(ui, "Fast mode", fast);
        context_row(ui, "Most used reasoning", reasoning);
        context_row(ui, "Time worked", format_worked(record.worked_total_ms()));
        context_row(ui, "Runs", record.runs.to_string());
        context_row(ui, "Skills explored", record.skills_explored().to_string());
        context_row(ui, "Total skills used", record.skills_used().to_string());
    });
}

/// "xhigh" as "Extra High".
fn effort_label(effort: &str) -> String {
    match effort {
        "xhigh" => "Extra High".into(),
        other => {
            let mut chars = other.chars();
            chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
        }
    }
}

/// "9h 1m", "12m 30s", "40s".
fn format_worked(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let (hours, minutes, rest) = (seconds / 3_600, seconds % 3_600 / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {rest}s")
    } else {
        format!("{rest}s")
    }
}

fn format_days(days: u32) -> String {
    format!("{days} day{}", if days == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busier_days_are_lighter_and_idle_days_are_grey() {
        assert_eq!(activity_color(0, 1_000), EMPTY_DAY);
        assert_eq!(activity_color(1_000, 1_000), HIGH_DAY);
        let low = activity_color(10, 1_000);
        let mid = activity_color(250, 1_000);
        assert!(low.g() < mid.g() && mid.g() < HIGH_DAY.g(), "{low:?} {mid:?}");
        assert_ne!(low, EMPTY_DAY);
        assert_eq!(activity_color(5, 0), EMPTY_DAY, "no peak, nothing to scale against");
    }

    #[test]
    fn durations_and_dates_read_the_way_the_profile_shows_them() {
        assert_eq!(format_worked(9 * 3_600_000 + 61_000), "9h 1m");
        assert_eq!(format_worked(750_000), "12m 30s");
        assert_eq!(format_worked(40_000), "40s");
        assert_eq!(format_worked(0), "0s");
        assert_eq!((format_days(1), format_days(38)), ("1 day".to_owned(), "38 days".to_owned()));
        assert_eq!(date_text("2026-11-06"), "Nov 6, 2026");
        assert_eq!(date_text("garbage"), "garbage");
        assert_eq!((effort_label("xhigh"), effort_label("high"), effort_label("medium")), ("Extra High".to_owned(), "High".to_owned(), "Medium".to_owned()));
    }
}
