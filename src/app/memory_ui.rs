//! Settings > Memory: switch long-term memory on or off, see what Rhea remembers, correct or
//! delete it, import existing chats, and fetch the better embedding model.

use super::*;
use crate::memory::{self, embed, store::{Filter, Memory as Record}};

/// A long task running in the background (importing chats, downloading the model, re-indexing).
#[derive(Default, Clone)]
pub(super) struct MemoryJob {
    pub running: bool,
    pub message: String,
    pub fraction: Option<f32>,
}

pub(super) struct MemoryView {
    pub search: String,
    pub kind: String,
    pub items: Vec<Record>,
    pub total: i64,
    pub counts: Vec<(String, i64)>,
    pub loaded_at: Option<Instant>,
    pub editing: Option<(i64, String)>,
    pub pending_delete: Option<i64>,
    pub pending_clear: bool,
}

impl Default for MemoryView {
    fn default() -> Self {
        Self { search: String::new(), kind: String::new(), items: Vec::new(), total: 0, counts: Vec::new(), loaded_at: None, editing: None, pending_delete: None, pending_clear: false }
    }
}

const KINDS: [(&str, &str); 9] = [
    ("", "All"),
    ("preference", "Preferences"),
    ("instruction", "Instructions"),
    ("fact", "Facts"),
    ("decision", "Decisions"),
    ("outcome", "Outcomes"),
    ("failure", "Failures"),
    ("workflow", "Workflows"),
    ("task", "Unfinished"),
];

pub fn load_memory_enabled() -> Option<bool> {
    settings_values()?.get("memory")?.as_bool()
}

fn save_memory_enabled(on: bool) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "Could not locate your home folder.".to_owned())?;
    let mut settings = settings_values().filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    settings["memory"] = Value::Bool(on);
    chat_store::write_json_atomic(&path, &settings).map_err(|error| format!("Could not save the memory setting: {error}"))
}

impl RheaApp {
    fn refresh_memory_view(&mut self) {
        let Some(memory) = memory::handle() else { return };
        let filter = Filter {
            kinds: if self.memory_view.kind.is_empty() { Vec::new() } else { vec![self.memory_view.kind.clone()] },
            contains: Some(self.memory_view.search.trim().to_owned()).filter(|text| !text.is_empty()),
            ..Default::default()
        };
        self.memory_view.items = memory.list(&filter, 60, 0);
        let (total, counts) = memory.counts();
        self.memory_view.total = total;
        self.memory_view.counts = counts;
        self.memory_view.loaded_at = Some(Instant::now());
    }

    fn start_memory_job(&self, label: &str, work: impl FnOnce(Arc<Mutex<MemoryJob>>) -> String + Send + 'static) {
        let job = Arc::clone(&self.memory_job);
        if job.lock().map(|j| j.running).unwrap_or(true) {
            return;
        }
        if let Ok(mut state) = job.lock() {
            *state = MemoryJob { running: true, message: label.to_owned(), fraction: None };
        }
        thread::spawn(move || {
            let message = work(Arc::clone(&job));
            if let Ok(mut state) = job.lock() {
                *state = MemoryJob { running: false, message, fraction: None };
            }
        });
    }

