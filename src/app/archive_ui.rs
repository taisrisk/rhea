//! Settings > Archived chats: every chat moved to `Archives`, with a small preview popup.

use super::*;

/// One archived chat as shown in the list.
#[derive(Clone)]
pub(super) struct ArchivedChat {
    id: String,
    title: String,
    count: usize,
    preview: String,
}

/// The opened preview: the chat's messages, read when the popup opens.
pub(super) struct ArchivePreview {
    id: String,
    title: String,
    turns: Vec<(String, String)>,
}

impl RheaApp {
    pub(super) fn refresh_archived(&mut self) {
        self.archived = chat_store::archived_ids()
            .into_iter()
            .filter_map(|id| {
                let metadata = chat_store::read_archived_file::<Conversation>(&id, "metadata.json").ok().flatten()?;
                Some(ArchivedChat {
                    title: if metadata.title.trim().is_empty() { "New chat".into() } else { metadata.title.clone() },
                    count: metadata.message_count,
                    preview: metadata.preview.clone(),
                    id,
                })
            })
            .collect();
        self.archived_loaded = true;
    }

    fn open_archive_preview(&mut self, chat: &ArchivedChat) {
        let history = chat_store::read_archived_file::<StoredHistory>(&chat.id, "history.json").ok().flatten().unwrap_or_default();
        self.archive_preview = Some(ArchivePreview {
            id: chat.id.clone(),
            title: chat.title.clone(),
            turns: history.turns.into_iter().map(|turn| (turn.role, turn.content)).collect(),
        });
    }

    fn restore_archived(&mut self, id: &str) {
        match chat_store::restore_chat(id) {
            Ok(()) => {
                if let Some(mut conversation) = chat_store::read_chat_file::<Conversation>(id, "metadata.json").ok().flatten() {
                    conversation.loaded = false;
                    conversation.turns.clear();
                    if !self.conversations.iter().any(|chat| chat.id == conversation.id) {
                        // Keep the active chat where it is while the list grows above it.
                        self.conversations.push(conversation);
                        let active_id = self.conversations.get(self.active_conversation).map(|chat| chat.id.clone());
                        self.conversations.sort_by(|a, b| conversation_created_ns(&b.id).cmp(&conversation_created_ns(&a.id)));
                        if let Some(active_id) = active_id {
                            self.active_conversation = self.conversations.iter().position(|chat| chat.id == active_id).unwrap_or(0);
                        }
                    }
                }
                self.history_dirty = true;
                self.flush_history(true);
                self.notice = "Chat restored to your history.".into();
            }
            Err(error) => self.notice = format!("Could not restore the chat: {error}"),
        }
        self.archive_preview = None;
        self.refresh_archived();
    }

    pub(super) fn archived_settings_ui(&mut self, ui: &mut egui::Ui) {
        if !self.archived_loaded {
            self.refresh_archived();
        }
        let chats = self.archived.clone();
        let location = chat_store::archive_root().map(|path| chat_store::tilde(&path)).unwrap_or_default();
        settings_group(ui, Some(&format!("{} archived", chats.len())), |ui| {
            if chats.is_empty() {
                settings_row(ui, Some(MUTED), "Nothing archived", "Use the … menu on a chat in the history and choose Archive chat.", |_| {});
            }
            for (position, chat) in chats.iter().enumerate() {
                if position > 0 {
                    row_divider(ui);
                }
                let detail = if chat.preview.is_empty() { format!("{} messages", chat.count) } else { format!("{} · {}", chat.count, chat.preview) };
                settings_row(ui, None, &chat.title, &detail, |ui| {
                    let armed = self.pending_archive_delete.as_deref() == Some(chat.id.as_str());
                    if action_button_colored(ui, "trash", if armed { "Click to delete" } else { "Delete" }, DANGER).clicked() {
                        if armed {
                            let _ = chat_store::delete_archived_chat(&chat.id);
                            self.pending_archive_delete = None;
                            self.refresh_archived();
                        } else {
                            self.pending_archive_delete = Some(chat.id.clone());
                        }
                    }
                    if action_button(ui, "restore", "Restore", false).clicked() {
                        self.restore_archived(&chat.id);
                    }
                    if action_button(ui, "eye", "Preview", false).clicked() {
                        self.open_archive_preview(chat);
                    }
                });
            }
        });
        if !location.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(format!("Archived chats are kept in {location}. Nothing in there is sent anywhere.")).size(10.5).color(MUTED));
        }
    }

    /// The preview popup, drawn above the settings window.
    pub(super) fn archive_preview_window(&mut self, ctx: &egui::Context) {
        let Some(preview) = self.archive_preview.as_ref() else { return };
        let (id, title) = (preview.id.clone(), preview.title.clone());
        let mut open = true;
        let mut restore = false;
        egui::Window::new(egui::RichText::new(format!("Archived · {title}")).size(13.0).color(INK))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 440.0])
            .order(egui::Order::Foreground)
            .frame(egui::Frame::new().inner_margin(Margin::same(14)).fill(SURFACE).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(14)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if action_button(ui, "restore", "Restore to history", true).clicked() {
                        restore = true;
                    }
                });
                ui.add_space(8.0);
                egui::ScrollArea::vertical().id_salt("archive-preview-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    let turns = self.archive_preview.as_ref().map(|preview| preview.turns.clone()).unwrap_or_default();
                    if turns.is_empty() {
                        ui.label(egui::RichText::new("This chat has no saved messages.").size(12.0).color(MUTED));
                    }
                    for (role, content) in turns {
                        let you = role == "user";
                        ui.label(egui::RichText::new(if you { "You" } else { "Rhea" }).size(10.5).strong().color(if you { HOVER_GREEN } else { MUTED }));
                        let shown: String = content.chars().take(1500).collect();
                        let cut = content.chars().count() > 1500;
                        ui.add(egui::Label::new(egui::RichText::new(if cut { format!("{shown}…") } else { shown }).size(12.0).color(INK)).wrap());
                        ui.add_space(10.0);
                    }
                });
            });
        if restore {
            self.restore_archived(&id);
        } else if !open {
            self.archive_preview = None;
        }
    }
}
