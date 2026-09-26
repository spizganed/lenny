//! Lenny Desktop's window (docs/design.md §6 "Desktop receiver"): app-drawn title bar on a borderless window,
//! preview hero on the left, stacked control cards on the right; one column when the window is narrow.
//! Sizes come from the window each frame (fractions and clamps, no fixed positions), so resizing keeps everything
//! aligned. Only winit/egui calls here: this file is expected to build on Windows unchanged.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, CornerRadius, CursorIcon, Id, Layout, Margin, Pos2, Rect,
    ResizeDirection, RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, ViewportCommand,
};
use lenny_core::wire::PAN_CENTER;
use lenny_core::*;

use crate::receiver::{c_chars, local_ipv4s, Engine};
use crate::theme::{self as t, Kind};

const TITLE_H: f32 = 68.0;
const WIDE: f32 = 1000.0; // below this the layout is one scrolling column

pub struct App {
    engine: Result<Engine, String>,
    video: Option<Video>,
    preview_seq: u64,
    qr: Option<(String, TextureHandle)>,
    manual: bool,
    /// Pan being dragged on the preview (sent at most every 80 ms and on release).
    pan: Option<(f32, f32)>,
    pan_sent: Instant,
    wheel_zoom: Option<(u16, Instant)>,
    rates: Rates,
    screenshot: Option<(std::path::PathBuf, Instant, Duration, bool)>,
}

struct Video {
    tex: TextureHandle,
    size: (usize, usize),
    source: (usize, usize),
}

#[derive(Default)]
struct Rates {
    at: Option<Instant>,
    last: Option<lenny_stats>,
    last_decoded: u64,
    fps: f64,
    kbps: f64,
    shown_fps: f64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, port: u16, screenshot: Option<(std::path::PathBuf, Duration)>) -> Self {
        t::install_fonts(&cc.egui_ctx);
        t::apply_style(&cc.egui_ctx);
        let engine = Engine::start(cc.egui_ctx.clone(), port);
        if let Err(e) = &engine {
            log::error!("{e}");
        }
        App {
            engine,
            video: None,
            preview_seq: 0,
            qr: None,
            manual: false,
            pan: None,
            pan_sent: Instant::now(),
            wheel_zoom: None,
            rates: Rates::default(),
            screenshot: screenshot.map(|(p, d)| (p, Instant::now(), d, false)),
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::from(t::PAGE).to_array()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        t::paint_page(ctx);
        if let Ok(e) = &mut self.engine {
            e.tick();
        }
        self.pull_video(ctx);
        self.update_rates();
        title_bar(ctx, self.engine.as_ref().ok());
        resize_edges(ctx);

        let w = ctx.screen_rect().width();
        let side_margin = if w < 700.0 { 16 } else { 28 };
        if w >= WIDE {
            let side_w = (w * 0.31).clamp(340.0, 440.0);
            egui::SidePanel::right("controls")
                .exact_width(side_w)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(Margin { left: 0, right: side_margin, top: 4, bottom: 16 }))
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| self.cards(ui));
                });
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.inner_margin(Margin { left: side_margin, right: 16, top: 4, bottom: 24 }))
                .show(ctx, |ui| {
                    let size = ui.available_size();
                    self.preview(ui, size);
                });
        } else {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.inner_margin(Margin {
                    left: side_margin,
                    right: side_margin,
                    top: 4,
                    bottom: 16,
                }))
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let width = ui.available_width();
                        // As tall as the picture needs at this width (its real aspect ratio), within 60 % of the window.
                        let aspect = self.video.as_ref().map_or(16.0 / 9.0, |v| v.size.0 as f32 / v.size.1 as f32);
                        let pad = 2.0 * (16.0 + t::BORDER) + t::SH_HERO;
                        let h = ((width - pad) / aspect + pad).min(ctx.screen_rect().height() * 0.6).max(200.0);
                        self.preview(ui, vec2(width, h));
                        ui.add_space(16.0);
                        self.cards(ui);
                    });
                });
        }
        self.approval_dialog(ctx);
        self.take_screenshot(ctx);
        // Stats and the QR token refresh even when nothing else happens.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

// ---- window chrome -------------------------------------------------------------------------------------------

