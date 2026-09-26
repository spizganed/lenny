//! Lenny Desktop: the receiver app. Rust over eframe/egui (winit), linking the core crate directly (no FFI).
//!
//! Usage: lenny-desktop [--port N] [--screenshot out.png [--after SECONDS]] [--size WxH]
//!   --screenshot renders the window, saves it after SECONDS (default 3) and quits (docs, smoke test under Xvfb).

use lenny_desktop::ui;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let port = arg("--port").and_then(|p| p.parse().ok()).unwrap_or(lenny_core::LENNY_DEFAULT_PORT);
    let after = std::time::Duration::from_secs_f32(arg("--after").and_then(|s| s.parse().ok()).unwrap_or(3.0));
    let screenshot = arg("--screenshot").map(|p| (std::path::PathBuf::from(p), after));
    let size = arg("--size")
        .and_then(|s| s.split_once('x').and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?])))
        .unwrap_or([1360.0, 860.0]);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Lenny Desktop")
            .with_app_id("com.spizganed.lenny")
            // Borderless: the title bar, window buttons, drag and resize are ours (ui.rs), drawn per docs/design.md.
            .with_decorations(false)
            .with_inner_size(size)
            .with_min_inner_size([420.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("Lenny Desktop", options, Box::new(move |cc| Ok(Box::new(ui::App::new(cc, port, screenshot)))))
}
