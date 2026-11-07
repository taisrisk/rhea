//! UI for the team features: the question / plan panel that grows out of the chat box,
//! the History | Subagents tabs on the right panel, the subagent chat view, and the
//! Agents & hooks settings page.

use super::*;
use crate::team::{Agent, AgentStatus, AskRequest, PlanRequest};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum SideTab {
    #[default]
    History,
    Subagents,
}

/// Progress through the questions of one `ask_user` call.
#[derive(Default)]
pub(super) struct AskUi {
    request_id: u64,
    index: usize,
    answers: Vec<(String, String)>,
    selected: Option<usize>,
    custom: String,
}

pub(super) enum Interaction {
    Ask(AskRequest),
    Plan(PlanRequest),
}

pub(super) fn title_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn age_label(agent: &Agent) -> String {
    let since = agent.finished_ms.unwrap_or(agent.started_ms);
    let seconds = now_ms().saturating_sub(since) / 1000;
    match seconds {
        0..=4 => "now".into(),
        5..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// Deterministic 5x5 pixel avatar for an agent: two complementary colors in a
/// left-right symmetric pattern, derived from the agent's identity in microseconds.
pub(super) fn avatar_cells(seed: &str) -> [[Color32; 5]; 5] {
    // FNV-1a, then xorshift64* for the random stream.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in seed.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let mut state = hash | 1;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let hue = (next() % 360) as f32;
    let color_a = hsv_color(hue, 0.55 + (next() % 30) as f32 / 100.0, 0.90);
    let color_b = hsv_color(hue + 180.0, 0.50 + (next() % 30) as f32 / 100.0, 0.78 + (next() % 15) as f32 / 100.0);
    // 15 independent cells (3 columns x 5 rows), mirrored; keep both colors well represented.
    let mut bits = 0u16;
    for _ in 0..32 {
        bits = (next() & 0x7fff) as u16;
        if (6..=9).contains(&bits.count_ones()) {
            break;
        }
    }
    let mut cells = [[color_a; 5]; 5];
    for (row, line) in cells.iter_mut().enumerate() {
        for (column, cell) in line.iter_mut().enumerate() {
            let source = column.min(4 - column);
            *cell = if bits >> (row * 3 + source) & 1 == 1 { color_a } else { color_b };
        }
    }
    cells
}

fn hsv_color(hue: f32, saturation: f32, value: f32) -> Color32 {
    let hue = hue.rem_euclid(360.0) / 60.0;
    let chroma = value * saturation;
    let x = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = value - chroma;
    Color32::from_rgb(((r + m) * 255.0).round() as u8, ((g + m) * 255.0).round() as u8, ((b + m) * 255.0).round() as u8)
}

/// Paint the avatar into `rect` as a rounded square (10 px corners, capped for small sizes).
fn paint_avatar(ui: &egui::Ui, agent: &Agent, rect: Rect) {
    let key = format!("{}:{}", agent.id, agent.name);
    let texture_id = egui::Id::new(("agent-avatar", key.clone()));
    let texture = ui.ctx().data(|data| data.get_temp::<egui::TextureHandle>(texture_id)).unwrap_or_else(|| {
        let cells = avatar_cells(&key);
        let pixels: Vec<Color32> = cells.iter().flatten().copied().collect();
        let handle = ui.ctx().load_texture(format!("agent-avatar-{key}"), egui::ColorImage::new([5, 5], pixels), egui::TextureOptions::NEAREST);
        ui.ctx().data_mut(|data| data.insert_temp(texture_id, handle.clone()));
        handle
    });
    let radius = (10.0_f32).min(rect.width() * 0.32).round() as u8;
    egui::Image::new(egui::load::SizedTexture::new(texture.id(), rect.size()))
        .corner_radius(CornerRadius::same(radius))
        .paint_at(ui, rect);
}

impl RheaApp {
    pub(super) fn main_tool_context(&self) -> tools::ToolContext {
        tools::ToolContext {
            mcp_manager: Arc::clone(&self.mcp_manager),
            chrome_alive: Arc::clone(&self.chrome_alive),
            hooks: Arc::clone(&self.hooks),
            chat_id: self.conversations.get(self.active_conversation).map(|c| c.id.clone()).unwrap_or_default(),
            agent_id: team::MAIN.into(),
            agent_name: "main".into(),
            model: self.selected_model.clone(),
            effort: self.reasoning_effort.clone(),
            context_window: self.context_window,
            fast: self.fast_mode,
            provider: self.model_list.iter().find(|m| m.slug == self.selected_model).map(|m| m.provider).unwrap_or_default(),
            compose: self.compose_mode,
            subagent_model: self.subagent_model.clone(),
            compact_percent: self.compact_percent,
        }
    }

    pub(super) fn active_chat_id(&self) -> String {
        self.conversations.get(self.active_conversation).map(|c| c.id.clone()).unwrap_or_default()
    }

    /// Load a chat's saved agents the first time it becomes active.
    pub(super) fn sync_agents_for_active_chat(&mut self) {
        let id = self.active_chat_id();
        if id.is_empty() || self.agents_loaded_for == id {
            return;
        }
        team::hub().load_chat(&id);
        self.agents_loaded_for = id;
    }

    // ---- question / plan panel ----------------------------------------------------

    pub(super) fn current_interaction(&self) -> Option<Interaction> {
        let hub = team::hub();
        hub.pending_plan().map(Interaction::Plan).or_else(|| hub.pending_ask().map(Interaction::Ask))
    }

    /// Height the panel wants for this interaction, capped at 55% of the workspace.
    pub(super) fn interaction_height(&self, interaction: &Interaction, workspace_height: f32) -> f32 {
        let cap = (workspace_height * 0.55).max(150.0);
        match interaction {
            Interaction::Plan(_) => cap,
            Interaction::Ask(request) => {
                let options = request.questions.get(self.ask_ui.as_ref().map_or(0, |ask| ask.index)).map_or(0, |q| q.options.len());
                (150.0 + options as f32 * 38.0).min(cap)
            }
        }
    }

    pub(super) fn interaction_panel(&mut self, ui: &mut egui::Ui, rect: Rect, interaction: Option<Interaction>, progress: f32) {
        let radius = CornerRadius { nw: 18, ne: 18, sw: 0, se: 0 };
        ui.painter().rect_filled(rect, radius, RAISED);
        ui.painter().rect_stroke(rect, radius, egui::Stroke::new(1.0, Color32::from_rgb(48, 50, 49)), egui::StrokeKind::Inside);
        let Some(interaction) = interaction else { return };
        let mut inner = ui.new_child(
            UiBuilder::new().layout(egui::Layout::top_down(egui::Align::Min)).id_salt("interaction-panel").max_rect(rect.shrink2(Vec2::new(16.0, 12.0))),
        );
        inner.set_clip_rect(rect);
        inner.multiply_opacity(progress);
        match interaction {
            Interaction::Ask(request) => self.ask_panel(&mut inner, request),
            Interaction::Plan(plan) => self.plan_panel(&mut inner, plan),
        }
    }

    fn ask_panel(&mut self, ui: &mut egui::Ui, request: AskRequest) {
        if self.ask_ui.as_ref().is_none_or(|ask| ask.request_id != request.id) {
            self.ask_ui = Some(AskUi { request_id: request.id, ..Default::default() });
        }
        let Some(mut state) = self.ask_ui.take() else { return };
        let total = request.questions.len();
        let Some(question) = request.questions.get(state.index).cloned() else {
            team::hub().answer_ask(request.id, Some(state.answers));
            return;
        };
        let mut skip = false;
        let mut send = false;
        ui.horizontal(|ui| {
            let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
            paint_icon(ui.painter(), icon_rect, "question", MUTED);
            ui.label(egui::RichText::new(if total > 1 { format!("Question {} of {}", state.index + 1, total) } else { "Question".to_owned() }).size(12.0).color(MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, "cancel", "Skip these questions", 24.0).clicked() {
                    skip = true;
                }
            });
        });
        ui.add_space(8.0);
        ui.label(egui::RichText::new(&question.question).size(13.0).strong().color(INK));
        ui.add_space(8.0);
        egui::ScrollArea::vertical().id_salt("ask-options").max_height((ui.available_height() - 52.0).max(40.0)).auto_shrink([false, true]).show(ui, |ui| {
            for (index, option) in question.options.iter().enumerate() {
                let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());
                let chosen = state.selected == Some(index) && state.custom.trim().is_empty();
                if chosen || response.hovered() {
                    ui.painter().rect_filled(rect, CornerRadius::same(9), if chosen { HOVER_FILL } else { MINT });
                }
                let key = Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), Vec2::splat(22.0));
                ui.painter().rect_filled(key, CornerRadius::same(6), if chosen { HOVER_GREEN.gamma_multiply(0.35) } else { MINT });
                ui.painter().text(key.center(), egui::Align2::CENTER_CENTER, format!("{}", index + 1), egui::FontId::proportional(11.0), INK);
                let galley = ui.painter().layout_no_wrap(option.label.clone(), egui::FontId::proportional(12.5), INK);
                let text_pos = pos2(key.right() + 12.0, rect.center().y - galley.size().y / 2.0);
                let label_width = galley.size().x;
                ui.painter().galley(text_pos, galley, INK);
                if !option.description.is_empty() {
                    ui.painter().text(pos2(text_pos.x + label_width + 10.0, rect.center().y), egui::Align2::LEFT_CENTER, &option.description, egui::FontId::proportional(11.0), MUTED);
                }
                if response.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if response.clicked() {
                    state.selected = Some(index);
                    state.custom.clear();
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
            paint_icon(ui.painter(), icon_rect, "edit", MUTED);
            let width = (ui.available_width() - 150.0).max(80.0);
            let edit = ui.add_enabled(
                question.allow_custom,
                egui::TextEdit::singleline(&mut state.custom)
                    .desired_width(width)
                    .frame(egui::Frame::NONE)
                    .hint_text(egui::RichText::new(if question.allow_custom { "Or write your own response" } else { "Pick an option above" }).color(MUTED)),
            );
            if edit.changed() && !state.custom.trim().is_empty() {
                state.selected = None;
            }
            let typed = !state.custom.trim().is_empty();
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && typed {
                send = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = typed || state.selected.is_some();
                if ui.add_enabled(ready, egui::Button::new(egui::RichText::new("Send").size(12.0).strong().color(INK)).fill(Color32::from_rgb(33, 78, 55)).corner_radius(CornerRadius::same(14)).min_size(Vec2::new(64.0, 28.0))).clicked() {
                    send = true;
                }
                if ui.add(egui::Button::new(egui::RichText::new("Skip").size(12.0).color(INK)).fill(MINT).corner_radius(CornerRadius::same(14)).min_size(Vec2::new(60.0, 28.0))).clicked() {
                    skip = true;
                }
            });
        });
        if skip {
            team::hub().answer_ask(request.id, None);
            self.ask_ui = None;
            return;
        }
        if send {
            let answer = if !state.custom.trim().is_empty() {
                Some(state.custom.trim().to_owned())
            } else {
                state.selected.and_then(|index| question.options.get(index)).map(|option| option.label.clone())
            };
            if let Some(answer) = answer {
                state.answers.push((question.id.clone(), answer));
                state.index += 1;
                state.selected = None;
                state.custom.clear();
                if state.index >= total {
                    team::hub().answer_ask(request.id, Some(std::mem::take(&mut state.answers)));
                    self.ask_ui = None;
                    return;
                }
            }
        }
        self.ask_ui = Some(state);
    }

    fn plan_panel(&mut self, ui: &mut egui::Ui, plan: PlanRequest) {
        ui.horizontal(|ui| {
            let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
            paint_icon(ui.painter(), icon_rect, "compose", HOVER_GREEN);
            ui.label(egui::RichText::new("Proposed plan").size(12.0).strong().color(INK));
        });
        ui.add_space(6.0);
        let body_height = (ui.available_height() - 46.0).max(40.0);
        egui::ScrollArea::vertical().id_salt(("plan-body", plan.id)).max_height(body_height).auto_shrink([false, false]).show(ui, |ui| {
            ui.scope(|ui| {
                ui.style_mut().visuals.override_text_color = Some(INK);
                ui.style_mut().interaction.selectable_labels = true;
                CommonMarkViewer::new().show(ui, &mut self.markdown_cache, &preserve_line_breaks(&plan.markdown));
            });
        });
        ui.add_space(6.0);
        let mut accept = false;
        let mut revise = false;
        let mut cancel_revise = false;
        if self.plan_revising {
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 190.0).max(100.0);
                let edit = ui.add(
                    egui::TextEdit::multiline(&mut self.plan_feedback)
                        .desired_rows(1)
                        .desired_width(width)
                        .hint_text(egui::RichText::new("Tell Rhea what to change in the plan").color(MUTED)),
                );
                if !edit.has_focus() && !self.plan_feedback_focused {
                    edit.request_focus();
                    self.plan_feedback_focused = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ready = !self.plan_feedback.trim().is_empty();
                    if ui.add_enabled(ready, egui::Button::new(egui::RichText::new("Send revision").size(12.0).strong().color(INK)).fill(Color32::from_rgb(33, 78, 55)).corner_radius(CornerRadius::same(14)).min_size(Vec2::new(110.0, 28.0))).clicked() {
                        revise = true;
                    }
                    if ui.add(egui::Button::new(egui::RichText::new("Back").size(12.0).color(INK)).fill(MINT).corner_radius(CornerRadius::same(14)).min_size(Vec2::new(56.0, 28.0))).clicked() {
                        cancel_revise = true;
                    }
                });
            });
        } else {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new(egui::RichText::new("Accept").size(12.5).strong().color(INK)).fill(Color32::from_rgb(33, 78, 55)).corner_radius(CornerRadius::same(14)).min_size(Vec2::new(84.0, 30.0))).on_hover_text("Accept the plan and let Rhea start working").clicked() {
                    accept = true;
                }
                if ui.add(egui::Button::new(egui::RichText::new("Revise").size(12.5).color(INK)).fill(MINT).corner_radius(CornerRadius::same(14)).min_size(Vec2::new(78.0, 30.0))).on_hover_text("Ask Rhea to change the plan").clicked() {
                    self.plan_revising = true;
                    self.plan_feedback_focused = false;
                }
            });
        }
        if cancel_revise {
            self.plan_revising = false;
        }
        if revise {
            team::hub().decide_plan(plan.id, false, self.plan_feedback.trim());
            self.plan_feedback.clear();
            self.plan_revising = false;
        }
        if accept {
            team::hub().decide_plan(plan.id, true, "");
            self.plan_revising = false;
            self.plan_feedback.clear();
            self.compose_mode = false;
        }
    }

    // ---- right panel: tabs and subagents ------------------------------------------

    /// History | Subagents switcher; returns true when it was drawn.
    pub(super) fn side_tabs(&mut self, ui: &mut egui::Ui, agents: &[Agent]) {
        let running = agents.iter().filter(|a| a.status == AgentStatus::Running).count();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(16), SHELL);
        ui.painter().rect_stroke(rect, CornerRadius::same(16), egui::Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        let half = rect.width() / 2.0;
        let position = ui.ctx().animate_value_with_time(
            ui.make_persistent_id("side-tab-pill"),
            if self.side_tab == SideTab::History { 0.0 } else { 1.0 },
            ANIM,
        );
        let pill = Rect::from_min_size(pos2(rect.left() + 3.0 + position * half, rect.top() + 3.0), Vec2::new(half - 6.0, rect.height() - 6.0));
        ui.painter().rect_filled(pill, CornerRadius::same(13), MINT);
        for (index, (tab, label)) in [(SideTab::History, "History".to_owned()), (SideTab::Subagents, if running > 0 { format!("Subagents · {running}") } else { format!("Subagents · {}", agents.len()) })].into_iter().enumerate() {
            let tab_rect = Rect::from_min_size(pos2(rect.left() + index as f32 * half, rect.top()), Vec2::new(half, rect.height()));
            let response = ui.interact(tab_rect, ui.make_persistent_id(("side-tab", index)), Sense::click());
            let active = self.side_tab == tab;
            let color = if active { INK } else if response.hovered() { HOVER_GREEN } else { MUTED };
            ui.painter().text(tab_rect.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(11.5), color);
            if response.clicked() {
                self.side_tab = tab;
            }
        }
        ui.add_space(10.0);
    }

    pub(super) fn agents_panel(&mut self, ui: &mut egui::Ui, agents: Vec<Agent>) {
        if let Some(id) = self.open_agent.clone() {
            if let Some(agent) = agents.iter().find(|a| a.id == id).cloned() {
                self.agent_view(ui, agent);
                return;
            }
            self.open_agent = None;
        }
        let (active, done): (Vec<&Agent>, Vec<&Agent>) = agents.iter().partition(|a| a.status == AgentStatus::Running);
        egui::ScrollArea::vertical().id_salt("agents-list").auto_shrink([false, false]).show(ui, |ui| {
            ui.label(egui::RichText::new(format!("Active · {}", active.len())).size(12.0).color(MUTED));
            ui.add_space(4.0);
            if active.is_empty() {
                ui.label(egui::RichText::new("No active subagents").size(12.0).color(MUTED));
            }
            for agent in &active {
                self.agent_row(ui, agent);
            }
            ui.add_space(14.0);
            ui.label(egui::RichText::new(format!("Done · {}", done.len())).size(12.0).color(MUTED));
            ui.add_space(4.0);
            for agent in &done {
                self.agent_row(ui, agent);
            }
        });
    }

    fn agent_row(&mut self, ui: &mut egui::Ui, agent: &Agent) {
        let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
        if response.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(10), RAISED);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        paint_avatar(ui, agent, Rect::from_center_size(pos2(rect.left() + 20.0, rect.center().y), Vec2::splat(30.0)));
        let name = title_case(&agent.name);
        ui.painter().text(pos2(rect.left() + 44.0, rect.center().y), egui::Align2::LEFT_CENTER, name, egui::FontId::proportional(12.5), INK);
        let (status, color) = match agent.status {
            AgentStatus::Running => ("working".to_owned(), HOVER_GREEN),
            AgentStatus::Failed => ("failed".to_owned(), DANGER),
            AgentStatus::Stopped => ("stopped".to_owned(), MUTED),
            AgentStatus::Idle => (age_label(agent), MUTED),
        };
        ui.painter().text(pos2(rect.right() - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, status, egui::FontId::proportional(11.0), color);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &agent.name));
        if response.clicked() {
            self.open_agent = Some(agent.id.clone());
        }
    }

    fn agent_view(&mut self, ui: &mut egui::Ui, agent: Agent) {
        let running = agent.status == AgentStatus::Running;
        ui.horizontal(|ui| {
            if icon_button(ui, "back", "Back to subagents", 28.0).clicked() {
                self.open_agent = None;
            }
            let (badge, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
            paint_avatar(ui, &agent, badge);
            ui.label(egui::RichText::new(title_case(&agent.name)).size(13.5).strong().color(INK));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if running && icon_button(ui, "stop", "Stop this agent", 26.0).clicked() {
                    let _ = team::hub().stop(&agent.chat_id, &agent.id);
                }
                ui.label(egui::RichText::new(&agent.model).size(10.5).color(MUTED));
            });
        });
        ui.add_space(4.0);
        let footer = 44.0;
        let body_height = (ui.available_height() - footer).max(60.0);
        egui::ScrollArea::vertical().id_salt(("agent-transcript", agent.id.clone())).max_height(body_height).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for (index, turn) in agent.turns.iter().enumerate() {
                ui.add_space(8.0);
                if turn.role == "user" {
                    let heading = if index == 0 { "Task" } else { "Message" };
                    egui::Frame::new().fill(SHELL).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(9)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new(heading).size(10.5).strong().color(MUTED));
                        ui.add(egui::Label::new(egui::RichText::new(turn.content.chars().take(1200).collect::<String>()).size(11.5).color(MUTED)).wrap());
                    });
                    continue;
                }
                if !turn.tool_activity.is_empty() {
                    egui::CollapsingHeader::new(egui::RichText::new(format!("Worked · {} tool call{}", turn.tool_activity.len(), if turn.tool_activity.len() == 1 { "" } else { "s" })).size(11.5).color(MUTED))
                        .id_salt((agent.id.clone(), "tools", index))
                        .default_open(running)
                        .show(ui, |ui| {
                            for activity in &turn.tool_activity {
                                let failed = activity.state == "failed";
                                ui.add(egui::Label::new(egui::RichText::new(tool_title(activity)).size(11.0).color(if failed { DANGER } else { INK })).truncate());
                            }
                        });
                }
                if turn.content.trim().is_empty() {
                    if running && index + 1 == agent.turns.len() {
                        ui.label(egui::RichText::new("Working…").size(11.5).color(MUTED));
                    }
                } else {
                    ui.scope(|ui| {
                        ui.style_mut().visuals.override_text_color = Some(INK);
                        ui.style_mut().interaction.selectable_labels = true;
                        CommonMarkViewer::new().show(ui, &mut self.markdown_cache, &preserve_line_breaks(&turn.content));
                    });
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 40.0).max(80.0);
            let edit = ui.add(egui::TextEdit::singleline(&mut self.agent_draft).desired_width(width).hint_text(egui::RichText::new(format!("Message {}", title_case(&agent.name))).color(MUTED)));
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let send = icon_button(ui, "send", "Send to this agent", 30.0).clicked() || enter;
            if send && !self.agent_draft.trim().is_empty() {
                let context = self.main_tool_context();
                let text = std::mem::take(&mut self.agent_draft);
                match tools::message_agent(&context, &agent.id, text.trim()) {
                    Ok(_) => self.notice = format!("Sent to {}.", title_case(&agent.name)),
                    Err(error) => self.notice = error,
                }
                edit.request_focus();
            }
        });
    }

    // ---- settings -----------------------------------------------------------------
    fn skills_settings_ui(&mut self, ui: &mut egui::Ui) {
        let installed = crate::skills::list();
        settings_group(ui, Some("Skills"), |ui| {
            if installed.is_empty() {
                settings_row(ui, Some(MUTED), "No skills installed", "Paste a GitHub link below, or ask Rhea to install one. Folders in ~/.rhea/skills, ~/.agents/skills and ~/.claude/skills are found automatically.", |_| {});
            }
            let mut remove: Option<String> = None;
            for (index, skill) in installed.iter().enumerate() {
                if index > 0 {
                    row_divider(ui);
                }
                let detail = if skill.description.is_empty() { format!("/{} · {}", skill.name, skill.source) } else { format!("/{} · {}", skill.name, skill.description) };
                settings_row(ui, Some(HOVER_GREEN), &skill.name, &detail, |ui| {
                    if skill.source == "rhea" && action_button(ui, "cancel", "Remove", false).clicked() {
                        remove = Some(skill.name.clone());
                    }
                });
            }
            if let Some(name) = remove {
                self.notice = match crate::skills::remove(&name) {
                    Ok(()) => format!("Removed the {name} skill."),
                    Err(error) => error,
                };
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.skill_url).desired_width((ui.available_width() - 110.0).max(120.0)).hint_text(egui::RichText::new("https://github.com/owner/repo/tree/main/skills/name").color(MUTED)));
            let ready = !self.skill_url.trim().is_empty() && self.pending.is_none();
            if ui.add_enabled_ui(ready, |ui| action_button(ui, "check", "Install", true)).inner.clicked() {
                let source = std::mem::take(&mut self.skill_url);
                self.begin(
                    move |tx| {
                        let message = match crate::skills::install(&source, None) {
                            Ok(done) => format!("Installed the {} skill.", done["installed"].as_str().unwrap_or("new")),
                            Err(error) => format!("Could not install the skill: {error}"),
                        };
                        let _ = tx.send(Ok(CompletedOperation::McpUpdated(message)));
                    },
                    "Installing the skill from GitHub…",
                );
            }
        });
        ui.add_space(14.0);
    }


    pub(super) fn agents_settings_ui(&mut self, ui: &mut egui::Ui) {
        self.skills_settings_ui(ui);
        settings_group(ui, Some("Subagents"), |ui| {
            let current = if self.subagent_model.is_empty() {
                "Same as the main model".to_owned()
            } else {
                self.model_list.iter().find(|m| m.slug == self.subagent_model).map(|m| m.display_name.clone()).unwrap_or_else(|| self.subagent_model.clone())
            };
            settings_row(ui, None, "Model for subagents", "Workers started by Rhea use this model unless told otherwise.", |ui| {
                egui::ComboBox::from_id_salt("subagent-model").selected_text(current).show_ui(ui, |ui| {
                    if ui.selectable_label(self.subagent_model.is_empty(), "Same as the main model").clicked() {
                        self.subagent_model.clear();
                        let _ = save_subagent_model("");
                    }
                    for model in self.model_list.clone() {
                        if ui.selectable_label(self.subagent_model == model.slug, &model.display_name).clicked() {
                            self.subagent_model = model.slug.clone();
                            if let Err(error) = save_subagent_model(&model.slug) {
                                self.notice = error;
                            }
                        }
                    }
                });
            });
        });
        let hook_count = self.hooks.read().map(|hooks| hooks.count()).unwrap_or(0);
        settings_group(ui, Some("Hooks"), |ui| {
            settings_row(
                ui,
                Some(if hook_count > 0 { HOVER_GREEN } else { MUTED }),
                &format!("{hook_count} hook{} active", if hook_count == 1 { "" } else { "s" }),
                &chat_store::tilde(&crate::hooks::default_path()),
                |ui| {
                    if action_button(ui, "refresh", "Reload", false).clicked() {
                        match HookRegistry::load(&crate::hooks::default_path()) {
                            Ok(registry) => {
                                self.hooks_draft = registry.to_pretty_json();
                                if let Ok(mut hooks) = self.hooks.write() {
                                    *hooks = registry;
                                }
                                self.hooks_message = "Reloaded hooks from disk.".into();
                            }
                            Err(error) => self.hooks_message = error,
                        }
                    }
                },
            );
        });
        if self.hooks_draft.is_empty() {
            self.hooks_draft = self.hooks.read().map(|hooks| hooks.to_pretty_json()).unwrap_or_default();
        }
        ui.label(egui::RichText::new("Hook scripts run on PreToolUse, PostToolUse, UserPromptSubmit, SessionStart, Stop, SubagentStart and SubagentStop. Exit code 2 blocks; JSON on stdout can add context or change tool input.").size(11.0).color(MUTED));
        ui.add_space(6.0);
        egui::Frame::new().fill(Color32::from_rgb(13, 14, 14)).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(10)).show(ui, |ui| {
            ui.add(egui::TextEdit::multiline(&mut self.hooks_draft).code_editor().desired_rows(9).desired_width(f32::INFINITY).frame(egui::Frame::NONE));
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if action_button(ui, "check", "Save and reload", true).clicked() {
                match HookRegistry::from_json(&self.hooks_draft).and_then(|registry| registry.save(&crate::hooks::default_path()).map(|()| registry)) {
                    Ok(registry) => {
                        self.hooks_message = format!("Saved {} hook(s); active immediately.", registry.count());
                        if let Ok(mut hooks) = self.hooks.write() {
                            *hooks = registry;
                        }
                    }
                    Err(error) => self.hooks_message = error,
                }
            }
        });
        if !self.hooks_message.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(&self.hooks_message).size(11.0).color(MUTED));
        }
    }
}

