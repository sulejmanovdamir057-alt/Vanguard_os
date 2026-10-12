use std::sync::{Arc, Mutex};

mod telemetry;
mod executor;
mod shell;
mod ui_window;
mod app;
mod ai_core;
mod desktop;

use telemetry::{SystemMetrics, spawn_telemetry_daemon};
use app::VanguardOSApp;

// Ход работы показываем только в GUI-чате: баннер и логи инициализации
// в консоль больше не дублируются.
fn main() -> eframe::Result<()> {
    let shared_metrics = Arc::new(Mutex::new(SystemMetrics::new()));
    spawn_telemetry_daemon(Arc::clone(&shared_metrics));

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_resizable(false)
            .with_decorations(true)
            .with_maximized(true)
            .with_title("Vanguard Sovereign OS — Terminal Link"),
        ..Default::default()
    };

    eframe::run_native(
        "vanguard_os_core",
        native_options,
        Box::new(|cc| Ok(Box::new(VanguardOSApp::new(cc, shared_metrics)))),
    )
}
