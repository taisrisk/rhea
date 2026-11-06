//! Shared motion rules. They follow the 20 animation guidelines in `docs/design/motion.md`:
//! ease-out for anything entering, 150-250 ms for dropdowns, 30 ms stagger between list rows, a
//! 0.97 scale while pressed, no tween while a control is held, exits shorter than entrances,
//! icons that crossfade instead of swapping, and one shared place for every timing so the whole
//! app moves the same way.

use super::*;
use egui::emath::easing;

/// Hover and focus feedback: quick, eased out.
pub(super) const HOVER_SECS: f32 = 0.15;
/// A control returning from pressed: ease out, slightly slower than the press itself.
const RELEASE_SECS: f32 = 0.16;
/// Pressing in is faster than letting go (exits are about 20% shorter than entrances).
const PRESS_SECS: f32 = 0.08;
/// Dropdowns, popovers and panel switches.
pub(super) const PANEL_SECS: f32 = 0.22;
/// Delay between consecutive list rows entering.
pub(super) const STAGGER_SECS: f32 = 0.03;

pub(super) fn ease_out(t: f32) -> f32 {
    easing::cubic_out(t.clamp(0.0, 1.0))
}

/// 0..1 for hover, eased out in both directions.
pub(super) fn hover_t(ctx: &egui::Context, id: egui::Id, hovered: bool) -> f32 {
    ctx.animate_bool_with_time_and_easing(id.with("motion-hover"), hovered, HOVER_SECS, easing::cubic_out)
}

/// Scale of a control while the pointer is down on it: 0.97, reached fast and released smoothly.
/// While it is held it stays exactly at 0.97 (no tween).
pub(super) fn press_scale(ctx: &egui::Context, response: &egui::Response) -> f32 {
    let down = response.is_pointer_button_down_on();
    let t = ctx.animate_bool_with_time_and_easing(response.id.with("motion-press"), down, if down { PRESS_SECS } else { RELEASE_SECS }, easing::cubic_out);
    1.0 - 0.03 * t
}

pub(super) fn scale_rect(rect: Rect, scale: f32) -> Rect {
    Rect::from_center_size(rect.center(), rect.size() * scale)
}

/// How far into its entrance row `index` of a list is, `elapsed` seconds after the list appeared.
pub(super) fn stagger(elapsed: f32, index: usize) -> f32 {
    ease_out((elapsed - index as f32 * STAGGER_SECS) / PANEL_SECS)
}

/// The activity ring turns slowly for the main agent alone and faster with every running
/// subagent. The speed eases toward its target so adding or losing a subagent never jerks the ring.
#[derive(Default)]
pub(super) struct Spinner {
    pub phase: f32,
    speed: f32,
}

impl Spinner {
    /// Radians per second: about one turn every three seconds alone, more with each subagent.
    pub fn target_speed(running_subagents: usize) -> f32 {
        (2.0 + 1.6 * running_subagents as f32).min(14.0)
    }

    pub fn step(&mut self, running_subagents: usize, dt: f32) {
        let dt = dt.clamp(0.0, 0.1);
        if self.speed == 0.0 {
            self.speed = Self::target_speed(0);
        }
        self.speed += (Self::target_speed(running_subagents) - self.speed) * (dt * 3.0).min(1.0);
        self.phase = (self.phase + self.speed * dt) % std::f32::consts::TAU;
    }
}

