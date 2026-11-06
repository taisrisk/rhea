//! Settings > Chat & model > Updates: look for a newer release and install it.

use super::*;
use crate::installer::{self, Release};

#[derive(Clone, Default)]
pub(super) enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing,
    Failed(String),
}

impl RheaApp {
    pub(super) fn updates_ui(&mut self, ui: &mut egui::Ui) {
        let state = self.update.lock().map(|state| state.clone()).unwrap_or_default();
        if matches!(state, UpdateState::Checking | UpdateState::Installing) {
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
        settings_group(ui, Some("Updates"), |ui| {
            let (dot, detail) = match &state {
                UpdateState::Idle => (MUTED, format!("Rhea {}", installer::VERSION)),
                UpdateState::Checking => (MINT_DEEP, "Looking for a newer release…".to_owned()),
                UpdateState::UpToDate => (HOVER_GREEN, format!("Rhea {} is the newest release.", installer::VERSION)),
                UpdateState::Available(release) => (HOVER_GREEN, format!("Version {} is available (you have {}).", release.version, installer::VERSION)),
                UpdateState::Installing => (MINT_DEEP, "Downloading the update… Rhea restarts when it is ready.".to_owned()),
                UpdateState::Failed(error) => (DANGER, short_error(error)),
            };
            settings_row(ui, Some(dot), "Version", &detail, |ui| match &state {
                UpdateState::Available(release) => {
                    let label = if release.asset.is_some() { "Install update" } else { "Open release" };
                    if action_button(ui, "download", label, true).clicked() {
                        match release.asset {
                            Some(_) => self.install_update(release.clone(), ui.ctx().clone()),
                            None => {
                                let _ = webbrowser::open(&release.page);
                            }
                        }
                    }
                }
                UpdateState::Checking | UpdateState::Installing => {}
                _ => {
                    if action_button(ui, "refresh", "Check for updates", false).clicked() {
                        self.check_for_update();
                    }
                }
            });
        });
    }

    fn check_for_update(&mut self) {
        let shared = Arc::clone(&self.update);
        if let Ok(mut state) = shared.lock() {
            *state = UpdateState::Checking;
        }
        thread::spawn(move || {
            let result = match installer::check_for_update() {
                Ok(Some(release)) => UpdateState::Available(release),
                Ok(None) => UpdateState::UpToDate,
                Err(error) => UpdateState::Failed(error),
            };
            if let Ok(mut state) = shared.lock() {
                *state = result;
            }
        });
    }

    /// Download the installer and run it; it replaces this install, so this copy closes.
    fn install_update(&mut self, release: Release, ctx: egui::Context) {
        let shared = Arc::clone(&self.update);
        if let Ok(mut state) = shared.lock() {
            *state = UpdateState::Installing;
        }
        thread::spawn(move || match installer::download_and_run(&release) {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(error) => {
                if let Ok(mut state) = shared.lock() {
                    *state = UpdateState::Failed(error);
                }
            }
        });
    }
}