    pub(super) fn memory_settings_ui(&mut self, ui: &mut egui::Ui) {
        let job = self.memory_job.lock().map(|job| job.clone()).unwrap_or_default();
        if job.running {
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
        // Re-read the list when it is stale (new memories arrive in the background) or a job ends.
        let stale = self.memory_view.loaded_at.is_none_or(|at| at.elapsed() > Duration::from_secs(3));
        if stale {
            self.refresh_memory_view();
        }
        ui.ctx().request_repaint_after(Duration::from_secs(3));

        settings_group(ui, Some("Long-term memory"), |ui| {
            let mut on = memory::enabled();
            settings_row(
                ui,
                Some(if on { HOVER_GREEN } else { MUTED }),
                "Remember across chats",
                "Rhea keeps what you tell it (preferences, instructions, decisions) and what it actually did (verified results and failures), and brings the relevant part back before each message. Everything stays on this computer.",
                |ui| {
                    if toggle_switch(ui, &mut on).changed() {
                        memory::set_enabled(on);
                        if let Err(error) = save_memory_enabled(on) {
                            self.notice = error;
                        }
                    }
                },
            );
            row_divider(ui);
            let (summary, tip) = (format!("{} remembered", self.memory_view.total), memory::db_path());
            settings_row(ui, None, "Memory", &format!("{summary} · {}", chat_store::tilde(&tip)), |ui| {
                let busy = job.running;
                if ui.add_enabled_ui(!busy, |ui| action_button(ui, "download", "Index my chats", false)).inner.clicked() {
                    self.start_memory_job("Reading your existing chats…", |job| {
                        let added = memory::import_existing_chats(|done, total| {
                            if let Ok(mut state) = job.lock() {
                                state.fraction = Some(done as f32 / total.max(1) as f32);
                            }
                        });
                        format!("Indexed your chats: {added} memories added.")
                    });
                }
                if ui.add_enabled_ui(!busy, |ui| action_button(ui, "copy", "Export", false)).inner.clicked() {
                    if let Some(handle) = memory::handle() {
                        let path = downloads_directory().unwrap_or_else(std::env::temp_dir).join("rhea-memory.json");
                        self.notice = match fs::write(&path, serde_json::to_vec_pretty(&handle.export_json()).unwrap_or_default()) {
                            Ok(()) => format!("Exported to {}.", chat_store::tilde(&path)),
                            Err(error) => format!("Could not export: {error}"),
                        };
                    }
                }
            });
            row_divider(ui);
            let installed = embed::model_installed();
            let active = embed::active().name().to_owned();
            settings_row(
                ui,
                Some(if installed { HOVER_GREEN } else { MUTED }),
                "Matching model",
                &if installed {
                    format!("Using {active}: a sentence model that runs on this computer and scores how closely each memory matches (80% and up is brought along automatically).")
                } else {
                    format!("Using the built-in matcher ({active}). Download the {} model {} for real understanding of meaning (cheap = inexpensive) and relatedness scores.", embed::MODEL_NAME, embed::MODEL_SIZE)
                },
                |ui| {
                    if !installed && ui.add_enabled_ui(!job.running, |ui| action_button(ui, "download", "Download model", true)).inner.clicked() {
                        self.start_memory_job("Downloading the matching model…", |job| match embed::download_model(|fraction| {
                            if let Ok(mut state) = job.lock() {
                                state.fraction = Some(fraction);
                            }
                        }) {
                            Ok(()) => {
                                let name = embed::reload();
                                if let Ok(mut state) = job.lock() {
                                    state.message = format!("Re-indexing memories with {name}…");
                                }
                                if let Some(handle) = memory::handle() {
                                    while handle.reindex(200) > 0 {}
                                }
                                format!("Now using {name}.")
                            }
                            Err(error) => error,
                        });
                    }
                },
            );
            if job.running {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&job.message).size(11.0).color(MUTED));
                let (bar, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
                ui.painter().rect_filled(bar, CornerRadius::same(3), MINT);
                let shown = ui.ctx().animate_value_with_time(ui.make_persistent_id("memory-progress"), job.fraction.unwrap_or(0.0), 0.3);
                ui.painter().rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width() * shown.clamp(0.02, 1.0), bar.height())), CornerRadius::same(3), HOVER_GREEN);
                ui.add_space(4.0);
            }
        });
        if !job.running && !job.message.is_empty() {
            ui.add_space(4.0);
            ui.add(egui::Label::new(egui::RichText::new(&job.message).size(11.0).color(MUTED)).wrap());
        }
        ui.add_space(12.0);

        // Filters
        ui.horizontal_wrapped(|ui| {
            for (key, label) in KINDS {
                let count = if key.is_empty() { self.memory_view.total } else { self.memory_view.counts.iter().find(|(kind, _)| kind == key).map(|(_, n)| *n).unwrap_or(0) };
                let selected = self.memory_view.kind == key;
                let text = egui::RichText::new(format!("{label} {count}")).size(11.0).color(if selected { INK } else { MUTED });
                if ui.add(egui::Button::new(text).fill(if selected { MINT } else { Color32::TRANSPARENT }).corner_radius(CornerRadius::same(12))).clicked() {
                    self.memory_view.kind = key.to_owned();
                    self.memory_view.loaded_at = None;
                }
            }
        });
        ui.add_space(6.0);
        let before = self.memory_view.search.clone();
        ui.add(egui::TextEdit::singleline(&mut self.memory_view.search).desired_width(f32::INFINITY).hint_text(egui::RichText::new("Search what Rhea remembers").color(MUTED)));
        if before != self.memory_view.search {
            self.memory_view.loaded_at = None;
        }
        ui.add_space(8.0);

        let items = self.memory_view.items.clone();
        settings_group(ui, None, |ui| {
            if items.is_empty() {
                settings_row(ui, Some(MUTED), "Nothing here yet", "As you chat, preferences, decisions and results will appear. Use \"Index my chats\" to start from your history.", |_| {});
            }
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    row_divider(ui);
                }
                self.memory_row(ui, item);
            }
        });
        ui.add_space(10.0);
        let armed = self.memory_view.pending_clear;
        if action_button_colored(ui, "trash", if armed { "Click again to erase all memory" } else { "Erase all memory" }, DANGER).clicked() {
            if armed {
                if let Some(handle) = memory::handle() {
                    handle.clear();
                }
                self.memory_view.pending_clear = false;
                self.memory_view.loaded_at = None;
            } else {
                self.memory_view.pending_clear = true;
            }
        }
    }

    fn memory_row(&mut self, ui: &mut egui::Ui, item: &Record) {
        let editing = self.memory_view.editing.as_ref().is_some_and(|(id, _)| *id == item.id);
        let mut tags = vec![item.kind.clone(), memory::age_label(item.created_at)];
        tags.push(format!("importance {}%", ((item.importance * item.weight).clamp(0.0, 1.0) * 100.0).round() as i64));
        if item.verified {
            tags.push("verified".into());
        }
        if let Some(project) = item.scope.strip_prefix("project:") {
            tags.push(project.to_owned());
        }
        if editing {
            ui.vertical(|ui| {
                if let Some((_, draft)) = self.memory_view.editing.as_mut() {
                    ui.add(egui::TextEdit::multiline(draft).desired_rows(3).desired_width(f32::INFINITY));
                }
                ui.horizontal(|ui| {
                    if action_button(ui, "check", "Save", true).clicked() {
                        if let (Some((id, text)), Some(handle)) = (self.memory_view.editing.take(), memory::handle()) {
                            handle.correct(id, &text);
                            self.memory_view.loaded_at = None;
                        }
                    }
                    if action_button(ui, "cancel", "Cancel", false).clicked() {
                        self.memory_view.editing = None;
                    }
                });
            });
            return;
        }
        settings_row(ui, None, &memory_title(&item.text), &tags.join(" · "), |ui| {
            let armed = self.memory_view.pending_delete == Some(item.id);
            if action_button_colored(ui, "trash", if armed { "Click to delete" } else { "Delete" }, DANGER).clicked() {
                if armed {
                    if let Some(handle) = memory::handle() {
                        handle.forget(item.id);
                    }
                    self.memory_view.pending_delete = None;
                    self.memory_view.loaded_at = None;
                } else {
                    self.memory_view.pending_delete = Some(item.id);
                }
            }
            if action_button(ui, "edit", "Edit", false).clicked() {
                self.memory_view.editing = Some((item.id, item.text.clone()));
            }
        });
    }
}

/// A memory's text as a row title: one line, cut short.
fn memory_title(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 150 {
        format!("{}…", flat.chars().take(149).collect::<String>())
    } else {
        flat
    }
}
