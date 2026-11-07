#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod auth;
mod browser;
mod chat;
mod chat_store;
mod chrome_ext;
mod compose;
mod desktop;
mod hooks;
mod installer;
mod jev;
mod laya;
mod ledger;
#[cfg(target_os = "macos")]
mod macos;
mod mcp;
mod opencode;
mod research;
#[cfg(test)]
mod replay;
mod memory;
mod shell;
mod skills;
mod spelling;
mod team;
mod tools;
mod uia;
mod usage;

use std::sync::Arc;

fn main() {
    // Data from a version that still called its folder ~/.rhea moves to ~/.rhea first.
    chat_store::migrate_legacy_data();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|argument| {
        argument == "--native-messaging-host" || argument.starts_with("chrome-extension://")
    }) {
        if let Err(error) = browser::run_native_messaging_host() {
            eprintln!("Rhea Chrome bridge failed: {error}");
        }
        return;
    }
    // The downloaded exe installs itself (and replaces an older install) before anything opens.
    if installer::run_install_step() == installer::Startup::Exit {
        return;
    }
    // An app opened from Finder gets a bare PATH; give child processes (MCP servers, Agent
    // Desktop, the shell tool's helpers) the PATH a Terminal would have.
    #[cfg(target_os = "macos")]
    macos::extend_path();
    if let Err(error) = run_gui() {
        show_startup_error(&error.to_string());
    }
}

#[cfg(target_os = "windows")]
fn show_startup_error(error: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let message: Vec<u16> = OsStr::new(error).encode_wide().chain(Some(0)).collect();
    let title: Vec<u16> = OsStr::new("Rhea could not start")
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn show_startup_error(error: &str) {
    eprintln!("Rhea could not start: {error}");
}

fn run_gui() -> eframe::Result {
    let icon_image = image::load_from_memory(include_bytes!("../assets/rhea-mark.png"))
        .expect("embedded Rhea icon must be a valid PNG")
        .into_rgba8();
    let (icon_width, icon_height) = icon_image.dimensions();
    let icon = Arc::new(eframe::egui::IconData {
        rgba: icon_image.into_raw(),
        width: icon_width,
        height: icon_height,
    });
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Rhea")
            .with_icon(icon)
            .with_inner_size([1440.0, 920.0])
            .with_min_inner_size([1024.0, 680.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Rhea",
        options,
        Box::new(|creation_context| {
            let mut fonts = eframe::egui::FontDefinitions::default();
            if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") {
                fonts.font_data.insert(
                    "rhea-segoe".into(),
                    Arc::new(eframe::egui::FontData::from_owned(bytes)),
                );
                if let Some(proportional) = fonts
                    .families
                    .get_mut(&eframe::egui::FontFamily::Proportional)
                {
                    proportional.insert(0, "rhea-segoe".into());
                }
            }
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            creation_context.egui_ctx.set_fonts(fonts);
            let mut visuals = eframe::egui::Visuals::dark();
            // Any text that doesn't explicitly set its own color (hint/placeholder text
            // without an overriding RichText color, disabled widget labels, default combo
            // box/button text) falls back to this. It was a muted green, which is why
            // placeholders and other untouched text read as green instead of neutral white.
            use eframe::egui::{Color32, CornerRadius, Stroke};
            visuals.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(238, 239, 237);
            // Neutral black/white widgets; green only as the accent.
            visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(27, 28, 28);
            visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(24, 36, 29);
            visuals.widgets.active.weak_bg_fill = Color32::from_rgb(30, 48, 37);
            // bg_fill drives slider handles and the (custom) scrollbar handle.
            visuals.widgets.inactive.bg_fill = Color32::from_rgb(44, 112, 74);
            visuals.widgets.hovered.bg_fill = Color32::from_rgb(64, 160, 102);
            visuals.widgets.active.bg_fill = Color32::from_rgb(74, 186, 118);
            visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(52, 120, 82));
            for widget in [
                &mut visuals.widgets.noninteractive,
                &mut visuals.widgets.inactive,
                &mut visuals.widgets.hovered,
                &mut visuals.widgets.active,
                &mut visuals.widgets.open,
            ] {
                widget.corner_radius = CornerRadius::same(8);
            }
            visuals.selection.bg_fill = Color32::from_rgb(40, 92, 62);
            visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(238, 239, 237));
            visuals.text_cursor.stroke = Stroke::new(1.8, Color32::from_rgb(74, 186, 118));
            visuals.window_fill = Color32::from_rgb(22, 23, 23);
            visuals.panel_fill = Color32::from_rgb(19, 20, 20);
            visuals.extreme_bg_color = Color32::from_rgb(13, 14, 14);
            visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(40, 42, 41));
            visuals.window_shadow = eframe::egui::Shadow {
                offset: [0, 14],
                blur: 48,
                spread: 0,
                color: Color32::from_black_alpha(170),
            };
            visuals.popup_shadow = eframe::egui::Shadow {
                offset: [0, 8],
                blur: 24,
                spread: 0,
                color: Color32::from_black_alpha(150),
            };
            visuals.window_corner_radius = CornerRadius::same(14);
            visuals.menu_corner_radius = CornerRadius::same(12);
            creation_context.egui_ctx.set_visuals(visuals);
            creation_context
                .egui_ctx
                .style_mut_of(eframe::egui::Theme::Dark, |style| {
                    // Rounded accent-green scrollbar that thickens on hover.
                    style.spacing.scroll.floating = true;
                    style.spacing.scroll.bar_width = 7.0;
                    style.spacing.scroll.floating_width = 4.0;
                    style.spacing.scroll.foreground_color = false;
                    style.spacing.scroll.dormant_handle_opacity = 0.55;
                    style.spacing.scroll.active_handle_opacity = 0.9;
                    style.spacing.scroll.interact_handle_opacity = 1.0;
                    style.spacing.scroll.dormant_background_opacity = 0.0;
                    style.spacing.scroll.active_background_opacity = 0.12;
                    // Only chat bubbles, tool results and similar content opt in to text
                    // selection; chrome labels (titles, buttons, captions) stay unselectable.
                    style.interaction.selectable_labels = false;
                    style.animation_time = 0.15;
                });
            Ok(Box::new(app::RheaApp::default()))
        }),
    )
}
