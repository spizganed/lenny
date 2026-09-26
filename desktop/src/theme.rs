//! docs/design.md in egui: tokens (§3) and the sticker widgets (§4). Every interactive or grouping element is a flat
//! sticker: 3 px ink outline, chunky radius, hard offset shadow (blur 0), and it sinks into its shadow when pressed.
//! Nothing here depends on the OS: this layer carries over to Windows unchanged.

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Id,
    Rect, Response, Sense, Shadow, Stroke, StrokeKind, Ui, Vec2,
};

pub const INK: Color32 = Color32::from_rgb(0x07, 0x08, 0x1A);
pub const PAGE: Color32 = Color32::from_rgb(0x1A, 0x1D, 0x38);
pub const PAGE_DOT: Color32 = Color32::from_rgb(0x2C, 0x30, 0x60);
pub const SURFACE: Color32 = Color32::from_rgb(0x26, 0x2A, 0x4D);
pub const WELL: Color32 = Color32::from_rgb(0x0F, 0x11, 0x26);
pub const TEXT: Color32 = Color32::from_rgb(0xF4, 0xF1, 0xE8);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0xB7, 0xB9, 0xD6);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x9A, 0x9D, 0xC8);
pub const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD2, 0x3F);
pub const MINT: Color32 = Color32::from_rgb(0x2E, 0xE6, 0xC5);
pub const GREEN: Color32 = Color32::from_rgb(0x4A, 0xDE, 0x80);
pub const CORAL: Color32 = Color32::from_rgb(0xFF, 0x6B, 0x6B);
pub const LILAC: Color32 = Color32::from_rgb(0xA7, 0x8B, 0xFA);

pub const BORDER: f32 = 3.0;
pub const R_CARD: u8 = 20;
pub const R_BUTTON: u8 = 16;
pub const R_TILE: u8 = 14;
pub const R_CHIP: u8 = 12;
pub const R_PILL: u8 = 255;
pub const SH_HERO: f32 = 8.0;
pub const SH_CARD: f32 = 6.0;
pub const SH_PRIMARY: f32 = 5.0;
pub const SH_BUTTON: f32 = 4.0;
pub const SH_SMALL: f32 = 3.0;

/// Loads the design fonts (DM Sans, Bricolage Grotesque, JetBrains Mono; OFL) from `assets/fonts/` next to the
/// executable or in the source tree, when they're there; egui's built-in fonts otherwise.
// ponytail: fonts aren't committed (the build sandbox couldn't download them); drop the TTFs into
// desktop/assets/fonts (names below) and they're picked up, or embed them with include_bytes! when they are.
pub fn install_fonts(ctx: &egui::Context) {
    let dirs = [
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("assets/fonts"))),
        Some(std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts"))),
    ];
    let mut fonts = FontDefinitions::default();
    let mut found = 0;
    for (family, file) in [("body", "DMSans.ttf"), ("heading", "BricolageGrotesque.ttf"), ("mono", "JetBrainsMono.ttf")]
    {
        let bytes = dirs.iter().flatten().find_map(|d| std::fs::read(d.join(file)).ok());
        let fam = FontFamily::Name(family.into());
        let list = fonts.families.entry(fam.clone()).or_default();
        if let Some(b) = bytes {
            fonts.font_data.insert(family.into(), std::sync::Arc::new(FontData::from_owned(b)));
            list.push(family.into());
            found += 1;
        }
        // Fallbacks: egui's own fonts (and its emoji/symbol font for icons).
        let base = if family == "mono" { FontFamily::Monospace } else { FontFamily::Proportional };
        let fallback = fonts.families[&base].clone();
        fonts.families.get_mut(&fam).unwrap().extend(fallback);
    }
    if found < 3 {
        log::info!("design fonts: {found}/3 found in assets/fonts, using built-in fallbacks for the rest");
    }
    ctx.set_fonts(fonts);
}

pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("body".into()))
}
pub fn heading(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("heading".into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mono".into()))
}

/// No ripples, no hover tints, no Material leftovers: feedback comes from the press-sink.
pub fn apply_style(ctx: &egui::Context) {
    ctx.style_mut(|s| {
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = PAGE;
        v.window_fill = SURFACE;
        v.extreme_bg_color = WELL;
        v.override_text_color = Some(TEXT);
        v.window_shadow = Shadow { offset: [8, 8], blur: 0, spread: 0, color: INK };
        v.window_stroke = Stroke::new(BORDER, INK);
        v.window_corner_radius = CornerRadius::same(R_CARD);
        v.selection.bg_fill = LILAC;
        use egui::TextStyle::*;
        s.text_styles = [
            (Small, body(12.0)),
            (Body, body(15.0)),
            (Button, body(16.0)),
            (Heading, heading(24.0)),
            (Monospace, mono(14.0)),
        ]
        .into();
        s.spacing.item_spacing = vec2(12.0, 14.0);
        s.spacing.scroll.bar_width = 8.0;
        s.interaction.selectable_labels = false;
    });
}

/// Dot grid behind everything (§3.7): pageDot dots on a 24 px grid.
pub fn paint_page(ctx: &egui::Context) {
    let p = ctx.layer_painter(egui::LayerId::background());
    let r = ctx.screen_rect();
    p.rect_filled(r, 0.0, PAGE);
    let step = if r.width() < 700.0 { 22.0 } else { 24.0 };
    let mut y = r.top() + step / 2.0;
    while y < r.bottom() {
        let mut x = r.left() + step / 2.0;
        while x < r.right() {
            p.circle_filled(pos2(x, y), 1.6, PAGE_DOT);
            x += step;
        }
        y += step;
    }
}

/// A sticker at `rect`: fill, 3 px ink outline, hard shadow. `sink` 0..1 is how far it's pressed into its shadow.
/// Returns the rect the face was drawn at (content goes there).
pub fn paint_sticker(ui: &Ui, rect: Rect, fill: Color32, radius: u8, shadow: f32, sink: f32) -> Rect {
    let p = ui.painter();
    let off = shadow * sink;
    if shadow > 0.0 && sink < 1.0 {
        p.rect_filled(rect.translate(Vec2::splat(shadow)), CornerRadius::same(radius), INK);
    }
    let face = rect.translate(Vec2::splat(off));
    p.rect(face, CornerRadius::same(radius), fill, Stroke::new(BORDER, INK), StrokeKind::Inside);
    face
}

/// Press-sink (§2.4): 80 ms towards the shadow while held, back on release.
fn sink(ui: &Ui, id: Id, r: &Response, enabled: bool) -> f32 {
    let down = enabled && r.is_pointer_button_down_on();
    if ui.ctx().style().animation_time == 0.0 {
        return if down { 1.0 } else { 0.0 };
    }
    ui.ctx().animate_bool_with_time(id.with("sink"), down, 0.08)
}

pub fn card_frame(fill: Color32, shadow: f32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(BORDER, INK))
        .corner_radius(CornerRadius::same(R_CARD))
        .inner_margin(18)
        .outer_margin(egui::Margin { left: 0, top: 0, right: shadow as i8, bottom: shadow as i8 })
        .shadow(Shadow { offset: [shadow as i8, shadow as i8], blur: 0, spread: 0, color: INK })
}

/// Card (§4): surface, section label in mono caps, then the content.
pub fn card<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_frame(SURFACE, SH_CARD)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(label.to_uppercase()).font(mono(12.0)).color(TEXT_MUTED).strong());
            add(ui)
        })
        .inner
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Destructive,
    Neutral,
}