/// Custom title bar (decorations are off): wordmark, platform tag, status chip, and real window controls.
/// The empty part drags the window; a double click maximizes/restores.
fn title_bar(ctx: &egui::Context, engine: Option<&Engine>) {
    egui::TopBottomPanel::top("title")
        .exact_height(TITLE_H)
        .show_separator_line(false)
        .frame(egui::Frame::NONE.inner_margin(Margin { left: 28, right: 20, top: 12, bottom: 4 }))
        .show(ctx, |ui| {
            let bar = ui.max_rect();
            let drag = ui.interact(bar, Id::new("title-drag"), Sense::click_and_drag());
            if drag.drag_started_by(egui::PointerButton::Primary) {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }
            let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            if drag.double_clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
            ui.horizontal_centered(|ui| {
                // Wordmark with its 3 px hard ink text shadow.
                let (r, _) = ui.allocate_exact_size(vec2(96.0, 46.0), Sense::hover());
                let font = t::heading(38.0);
                ui.painter().text(r.left_center() + vec2(3.0, 3.0), Align2::LEFT_CENTER, "Lenny", font.clone(), t::INK);
                ui.painter().text(r.left_center(), Align2::LEFT_CENTER, "Lenny", font, t::TEXT);
                let os = if cfg!(windows) { "WINDOWS" } else { "LINUX" };
                let (r, _) = ui.allocate_exact_size(vec2(84.0, 36.0), Sense::hover());
                t::badge(ui, r.left_center() - vec2(0.0, 16.0), false, os, t::LILAC, false);
                if let Some(e) = engine {
                    let (color, label) = status(e);
                    t::status_chip(ui, color, label, t::SURFACE);
                } else {
                    t::status_chip(ui, t::CORAL, "Can't start", t::SURFACE);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if t::icon_button(ui, "🗙", "Close", t::SURFACE, 44.0).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                    let (icon, tip) = if maximized { ("🗗", "Restore") } else { ("🗖", "Maximize") };
                    if t::icon_button(ui, icon, tip, t::SURFACE, 44.0).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                    }
                    if t::icon_button(ui, "🗕", "Minimize", t::SURFACE, 44.0).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                });
            });
        });
}

/// Borderless windows get no resize frame from the OS: the outer 6 px start an OS resize (winit, same on Windows).
fn resize_edges(ctx: &egui::Context) {
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    let r = ctx.screen_rect();
    let Some(p) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    const E: f32 = 6.0;
    let (l, rt, tp, b) = (p.x < r.left() + E, p.x > r.right() - E, p.y < r.top() + E, p.y > r.bottom() - E);
    let (dir, cursor) = match (l, rt, tp, b) {
        (true, _, true, _) => (ResizeDirection::NorthWest, CursorIcon::ResizeNorthWest),
        (_, true, true, _) => (ResizeDirection::NorthEast, CursorIcon::ResizeNorthEast),
        (true, _, _, true) => (ResizeDirection::SouthWest, CursorIcon::ResizeSouthWest),
        (_, true, _, true) => (ResizeDirection::SouthEast, CursorIcon::ResizeSouthEast),
        (true, ..) => (ResizeDirection::West, CursorIcon::ResizeWest),
        (_, true, ..) => (ResizeDirection::East, CursorIcon::ResizeEast),
        (_, _, true, _) => (ResizeDirection::North, CursorIcon::ResizeNorth),
        (.., true) => (ResizeDirection::South, CursorIcon::ResizeSouth),
        _ => return,
    };
    ctx.set_cursor_icon(cursor);
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
    }
}

fn status(e: &Engine) -> (Color32, &'static str) {
    let s = e.session.state();
    match s {
        x if x == LENNY_STATE_STREAMING as i32 => {
            if e.stream_status().is_some() {
                (t::LILAC, "Phone paused")
            } else {
                (t::GREEN, "Streaming")
            }
        }
        x if x == LENNY_STATE_AWAITING_APPROVAL as i32 => (t::LILAC, "Allow the phone?"),
        x if x == LENNY_STATE_HANDSHAKE as i32 => (t::LILAC, "Connecting…"),
        x if x == LENNY_STATE_CLOSED as i32 => (t::CORAL, "Stopped"),
        _ => (t::LILAC, "Waiting for a phone"),
    }
}

// ---- data ---------------------------------------------------------------------------------------------------

impl App {
    fn pull_video(&mut self, ctx: &egui::Context) {
        let Ok(e) = &self.engine else { return };
        if let Some(p) = e.take_preview(&mut self.preview_seq) {
            let img = ColorImage::from_rgba_unmultiplied([p.width, p.height], &p.rgba);
            match &mut self.video {
                Some(v) if v.size == (p.width, p.height) => v.tex.set(img, TextureOptions::LINEAR),
                _ => {
                    let tex = ctx.load_texture("preview", img, TextureOptions::LINEAR);
                    self.video = Some(Video { tex, size: (p.width, p.height), source: p.source });
                }
            }
            if let Some(v) = &mut self.video {
                v.source = p.source;
            }
        }
        if e.session.state() != LENNY_STATE_STREAMING as i32 {
            self.video = None; // next stream re-measures from its first frame
            self.manual = false;
        }
    }