#[cfg(test)]
mod avatar_tests {
    use super::*;

    #[test]
    fn avatars_are_deterministic_symmetric_and_two_colored() {
        for seed in ["a1:alpha", "a2:beta", "a3:gamma", "a9:Research", "", "x"] {
            let cells = avatar_cells(seed);
            assert_eq!(cells, avatar_cells(seed));
            let mut colors = std::collections::HashSet::new();
            for row in cells {
                assert_eq!(row[0], row[4]);
                assert_eq!(row[1], row[3]);
                colors.extend(row);
            }
            assert_eq!(colors.len(), 2, "{seed} should use exactly two complementary colors");
        }
    }

    #[test]
    fn different_agents_look_different_and_generation_is_fast() {
        let seeds: Vec<String> = (0..200).map(|i| format!("a{i}:agent{i}")).collect();
        let started = std::time::Instant::now();
        let patterns: std::collections::HashSet<_> = seeds.iter().map(|seed| format!("{:?}", avatar_cells(seed))).collect();
        assert!(started.elapsed() < Duration::from_millis(200), "200 avatars took {:?}", started.elapsed());
        assert!(patterns.len() > 180, "only {} distinct avatars", patterns.len());
    }

    #[test]
    fn the_two_colors_are_complementary_hues() {
        let cells = avatar_cells("a1:alpha");
        let mut distinct: Vec<Color32> = cells.iter().flatten().copied().collect();
        distinct.dedup();
        let unique: std::collections::BTreeSet<[u8; 3]> = cells.iter().flatten().map(|c| [c.r(), c.g(), c.b()]).collect();
        let colors: Vec<[u8; 3]> = unique.into_iter().collect();
        assert_eq!(colors.len(), 2);
        let hue = |c: [u8; 3]| {
            let (r, g, b) = (c[0] as f32, c[1] as f32, c[2] as f32);
            let (max, min) = (r.max(g).max(b), r.min(g).min(b));
            let d = max - min;
            let h = if d == 0.0 { 0.0 } else if max == r { ((g - b) / d).rem_euclid(6.0) } else if max == g { (b - r) / d + 2.0 } else { (r - g) / d + 4.0 };
            h * 60.0
        };
        let gap = (hue(colors[0]) - hue(colors[1])).abs();
        let gap = gap.min(360.0 - gap);
        assert!((gap - 180.0).abs() < 6.0, "hue gap was {gap}");
    }
}
