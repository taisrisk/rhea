//! Settings > Browser: the Chrome extension's one-click install and live status.

use super::*;
use crate::chrome_ext;

impl RheaApp {
    pub(super) fn browser_settings_ui(&mut self, ui: &mut egui::Ui) {
        let status = self.chrome_status.lock().map(|status| status.clone()).unwrap_or_default();
        let folder = chrome_ext::folder();
        // Poll while the page is open and things are still settling.
        if !status.working() {
            ui.ctx().request_repaint_after(Duration::from_secs(2));
        }
        settings_group(ui, Some("Chrome extension"), |ui| {
            let (dot, title, detail) = overall(&status, &folder);
            settings_row(ui, Some(dot), title, &detail, |ui| {
                if status.working() {
                    self.test_and_disconnect_buttons(ui);
                } else {
                    let label = if status.files_ready || status.in_chrome() { "Reinstall" } else { "Install extension" };
                    let enabled = self.pending.is_none();
                    if ui.add_enabled_ui(enabled, |ui| action_button(ui, "download", label, true)).inner.clicked() {
                        ui.ctx().copy_text(folder.display().to_string());
                        self.begin(
                            |tx| {
                                let _ = tx.send(chrome_ext::install().map(CompletedOperation::McpUpdated));
                            },
                            "Downloading the Chrome extension…",
                        );
                    }
                }
            });
            row_divider(ui);
            step_row(ui, "1", "Extension files", status.files_ready, &match &status.version {
                Some(version) => format!("Version {version} in {}", chat_store::tilde(&folder)),
                None => format!("Will be saved to {}", chat_store::tilde(&folder)),
            });
            row_divider(ui);
            step_row(ui, "2", "Added to Chrome", status.in_chrome() && !status.points_elsewhere(), &if !status.chrome_found {
                "Google Chrome was not found on this computer.".to_owned()
            } else if status.in_chrome() {
                "Chrome has the extension.".to_owned()
            } else {
                "Chrome does not let any program add an unpacked extension for you. In chrome://extensions switch on Developer mode, choose Load unpacked, then paste the folder path (it is copied for you).".to_owned()
            });
            row_divider(ui);
            step_row(ui, "3", "Connected to Rhea", status.connected, if status.connected { "Chrome tabs are available to Rhea." } else { "Open Chrome with the extension on; this turns green by itself." });
            row_divider(ui);
            settings_row(ui, None, "Folder", &chat_store::tilde(&folder), |ui| {
                if action_button(ui, "folder", "Open", false).clicked() {
                    self.notice = open_local_path(folder.clone());
                }
                if action_button(ui, "copy", "Copy path", false).clicked() {
                    ui.ctx().copy_text(folder.display().to_string());
                    self.notice = "Folder path copied.".into();
                }
                if action_button(ui, "globe", "GitHub", false).clicked() {
                    self.notice = chrome_ext::open_github_page().map(|()| "Opened the extension's GitHub page.".to_owned()).unwrap_or_else(|error| error);
                }
                if status.chrome_found && action_button(ui, "globe", "Extensions page", false).clicked() {
                    ui.ctx().copy_text(folder.display().to_string());
                    self.notice = chrome_ext::open_extensions_page().map(|()| "Opened chrome://extensions · the folder path is copied.".to_owned()).unwrap_or_else(|error| error);
                }
            });
            let profiles = chrome_ext::all_profiles();
            if status.chrome_found && profiles.len() > 1 {
                row_divider(ui);
                settings_row(
                    ui,
                    None,
                    "Every Chrome profile",
                    &format!("You have {} Chrome profiles. An extension is per-profile in Chrome, so each needs its own Load unpacked click.", profiles.len()),
                    |ui| {
                        if action_button(ui, "globe", "Open all", false).clicked() {
                            ui.ctx().copy_text(folder.display().to_string());
                            self.notice = chrome_ext::open_extensions_in_every_profile()
                                .map(|count| format!("Opened chrome://extensions in {count} profiles · the folder path is copied. Load unpacked once in each."))
                                .unwrap_or_else(|error| error);
                        }
                    },
                );
            }
        });
        ui.add(egui::Label::new(egui::RichText::new("Rhea acts only on tabs the extension can see, and never reads saved passwords or cookies.").size(10.5).color(MUTED)).wrap());
    }

    fn test_and_disconnect_buttons(&mut self, ui: &mut egui::Ui) {
        let enabled = self.pending.is_none();
        if ui.add_enabled_ui(enabled, |ui| action_button(ui, "cancel", "Disconnect", false)).inner.clicked() {
            self.begin(
                |tx| {
                    let result = browser::stop_browser_relay()
                        .map(|()| CompletedOperation::McpUpdated("Chrome extension disconnected.".into()))
                        .map_err(|error| error.to_string());
                    let _ = tx.send(result);
                },
                "Disconnecting the Chrome extension…",
            );
        }
        if ui.add_enabled_ui(enabled, |ui| action_button(ui, "check", "Test", false)).inner.clicked() {
            self.begin(
                |tx| {
                    let result = browser::execute_browser_command("snapshot", serde_json::json!({})).map(CompletedOperation::BrowserSnapshot).map_err(|error| error.to_string());
                    let _ = tx.send(result);
                },
                "Reading the active Chrome tab…",
            );
        }
    }
}

/// The one-line verdict: colour, title and what to do next.
fn overall(status: &chrome_ext::Status, folder: &std::path::Path) -> (Color32, &'static str, String) {
    if status.working() {
        return (HOVER_GREEN, "Working", "Connected · Rhea can use your Chrome tabs.".into());
    }
    if status.in_chrome() && status.points_elsewhere() {
        let from = status.entry.as_ref().and_then(|entry| entry.path.clone()).unwrap_or_default();
        return (DANGER, "Chrome loads a different copy", format!("Chrome loads it from {from}. Remove that one in chrome://extensions, then load {}.", chat_store::tilde(folder)));
    }
    if status.in_chrome() && status.entry.as_ref().is_some_and(|entry| !entry.enabled) {
        return (DANGER, "Turned off in Chrome", "Switch it on in chrome://extensions.".into());
    }
    if status.in_chrome() {
        return (MUTED, "Added to Chrome", "Waiting for the connection: open Chrome, or reload the extension in chrome://extensions.".into());
    }
    if status.files_ready {
        return (MUTED, "Downloaded, not added to Chrome yet", "Finish in Chrome: Developer mode, Load unpacked, paste the folder path.".into());
    }
    (DANGER, "Not installed", "One click downloads the extension and opens Chrome's extensions page.".into())
}

fn step_row(ui: &mut egui::Ui, number: &str, title: &str, done: bool, detail: &str) {
    let dot = if done { HOVER_GREEN } else { MUTED };
    settings_row(ui, Some(dot), &format!("{number}. {title}"), detail, |ui| {
        ui.label(egui::RichText::new(if done { "Done" } else { "Waiting" }).size(11.5).color(if done { HOVER_GREEN } else { MUTED }));
    });
}