    fn update_rates(&mut self) {
        let Ok(e) = &self.engine else { return };
        let now = Instant::now();
        if self.rates.at.is_some_and(|a| now - a < Duration::from_secs(1)) {
            return;
        }
        let st = e.session.stats();
        if let (Some(at), Some(last)) = (self.rates.at, self.rates.last) {
            let secs = (now - at).as_secs_f64();
            self.rates.fps = st.frames.saturating_sub(last.frames) as f64 / secs;
            self.rates.kbps = st.bytes.saturating_sub(last.bytes) as f64 * 8.0 / 1000.0 / secs;
            self.rates.shown_fps = e.decoded_frames().saturating_sub(self.rates.last_decoded) as f64 / secs;
        }
        self.rates.at = Some(now);
        self.rates.last = Some(st);
        self.rates.last_decoded = e.decoded_frames();
    }

    fn take_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, start, delay, asked)) = &mut self.screenshot else { return };
        // Give the engine a moment (virtual camera status, a phone that's already connecting), then shoot.
        if !*asked && start.elapsed() >= *delay {
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(Default::default()));
            *asked = true;
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|ev| match ev {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            if let Err(err) = save_png(path, &img) {
                log::error!("screenshot: {err}");
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

fn save_png(path: &std::path::Path, img: &ColorImage) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(std::io::Error::other)?;
    w.write_image_data(img.as_raw()).map_err(std::io::Error::other)
}

// ---- preview (Task 6: the box is the video's real aspect ratio) -----------------------------------------------

impl App {
    fn preview(&mut self, ui: &mut Ui, avail: Vec2) {
        let (area, _) = ui.allocate_exact_size(avail, Sense::hover());
        const PAD: f32 = 16.0 + t::BORDER;
        // The picture's real aspect ratio, from the decoded frames (not what was requested). 16:9 until one arrives.
        let aspect = self.video.as_ref().map_or(16.0 / 9.0, |v| v.size.0 as f32 / v.size.1 as f32);
        let max = (area.size() - Vec2::splat(2.0 * PAD + t::SH_HERO)).max(Vec2::splat(40.0));
        let inner = if max.x / max.y > aspect { vec2(max.y * aspect, max.y) } else { vec2(max.x, max.x / aspect) };
        let card =
            Rect::from_center_size(area.center() - Vec2::splat(t::SH_HERO / 2.0), inner + Vec2::splat(2.0 * PAD));
        t::paint_sticker(ui, card, t::WELL, t::R_CARD, t::SH_HERO, 0.0);
        let pic = card.shrink(PAD);
        let Ok(e) = &self.engine else {
            let msg = self.engine.as_ref().err().cloned().unwrap_or_default();
            empty_state(ui, pic, "Can't start", &msg);
            return;
        };
        let streaming = e.session.state() == LENNY_STATE_STREAMING as i32;
        let fresh = e.last_frame_at().is_some_and(|t| t.elapsed() < Duration::from_secs(1));
        match (&self.video, streaming) {
            (Some(v), true) => {
                egui::Image::new(&v.tex).corner_radius(CornerRadius::same(t::R_TILE)).paint_at(ui, pic);
                ui.painter().rect_stroke(
                    pic,
                    CornerRadius::same(t::R_TILE),
                    Stroke::new(t::BORDER, t::INK),
                    StrokeKind::Inside,
                );
                if !fresh {
                    // Never a frozen picture without saying so (CLAUDE.md).
                    ui.painter().rect_filled(pic, CornerRadius::same(t::R_TILE), t::INK.gamma_multiply(0.6));
                    let why = e.stream_status().map(|(_, r)| r).filter(|r| !r.is_empty());
                    let text = why.unwrap_or_else(|| "Waiting for video…".into());
                    ui.painter().text(pic.center(), Align2::CENTER_CENTER, text, t::heading(24.0), t::TEXT);
                }
            }
            (_, true) => {
                let sub = e.stream_status().map(|(_, r)| r).unwrap_or_default();
                empty_state(ui, pic, "Waiting for video…", &sub);
            }
            _ => empty_state(ui, pic, "No phone connected", "Scan the QR code with Lenny on your phone"),
        }
        if streaming {
            let inset = 18.0;
            t::badge(ui, pic.left_top() + Vec2::splat(inset), false, "LIVE", t::GREEN, true);
            let (_, cs) = e.session.control_state();
            let caps = e.session.peer_caps();
            let lens = caps
                .as_ref()
                .and_then(|c| c.lenses.get(cs.lens_id as usize))
                .map(|l| String::from_utf8_lossy(&l.label).into_owned());
            let mut info = vec![];
            if let Some(v) = &self.video {
                info.push(format!("{}×{}", v.source.0, v.source.1));
            }
            if let Some(l) = lens {
                info.push(l);
            }
            if cs.zoom > 100 {
                info.push(format!("{:.1}×", cs.zoom as f32 / 100.0));
            }
            if !info.is_empty() {
                t::badge(ui, pos2(pic.right() - inset, pic.top() + inset), true, &info.join(" · "), t::SURFACE, false);
            }
            if self.video.is_some() {
                self.preview_input(ui, pic);
            }
        }
    }

    /// Manual mode: tap = focus there. Zoomed in: drag = pan the sensor crop (protocol.md §6.9), wheel = zoom.
    fn preview_input(&mut self, ui: &mut Ui, pic: Rect) {
        let Ok(e) = &self.engine else { return };
        let (_, cs) = e.session.control_state();
        let peer = e.session.peer().1;
        let caps = e.session.peer_caps();
        let lens = caps.as_ref().and_then(|c| c.lenses.get(cs.lens_id as usize));
        let base = lens.map_or(100, |l| if l.zoom_base == 0 { 100 } else { l.zoom_base }) as f32 / 100.0;
        let z = base * cs.zoom.max(1) as f32 / 100.0; // zoom on the camera's field of view
        let can_pan = peer.controls & LENNY_CAP_PAN != 0 && z > 1.01;
        let manual = self.manual || !is_auto(&cs);
        let r = ui.interact(pic, Id::new("preview"), Sense::click_and_drag());
        if manual && peer.controls & LENNY_CAP_FOCUS != 0 {
            r.clone().on_hover_cursor(CursorIcon::Crosshair);
        } else if can_pan {
            r.clone().on_hover_cursor(CursorIcon::Grab);
        }
        if r.clicked() && manual {
            if let Some(p) = r.interact_pointer_pos() {
                let (u, v) = ((p.x - pic.left()) / pic.width(), (p.y - pic.top()) / pic.height());
                e.control(lenny_control_cmd::LENNY_CTL_FOCUS_AT, norm(u), norm(v), 0);
                self.manual = true;
            }
        }
        if can_pan && r.dragged() {
            let d = r.drag_delta();
            let k = (1.0 / z) / (1.0 - 1.0 / z) * 65535.0;
            let (x, y) = self.pan.unwrap_or((cs.pan_x as f32, cs.pan_y as f32));
            let next =
                ((x - d.x / pic.width() * k).clamp(0.0, 65535.0), (y - d.y / pic.height() * k).clamp(0.0, 65535.0));
            self.pan = Some(next);
            if self.pan_sent.elapsed() > Duration::from_millis(80) {
                e.control(lenny_control_cmd::LENNY_CTL_PAN, next.0 as u16, next.1 as u16, 0);
                self.pan_sent = Instant::now();
            }
        }
        if r.drag_stopped() {
            if let Some((x, y)) = self.pan.take() {
                e.control(lenny_control_cmd::LENNY_CTL_PAN, x as u16, y as u16, 0);
            }
        }
        // Wheel over the picture zooms (within the lens's range), sent at most every 100 ms.
        if peer.controls & LENNY_CAP_ZOOM != 0 && r.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let (lo, hi) = lens.filter(|l| l.zoom_max > 0).map_or((100, 400), |l| (l.zoom_min.max(1), l.zoom_max));
                let cur = self.wheel_zoom.map_or(cs.zoom, |w| w.0) as f32;
                let next = (cur * (1.0 + scroll / 600.0)).clamp(lo as f32, hi as f32) as u16;
                self.wheel_zoom =
                    Some((next, self.wheel_zoom.map_or(Instant::now() - Duration::from_secs(1), |w| w.1)));
            }
        }
        if let Some((zv, at)) = self.wheel_zoom {
            if at.elapsed() > Duration::from_millis(100) {
                e.control(lenny_control_cmd::LENNY_CTL_ZOOM, 0, 0, zv as i32);
                self.wheel_zoom =
                    if ui.input(|i| i.smooth_scroll_delta.y) != 0.0 { Some((zv, Instant::now())) } else { None };
            }
        }
    }
}

