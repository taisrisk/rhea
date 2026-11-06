//! Settings > Local model: installing, starting and testing Laya, Rhea's fast local decision model.

use super::*;
use crate::laya;

impl RheaApp {
    pub(super) fn laya_settings_ui(&mut self, ui: &mut egui::Ui) {
        let status = self.laya_status.lock().map(|status| status.clone()).unwrap_or_default();
        let progress = laya::progress();
        if progress.running || status.running {
            ui.ctx().request_repaint_after(Duration::from_millis(400));
        }
        settings_group(ui, Some("Laya · local decision model"), |ui| {
            let (dot, title, detail) = if progress.running {
                (MINT_DEEP, "Installing…", format!("{} · {}", progress.step, progress.detail))
            } else if status.running {
                (HOVER_GREEN, "Running", format!("Answering on 127.0.0.1:{} (this computer only)", status.port.unwrap_or_default()))
            } else if status.installed() {
                (MUTED, "Installed", "Starts by itself the first time Rhea needs it, and stops when Rhea closes.".to_owned())
            } else {
                (DANGER, "Not installed", format!("One click downloads everything it needs: {}. Nothing else has to be installed.", laya::DOWNLOAD_ESTIMATE))
            };
            settings_row(ui, Some(dot), title, &detail, |ui| {
                if progress.running {
                    return;
                }
                if !status.installed() {
                    if action_button(ui, "download", if progress.error.is_some() { "Try again" } else { "Install" }, true).clicked() {
                        std::thread::spawn(|| {
                            let _ = laya::install();
                        });
                    }
                } else if status.running {
                    if action_button(ui, "cancel", "Stop", false).clicked() {
                        std::thread::spawn(laya::stop);
                    }
                    if action_button(ui, "check", "Test", false).clicked() {
                        std::thread::spawn(|| laya::set_note(laya::self_test()));
                    }
                } else if action_button(ui, "bolt", "Start", true).clicked() {
                    std::thread::spawn(|| {
                        laya::set_note(match laya::start() {
                            Ok(port) => format!("Laya is running on port {port}."),
                            Err(error) => error,
                        })
                    });
                }
            });
            if progress.running {
                ui.add_space(4.0);
                let (bar, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::hover());
                ui.painter().rect_filled(bar, CornerRadius::same(4), MINT);
                let shown = ui.ctx().animate_value_with_time(ui.make_persistent_id("laya-progress"), progress.fraction.unwrap_or(0.0), 0.3);
                if progress.fraction.is_some() {
                    ui.painter().rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width() * shown.clamp(0.02, 1.0), bar.height())), CornerRadius::same(4), HOVER_GREEN);
                } else {
                    // No size known (installing packages): a bar that sweeps back and forth.
                    let t = (ui.input(|input| input.time) as f32 * 0.9).fract();
                    let width = bar.width() * 0.25;
                    let x = bar.left() + (bar.width() - width) * (0.5 - 0.5 * (t * std::f32::consts::TAU).cos());
                    ui.painter().rect_filled(Rect::from_min_size(pos2(x, bar.top()), Vec2::new(width, bar.height())), CornerRadius::same(4), HOVER_GREEN);
                }
                ui.add_space(4.0);
            }
            if let Some(error) = progress.error.as_ref().filter(|_| !progress.running) {
                row_divider(ui);
                settings_row(ui, Some(DANGER), "Install did not finish", &short_error(error), |_| {});
            }
            row_divider(ui);
            step_status(ui, "Private Python", "Its own copy under ~/.rhea/laya; your own Python is never touched.", status.python);
            row_divider(ui);
            step_status(ui, "Laya runtime", "The laya package and PyTorch.", status.package);
            row_divider(ui);
            step_status(ui, "Model", "convaiinnovations/laya from Hugging Face, kept in ~/.rhea/laya.", status.model);
            row_divider(ui);
            settings_row(ui, None, "Folder", &chat_store::tilde(&laya::root()), |ui| {
                if action_button(ui, "folder", "Open", false).clicked() {
                    self.notice = open_local_path(laya::root());
                }
            });
        });
        let note = laya::note();
        if !note.is_empty() {
            ui.add_space(6.0);
            ui.add(egui::Label::new(egui::RichText::new(note).size(11.0).color(MUTED)).wrap());
        }
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new("Rhea uses Laya for quick local decisions, such as picking the right control when several look alike, and ChatGPT can ask it directly with the laya_decide tool. Nothing is sent to a server.")
                    .size(10.5)
                    .color(MUTED),
            )
            .wrap(),
        );
    }
}

fn step_status(ui: &mut egui::Ui, title: &str, detail: &str, done: bool) {
    settings_row(ui, Some(if done { HOVER_GREEN } else { MUTED }), title, detail, |ui| {
        ui.label(egui::RichText::new(if done { "Ready" } else { "Missing" }).size(11.5).color(if done { HOVER_GREEN } else { MUTED }));
    });
}