/// The round send button. Idle it shows an up arrow; while a run is working it shows the activity
/// ring, and hovering crossfades the ring into a stop square (scaling up as it arrives).
pub(super) fn orb(ui: &mut egui::Ui, id: &str, working: bool, enabled: bool, phase: f32, hint: &str) -> egui::Response {
    let (full, response) = ui.allocate_exact_size(Vec2::splat(40.0), if enabled { Sense::click() } else { Sense::hover() });
    let ctx = ui.ctx().clone();
    let hovered = response.hovered() && enabled;
    let h = hover_t(&ctx, response.id.with(id), hovered);
    let rect = scale_rect(full, if enabled { press_scale(&ctx, &response) } else { 1.0 });
    let center = rect.center();
    let idle = Color32::from_rgb(18, 20, 19);
    let body = if working { lerp_color(idle, Color32::from_rgb(60, 30, 30), h) } else { lerp_color(idle, Color32::from_rgb(30, 48, 37), h) };
    if h > 0.0 && !working {
        let glow = egui::Shadow { offset: [0, 0], blur: 14, spread: 0, color: Color32::from_rgba_unmultiplied(GLOW_GREEN[0], GLOW_GREEN[1], GLOW_GREEN[2], (46.0 * h) as u8) };
        ui.painter().add(glow.as_shape(rect, CornerRadius::same(20)));
    }
    ui.painter().circle_filled(center, 20.0 * (rect.width() / 40.0), body);
    ui.painter().circle_stroke(center, 19.5 * (rect.width() / 40.0), egui::Stroke::new(1.0, lerp_color(LINE, if working { DANGER } else { HOVER_GREEN }, h * 0.6)));
    if working {
        let ring = 1.0 - h;
        if ring > 0.01 {
            let points: Vec<_> = (0..=24)
                .map(|i| {
                    let a = phase + i as f32 / 24.0 * std::f32::consts::PI * 1.5;
                    pos2(center.x + 11.0 * a.cos(), center.y + 11.0 * a.sin())
                })
                .collect();
            ui.painter().add(egui::Shape::line(points, egui::Stroke::new(2.5, MINT_DEEP.gamma_multiply(ring))));
        }
        if h > 0.01 {
            let size = 22.0 * (0.8 + 0.2 * h);
            paint_icon(ui.painter(), Rect::from_center_size(center, Vec2::splat(size)), "stop", DANGER.gamma_multiply(h));
        }
    } else {
        let tint = lerp_color(INK, HOVER_GREEN, h);
        paint_icon(ui.painter(), Rect::from_center_size(center, Vec2::splat(24.0)), "up", tint.gamma_multiply(if enabled { 1.0 } else { 0.35 }));
    }
    if hovered {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, hint));
    response.on_hover_text(hint)
}

/// A switch whose knob stretches about 12% while it travels, then settles.
pub(super) fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(42.0, 24.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let ctx = ui.ctx().clone();
    let t = ctx.animate_bool_with_time_and_easing(response.id.with("switch"), *on, PANEL_SECS, easing::cubic_out);
    let hover = hover_t(&ctx, response.id, response.hovered());
    let track = lerp_color(Color32::from_rgb(40, 42, 41), Color32::from_rgb(44, 112, 74), t);
    ui.painter().rect_filled(rect, CornerRadius::same(12), lerp_color(track, track.gamma_multiply(1.25), hover));
    ui.painter().rect_stroke(rect, CornerRadius::same(12), egui::Stroke::new(1.0, lerp_color(LINE, HOVER_GREEN, t)), egui::StrokeKind::Inside);
    let height = 18.0;
    let stretch = 1.0 + 0.12 * 4.0 * t * (1.0 - t);
    let width = height * stretch;
    let left_stop = rect.left() + 3.0;
    let right_stop = rect.right() - 3.0 - width;
    let x = left_stop + (right_stop - left_stop) * t;
    let knob = Rect::from_min_size(pos2(x, rect.center().y - height / 2.0), Vec2::new(width, height));
    ui.painter().rect_filled(knob, CornerRadius::same(9), lerp_color(Color32::from_rgb(190, 192, 190), INK, t));
    if response.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, true, *on, ""));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_enter_one_after_another() {
        assert_eq!(stagger(0.0, 0), 0.0);
        assert!(stagger(0.05, 0) > stagger(0.05, 1));
        assert_eq!(stagger(1.0, 5), 1.0);
        // the fifth row has not started at 0.12 s, the first is well on its way
        assert_eq!(stagger(0.12, 5), 0.0);
        assert!(stagger(0.12, 0) > 0.4);
    }

    #[test]
    fn the_activity_ring_speeds_up_with_running_subagents_and_is_capped() {
        assert!(Spinner::target_speed(1) > Spinner::target_speed(0));
        assert!(Spinner::target_speed(4) > Spinner::target_speed(2));
        assert_eq!(Spinner::target_speed(100), 14.0);
        let mut ring = Spinner::default();
        for _ in 0..120 {
            ring.step(0, 1.0 / 60.0);
        }
        let alone = ring.speed;
        for _ in 0..120 {
            ring.step(3, 1.0 / 60.0);
        }
        assert!(ring.speed > alone + 2.0, "{} vs {}", ring.speed, alone);
        assert!(ring.phase >= 0.0 && ring.phase < std::f32::consts::TAU);
    }

    #[test]
    fn a_pressed_control_is_scaled_to_97_percent() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(100.0, 40.0));
        let scaled = scale_rect(rect, 0.97);
        assert!((scaled.width() - 97.0).abs() < 0.001 && scaled.center() == rect.center());
    }
}