fn norm(f: f32) -> u16 {
    (f.clamp(0.0, 1.0) * 65535.0) as u16
}

fn empty_state(ui: &Ui, r: Rect, title: &str, sub: &str) {
    ui.painter().rect(r, CornerRadius::same(t::R_TILE), t::WELL, Stroke::new(t::BORDER, t::INK), StrokeKind::Inside);
    ui.painter().text(r.center() - vec2(0.0, 44.0), Align2::CENTER_CENTER, "📷", t::body(52.0), t::TEXT_FAINT);
    ui.painter().text(r.center() + vec2(0.0, 12.0), Align2::CENTER_CENTER, title, t::heading(26.0), t::TEXT_FAINT);
    if !sub.is_empty() {
        ui.painter().text(r.center() + vec2(0.0, 44.0), Align2::CENTER_CENTER, sub, t::body(15.0), t::TEXT_MUTED);
    }
}

fn is_auto(cs: &lenny_control_state) -> bool {
    cs.af_mode == 0 && cs.exposure_comp == 0 && cs.exposure_lock == 0 && cs.wb_lock == 0
}

// ---- cards --------------------------------------------------------------------------------------------------

impl App {
    fn cards(&mut self, ui: &mut Ui) {
        let Ok(e) = &self.engine else {
            t::card(ui, "Lenny Desktop", |ui| {
                ui.label(RichText::new(self.engine.as_ref().err().cloned().unwrap_or_default()).color(t::CORAL));
            });
            return;
        };
        let streaming = e.session.state() == LENNY_STATE_STREAMING as i32;
        let gap = 16.0;
        self.connection_card(ui);
        if streaming {
            ui.add_space(gap);
            self.camera_card(ui);
            ui.add_space(gap);
            self.mode_card(ui);
            ui.add_space(gap);
            self.video_card(ui);
        }
        ui.add_space(gap);
        self.stream_card(ui);
    }