/// Full-width button (§4 Buttons). Disabled: no shadow, well fill, muted text, no press.
pub fn button(ui: &mut Ui, label: &str, kind: Kind, enabled: bool) -> Response {
    let (fill, fg, shadow) = match kind {
        Kind::Primary => (YELLOW, INK, SH_PRIMARY),
        Kind::Destructive => (CORAL, INK, SH_PRIMARY),
        Kind::Neutral => (WELL, TEXT, SH_BUTTON),
    };
    let (fill, fg, shadow) = if enabled { (fill, fg, shadow) } else { (WELL, TEXT_MUTED, 0.0) };
    // The shadow is part of the allocation, so shadowed and flat widgets line up on the right.
    let size = vec2(ui.available_width() - shadow, 52.0);
    let (rect, r) =
        ui.allocate_exact_size(size + Vec2::splat(shadow), if enabled { Sense::click() } else { Sense::hover() });
    let rect = Rect::from_min_size(rect.min, size);
    let s = sink(ui, r.id, &r, enabled);
    let face = paint_sticker(ui, rect, fill, R_BUTTON, shadow, s);
    ui.painter().text(face.center(), Align2::CENTER_CENTER, label, body(17.0), fg);
    if enabled {
        r.clone().on_hover_cursor(CursorIcon::PointingHand);
    }
    r
}

/// Square icon button (§4, header/title bar): surface, 3 px shadow, tooltip.
pub fn icon_button(ui: &mut Ui, icon: &str, tip: &str, fill: Color32, size: f32) -> Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(size + SH_SMALL), Sense::click());
    let rect = Rect::from_min_size(rect.min, Vec2::splat(size));
    let s = sink(ui, r.id, &r, true);
    let face = paint_sticker(ui, rect, fill, R_CHIP, SH_SMALL, s);
    let fg = if fill == SURFACE { TEXT } else { INK };
    ui.painter().text(face.center(), Align2::CENTER_CENTER, icon, body(size * 0.42), fg);
    r.on_hover_text(tip).on_hover_cursor(CursorIcon::PointingHand)
}