    fn connection_card(&mut self, ui: &mut Ui) {
        let Ok(e) = &self.engine else { return };
        let streaming = e.session.state() == LENNY_STATE_STREAMING as i32;
        t::card(ui, "Connection", |ui| {
            if streaming {
                let (_, peer) = e.session.peer();
                let (_, cs) = e.session.control_state();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("📱").font(t::body(22.0)));
                    ui.label(RichText::new(c_chars(&peer.name)).font(t::heading(22.0)));
                    if cs.battery <= 100 {
                        let charging = if cs.charging != 0 { " ⚡" } else { "" };
                        ui.label(
                            RichText::new(format!("{}%{charging}", cs.battery))
                                .font(t::mono(15.0))
                                .color(t::TEXT_MUTED),
                        );
                    }
                });
                if t::button(ui, "Disconnect", Kind::Destructive, true).clicked() {
                    // GOODBYE(USER): the phone stops retrying. Listening restarts for the next phone.
                    e.session.disconnect();
                    let s = e.session.clone();
                    std::thread::spawn(move || {
                        // start() refuses until the old I/O thread has finished closing.
                        for _ in 0..60 {
                            std::thread::sleep(Duration::from_millis(50));
                            if s.start() == LENNY_OK {
                                return;
                            }
                        }
                        log::error!("couldn't start listening again after disconnect");
                    });
                }
                return;
            }
            // Not connected: the QR code (single-use token), then where to type it in by hand.
            let uri = e.pair_uri();
            if self.qr.as_ref().is_none_or(|(u, _)| *u != uri) {
                self.qr = qr_texture(ui.ctx(), &uri).map(|tex| (uri.clone(), tex));
            }
            if let Some((_, tex)) = &self.qr {
                // The QR sits in a flat white box with a quiet zone; no outline or shadow on the code itself.
                let side = ui.available_width().min(260.0);
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), side), Sense::hover());
                let box_ = Rect::from_center_size(r.center(), Vec2::splat(side));
                ui.painter().rect_filled(box_, CornerRadius::same(t::R_TILE), Color32::from_rgb(0xFA, 0xFA, 0xF7));
                egui::Image::new(tex).paint_at(ui, box_.shrink(side * 0.08));
            }
            ui.label(
                RichText::new("Scan with Lenny on your phone. The code changes every 80 s and works once.")
                    .color(t::TEXT_MUTED)
                    .font(t::body(14.0)),
            );
            ui.label(RichText::new("OR TYPE THIS ADDRESS").font(t::mono(12.0)).color(t::TEXT_MUTED));
            ui.horizontal_wrapped(|ui| {
                for ip in local_ipv4s() {
                    t::copy_chip(ui, &ip);
                }
                t::copy_chip(ui, &e.port().to_string());
            });
            let known = e.known_phones();
            if !known.is_empty() {
                ui.label(RichText::new("KNOWN PHONES").font(t::mono(12.0)).color(t::TEXT_MUTED));
                ui.label(
                    RichText::new("They reconnect by themselves, no prompt.").color(t::TEXT_MUTED).font(t::body(13.0)),
                );
                for (id, name) in known {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("📱  {name}")).font(t::body(15.0)));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if t::icon_button(ui, "🗑", "Forget (asks again next time Lenny starts)", t::SURFACE, 36.0)
                                .clicked()
                            {
                                e.forget(&id);
                            }
                        });
                    });
                }
            }
        });
    }

    fn camera_card(&mut self, ui: &mut Ui) {
        let Ok(e) = &self.engine else { return };
        let (_, peer) = e.session.peer();
        let (_, cs) = e.session.control_state();
        let caps = e.session.peer_caps().unwrap_or_default();
        t::card(ui, "Camera", |ui| {
            if caps.lenses.len() > 1 && peer.controls & LENNY_CAP_LENS != 0 {
                let labels: Vec<String> =
                    caps.lenses.iter().map(|l| String::from_utf8_lossy(&l.label).into_owned()).collect();
                if let Some(i) = t::segmented(ui, "lens", &labels, Some(cs.lens_id as usize)) {
                    e.control(lenny_control_cmd::LENNY_CTL_SELECT_LENS, 0, 0, caps.lenses[i].id as i32);
                }
            }
            let lens = caps.lenses.get(cs.lens_id as usize);
            if let Some(l) = lens.filter(|l| l.zoom_max > 100 && peer.controls & LENNY_CAP_ZOOM != 0) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Zoom").font(t::body(16.0)).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{:.1}×", cs.zoom as f32 / 100.0))
                                .font(t::mono(15.0))
                                .color(t::LILAC),
                        );
                    });
                });
                let lo = l.zoom_min.max(1) as f32;
                if let Some(v) = t::slider(ui, "zoom", cs.zoom as f32, lo..=l.zoom_max as f32, 10.0) {
                    e.control(lenny_control_cmd::LENNY_CTL_ZOOM, 0, 0, v as i32);
                }
                if peer.controls & LENNY_CAP_PAN != 0 && cs.zoom > 100 {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Drag the preview to move around.").color(t::TEXT_MUTED).font(t::body(13.0)),
                        );
                        let centre =
                            egui::Label::new(RichText::new("Centre").color(t::LILAC).underline()).sense(Sense::click());
                        if (cs.pan_x, cs.pan_y) != (PAN_CENTER, PAN_CENTER)
                            && ui.add(centre).on_hover_cursor(CursorIcon::PointingHand).clicked()
                        {
                            e.control(lenny_control_cmd::LENNY_CTL_PAN, PAN_CENTER, PAN_CENTER, 0);
                        }
                    });
                }
            }
            if peer.controls & LENNY_CAP_TORCH != 0 {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("🔦  Torch").font(t::body(16.0)).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if t::switch(ui, "torch", cs.torch != 0) {
                            e.control(lenny_control_cmd::LENNY_CTL_TORCH, 0, 0, (cs.torch == 0) as i32);
                        }
                    });
                });
            }
        });
    }

    /// Task 5: Auto | Manual, always both visible. Manual shows tap-to-focus, EV and exposure lock.
    fn mode_card(&mut self, ui: &mut Ui) {
        let Ok(e) = &self.engine else { return };
        let (_, peer) = e.session.peer();
        let (_, cs) = e.session.control_state();
        let manual = self.manual || !is_auto(&cs);
        let mut go_manual = None;
        t::card(ui, "Focus & exposure", |ui| {
            let labels = ["Auto".to_string(), "Manual".to_string()];
            match t::segmented(ui, "mode", &labels, Some(manual as usize)) {
                Some(0) => {
                    go_manual = Some(false);
                    if !is_auto(&cs) {
                        e.control(lenny_control_cmd::LENNY_CTL_RESET_AUTO, 0, 0, 0);
                    }
                }
                Some(_) => go_manual = Some(true),
                None => {}
            }
            if !manual {
                ui.label(
                    RichText::new("Continuous autofocus and auto exposure.").color(t::TEXT_MUTED).font(t::body(14.0)),
                );
                return;
            }
            if peer.controls & LENNY_CAP_FOCUS != 0 {
                let text = if cs.af_mode != 0 {
                    "Focus held. Tap the preview to focus somewhere else."
                } else {
                    "Tap the preview to focus there."
                };
                ui.label(RichText::new(text).color(t::TEXT_MUTED).font(t::body(14.0)));
            }
            if peer.controls & LENNY_CAP_EXPOSURE_COMP != 0 && peer.exposure_max > peer.exposure_min {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Exposure").font(t::body(16.0)).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let ev = cs.exposure_comp as f32 / 1000.0;
                        let lock = if cs.exposure_lock != 0 { "Locked · " } else { "" };
                        ui.label(RichText::new(format!("{lock}{ev:+.1} EV")).font(t::mono(15.0)).color(t::LILAC));
                    });
                });
                let range = peer.exposure_min as f32..=peer.exposure_max as f32;
                if let Some(v) = t::slider(ui, "ev", cs.exposure_comp as f32, range, peer.exposure_step_milli as f32) {
                    e.control(lenny_control_cmd::LENNY_CTL_EXPOSURE_COMP, 0, 0, v.round() as i32);
                }
            }
            if peer.controls & LENNY_CAP_EXPOSURE_LOCK != 0 && t::toggle(ui, "Lock exposure", cs.exposure_lock != 0) {
                e.control(lenny_control_cmd::LENNY_CTL_EXPOSURE_LOCK, 0, 0, (cs.exposure_lock == 0) as i32);
            }
        });
        if let Some(m) = go_manual {
            self.manual = m;
        }
    }

    /// Task 3: aspect / resolution / frame rate, only from what the selected lens really streams.
    fn video_card(&mut self, ui: &mut Ui) {
        let Ok(e) = &self.engine else { return };
        let (ok, cur) = e.session.stream_settings();
        let (_, cs) = e.session.control_state();
        let Some(caps) = e.session.peer_caps() else { return };
        let modes =
            caps.lenses.get(cs.lens_id as usize).map(|l| &l.modes).filter(|m| !m.is_empty()).unwrap_or(&caps.modes);
        if !ok || modes.is_empty() {
            return;
        }
        let cur = cur.mode;
        let fps = |m: &lenny_mode| m.fps_num as u32 / m.fps_den.max(1) as u32;
        let aspect = |m: &lenny_mode| {
            let g = gcd(m.width as u32, m.height as u32).max(1);
            format!("{}:{}", m.width as u32 / g, m.height as u32 / g)
        };
        let mut aspects: Vec<String> = vec![];
        for m in modes {
            if !aspects.contains(&aspect(m)) {
                aspects.push(aspect(m));
            }
        }
        let a = aspect(&cur);
        let mut heights: Vec<u16> = modes.iter().filter(|m| aspect(m) == a).map(|m| m.height).collect();
        heights.sort();
        heights.dedup();
        let mut rates: Vec<u32> = modes.iter().filter(|m| aspect(m) == a && m.height == cur.height).map(fps).collect();
        rates.sort();
        rates.dedup();
        let mut pick: Option<lenny_mode> = None;
        let nearest = |a: &str, h: u16, f: u32| {
            modes
                .iter()
                .min_by_key(|m| {
                    ((aspect(m) != a) as u32, (m.height as i32 - h as i32).unsigned_abs(), fps(m).abs_diff(f))
                })
                .copied()
        };
        t::card(ui, "Video", |ui| {
            let row = |ui: &mut Ui, label: &str, id: &str, items: Vec<String>, sel: Option<usize>| -> Option<usize> {
                ui.label(RichText::new(label).font(t::body(14.0)).strong());
                t::segmented(ui, id, &items, sel)
            };
            if aspects.len() > 1 {
                if let Some(i) = row(ui, "Aspect", "aspect", aspects.clone(), aspects.iter().position(|x| *x == a)) {
                    pick = nearest(&aspects[i], cur.height, fps(&cur));
                }
            }
            let hl: Vec<String> =
                heights.iter().map(|h| if *h == 2160 { "4K".into() } else { format!("{h}p") }).collect();
            if let Some(i) = row(ui, "Resolution", "res", hl, heights.iter().position(|h| *h == cur.height)) {
                pick = nearest(&a, heights[i], fps(&cur));
            }
            let rl: Vec<String> = rates.iter().map(|r| r.to_string()).collect();
            if let Some(i) = row(ui, "Frame rate", "fps", rl, rates.iter().position(|r| *r == fps(&cur))) {
                pick = nearest(&a, cur.height, rates[i]);
            }
        });
        if let Some(m) = pick.filter(|m| *m != cur) {
            let (_, s) = e.session.stream_settings();
            e.session.select_stream(&lenny_stream_settings { mode: m, ..s });
        }
    }

    fn stream_card(&mut self, ui: &mut Ui) {
        let Ok(e) = &self.engine else { return };
        let streaming = e.session.state() == LENNY_STATE_STREAMING as i32;
        let st = e.session.stats();
        let (vcam_real, vcam) = e.vcam_status();
        t::card(ui, "Stream", |ui| {
            if streaming {
                let res = self.video.as_ref().map_or("—".into(), |v| format!("{}×{}", v.source.0, v.source.1));
                let ms = |us: i64| if us < 0 { "—".to_string() } else { format!("{:.0} ms", us as f64 / 1000.0) };
                let tiles = [
                    ("Resolution", res),
                    ("Frame rate", format!("{:.0} fps", self.rates.shown_fps)),
                    ("Bitrate", format!("{:.1} Mbps", self.rates.kbps / 1000.0)),
                    ("Latency", ms(st.latency_us)),
                    ("Round trip", ms(st.rtt_us)),
                    ("Received", format!("{:.0} fps", self.rates.fps)),
                ];
                egui::Grid::new("stats")
                    .num_columns(2)
                    .spacing([10.0, 10.0])
                    .min_col_width((ui.available_width() - 10.0) / 2.0)
                    .show(ui, |ui| {
                        for (i, (l, v)) in tiles.iter().enumerate() {
                            t::stat_tile(ui, l, v);
                            if i % 2 == 1 {
                                ui.end_row();
                            }
                        }
                    });
            }
            // Status only; the pipeline is the same whichever backend loaded.
            let (color, text) = if vcam_real {
                (t::GREEN, "Virtual camera: active")
            } else {
                (t::LILAC, "Virtual camera: unavailable in this environment")
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                ui.painter().circle(r.center(), 6.0, color, Stroke::new(t::BORDER, t::INK));
                ui.label(RichText::new(text).font(t::body(15.0)).strong());
            });
            ui.label(RichText::new(vcam).font(t::mono(12.0)).color(t::TEXT_MUTED));
        });
    }

    fn approval_dialog(&mut self, ctx: &egui::Context) {
        let Ok(e) = &self.engine else { return };
        if e.session.state() != LENNY_STATE_AWAITING_APPROVAL as i32 {
            return;
        }
        let name = c_chars(&e.session.peer().1.name);
        // Flat ink scrim at 60 %, no blur (§4 dialogs).
        let screen = ctx.screen_rect();
        egui::Area::new(Id::new("scrim")).order(egui::Order::Foreground).fixed_pos(Pos2::ZERO).show(ctx, |ui| {
            ui.painter().rect_filled(screen, 0.0, t::INK.gamma_multiply(0.6));
            ui.allocate_rect(screen, Sense::click()); // swallow clicks behind the dialog
        });
        egui::Area::new(Id::new("approve")).order(egui::Order::Tooltip).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(
            ctx,
            |ui| {
                ui.set_width((screen.width() - 48.0).clamp(280.0, 440.0));
                t::card_frame(t::SURFACE, t::SH_HERO).show(ui, |ui| {
                    ui.label(RichText::new("Allow this phone?").font(t::heading(26.0)));
                    ui.label(
                        RichText::new(format!(
                            "“{name}” wants to stream to this PC. Allowed phones reconnect by themselves."
                        ))
                        .color(t::TEXT_MUTED),
                    );
                    ui.columns(2, |c| {
                        if t::button(&mut c[0], "Decline", Kind::Neutral, true).clicked() {
                            e.session.approve(false);
                        }
                        if t::button(&mut c[1], "Allow", Kind::Primary, true).clicked() {
                            e.session.approve(true);
                        }
                    });
                });
            },
        );
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

fn qr_texture(ctx: &egui::Context, text: &str) -> Option<TextureHandle> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let n = code.width();
    let quiet = 4; // modules of quiet zone (§10.1)
    let side = n + 2 * quiet;
    let colors = code.to_colors();
    let mut img = ColorImage::filled([side, side], Color32::from_rgb(0xFA, 0xFA, 0xF7));
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == qrcode::Color::Dark {
                img[(x + quiet, y + quiet)] = t::INK;
            }
        }
    }
    Some(ctx.load_texture("qr", img, TextureOptions::NEAREST))
}