/// Segmented control (§4): one pill, equal segments, 3 px dividers, selected = mint. Returns the tapped index.
pub fn segmented(ui: &mut Ui, id: &str, labels: &[String], selected: Option<usize>) -> Option<usize> {
    if labels.is_empty() {
        return None;
    }
    let size = vec2(ui.available_width() - SH_BUTTON, 48.0);
    let (outer, _) = ui.allocate_exact_size(size + Vec2::splat(SH_BUTTON), Sense::hover());
    let rect = Rect::from_min_size(outer.min, size);
    let p = ui.painter();
    p.rect_filled(rect.translate(Vec2::splat(SH_BUTTON)), CornerRadius::same(R_PILL), INK);
    p.rect_filled(rect, CornerRadius::same(R_PILL), WELL);
    let w = rect.width() / labels.len() as f32;
    let mut tapped = None;
    for (i, label) in labels.iter().enumerate() {
        let seg = Rect::from_min_size(pos2(rect.left() + w * i as f32, rect.top()), vec2(w, rect.height()));
        let r = ui.interact(seg, Id::new(id).with(i), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let on = selected == Some(i);
        if on {
            // Rounded only where the segment meets the pill's ends.
            let mut cr = CornerRadius::ZERO;
            if i == 0 {
                cr.nw = R_PILL;
                cr.sw = R_PILL;
            }
            if i == labels.len() - 1 {
                cr.ne = R_PILL;
                cr.se = R_PILL;
            }
            ui.painter().rect_filled(seg, cr, MINT);
        }
        if i > 0 {
            ui.painter().line_segment([seg.left_top(), seg.left_bottom()], Stroke::new(BORDER, INK));
        }
        let font = if label.len() > 10 { body(13.0) } else { body(15.0) };
        ui.painter().text(seg.center(), Align2::CENTER_CENTER, label, font, if on { INK } else { TEXT });
        if r.clicked() {
            tapped = Some(i);
        }
    }
    ui.painter().rect_stroke(rect, CornerRadius::same(R_PILL), Stroke::new(BORDER, INK), StrokeKind::Inside);
    tapped
}

/// Toggle button (§4): off = well, on = mint + check. Returns clicked.
pub fn toggle(ui: &mut Ui, label: &str, on: bool) -> bool {
    let size = vec2(ui.available_width() - SH_BUTTON, 50.0);
    let (outer, r) = ui.allocate_exact_size(size + Vec2::splat(SH_BUTTON), Sense::click());
    let rect = Rect::from_min_size(outer.min, size);
    let s = sink(ui, r.id, &r, true);
    let face = paint_sticker(ui, rect, if on { MINT } else { WELL }, R_BUTTON, SH_BUTTON, s);
    let text = if on { format!("✔  {label}") } else { label.to_string() };
    ui.painter().text(face.center(), Align2::CENTER_CENTER, text, body(16.0), if on { INK } else { TEXT });
    r.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

/// Switch (§4): 68x38 pill track, 24 px knob, mint when on, 120 ms knob slide. Returns clicked.
pub fn switch(ui: &mut Ui, id: &str, on: bool) -> bool {
    let size = vec2(68.0, 38.0);
    let (outer, r) = ui.allocate_exact_size(size + Vec2::splat(SH_SMALL), Sense::click());
    let rect = Rect::from_min_size(outer.min, size);
    paint_sticker(ui, rect, if on { MINT } else { WELL }, R_PILL, SH_SMALL, 0.0);
    let t = ui.ctx().animate_bool_with_time(Id::new(id), on, 0.12);
    let x = egui::lerp(rect.left() + 19.0..=rect.right() - 19.0, t);
    ui.painter().circle(pos2(x, rect.center().y), 12.0, TEXT, Stroke::new(BORDER, INK));
    r.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

/// Status chip (§4): pill, dot in the status colour, bold label.
pub fn status_chip(ui: &mut Ui, color: Color32, label: &str, fill: Color32) {
    let galley = ui.painter().layout_no_wrap(label.to_string(), body(15.0), TEXT);
    let size = vec2(galley.size().x + 58.0, 42.0);
    let (outer, _) = ui.allocate_exact_size(size + Vec2::splat(SH_SMALL), Sense::hover());
    let face = paint_sticker(ui, Rect::from_min_size(outer.min, size), fill, R_PILL, SH_SMALL, 0.0);
    ui.painter().circle(pos2(face.left() + 22.0, face.center().y), 7.0, color, Stroke::new(BORDER, INK));
    ui.painter().galley(pos2(face.left() + 40.0, face.center().y - galley.size().y / 2.0), galley, TEXT);
}

/// Copy chip (§4): mono value + copy icon; a tap copies it and flashes a check for a second.
pub fn copy_chip(ui: &mut Ui, value: &str) {
    let id = Id::new(("copy", value));
    let copied_at: Option<f64> = ui.data(|d| d.get_temp(id));
    let now = ui.input(|i| i.time);
    let copied = copied_at.is_some_and(|t| now - t < 1.2);
    let icon = if copied { "✔" } else { "📋" };
    let galley = ui.painter().layout_no_wrap(format!("{value}  {icon}"), mono(14.0), TEXT);
    let size = vec2(galley.size().x + 28.0, 44.0);
    let (outer, r) = ui.allocate_exact_size(size + Vec2::splat(SH_SMALL), Sense::click());
    let s = sink(ui, id, &r, true);
    let face = paint_sticker(ui, Rect::from_min_size(outer.min, size), SURFACE, R_CHIP, SH_SMALL, s);
    ui.painter().galley(face.center() - galley.size() / 2.0, galley, TEXT);
    if r.on_hover_text("Copy").on_hover_cursor(CursorIcon::PointingHand).clicked() {
        ui.ctx().copy_text(value.to_string());
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    if copied {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
    }
}

/// Stat tile (§4): well, outline only, small label + mono value.
pub fn stat_tile(ui: &mut Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(WELL)
        .stroke(Stroke::new(BORDER, INK))
        .corner_radius(CornerRadius::same(R_TILE))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(egui::RichText::new(label).font(body(12.0)).color(TEXT_MUTED));
            ui.label(egui::RichText::new(value).font(mono(17.0)).strong());
        });
}

/// Badge overlaid on the preview (§4): small pill, mono bold.
pub fn badge(ui: &Ui, at: egui::Pos2, align_right: bool, text: &str, fill: Color32, dot: bool) {
    let fg = if fill == SURFACE { TEXT } else { INK };
    let galley = ui.painter().layout_no_wrap(text.to_string(), mono(13.0), fg);
    let w = galley.size().x + if dot { 38.0 } else { 24.0 };
    let min = if align_right { pos2(at.x - w, at.y) } else { at };
    let face = paint_sticker(ui, Rect::from_min_size(min, vec2(w, 32.0)), fill, R_PILL, SH_SMALL, 0.0);
    let mut x = face.left() + 12.0;
    if dot {
        ui.painter().circle_filled(pos2(x + 4.0, face.center().y), 4.5, INK);
        x += 14.0;
    }
    ui.painter().galley(pos2(x, face.center().y - galley.size().y / 2.0), galley, fg);
}

/// Exposure-style slider (§4): pill track, hard-edged lilac fill, bordered thumb. The value moves while dragging;
/// Some(value) is returned once, on release (one camera change, not thirty).
pub fn slider(ui: &mut Ui, id: &str, value: f32, range: std::ops::RangeInclusive<f32>, step: f32) -> Option<f32> {
    let key = Id::new(id);
    let (lo, hi) = (*range.start(), *range.end());
    let drag: Option<f32> = ui.data(|d| d.get_temp(key));
    let shown = drag.unwrap_or(value).clamp(lo, hi);
    let thumb = 34.0;
    let size = vec2(ui.available_width(), thumb + SH_SMALL);
    let (rect, r) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let track =
        Rect::from_min_size(pos2(rect.left(), rect.top() + (thumb - 22.0) / 2.0), vec2(rect.width() - SH_SMALL, 22.0));
    let f = if hi > lo { (shown - lo) / (hi - lo) } else { 0.0 };
    let tx = track.left() + thumb / 2.0 + f * (track.width() - thumb);
    paint_sticker(ui, track, WELL, R_PILL, SH_SMALL, 0.0);
    let fill = Rect::from_min_max(track.min + Vec2::splat(BORDER), pos2(tx, track.bottom() - BORDER));
    ui.painter().rect_filled(fill, CornerRadius { nw: R_PILL, sw: R_PILL, ne: 0, se: 0 }, LILAC);
    ui.painter().rect_stroke(track, CornerRadius::same(R_PILL), Stroke::new(BORDER, INK), StrokeKind::Inside);
    let c = pos2(tx, track.center().y);
    ui.painter().circle_filled(c + Vec2::splat(SH_SMALL), thumb / 2.0, INK);
    ui.painter().circle(c, thumb / 2.0, TEXT, Stroke::new(BORDER, INK));
    let at = |x: f32| {
        let t = ((x - track.left() - thumb / 2.0) / (track.width() - thumb)).clamp(0.0, 1.0);
        let v = lo + t * (hi - lo);
        if step > 0.0 {
            (lo + ((v - lo) / step).round() * step).clamp(lo, hi)
        } else {
            v
        }
    };
    if let Some(p) = r.interact_pointer_pos() {
        if r.dragged() || r.is_pointer_button_down_on() {
            ui.data_mut(|d| d.insert_temp(key, at(p.x)));
        }
    }
    if r.drag_stopped() || r.clicked() {
        let v = r.interact_pointer_pos().map(|p| at(p.x)).or(drag).unwrap_or(shown);
        ui.data_mut(|d| d.remove::<f32>(key));
        return Some(v);
    }
    None
}
