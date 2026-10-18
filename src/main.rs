//! Vanguard Sovereign OS — оболочка: egui-рендер рабочего стола в стиле Windows 11.
//!
//! Это **только графика**. Движок (шелл, исполнитель скриптов, телеметрия, компиляция)
//! живёт отдельным процессом `vanguard_core.exe` и обслуживается по IPC (см.
//! `vanguard_os_lib::ipc`). ИИ-поток (запрос к Ollama, SSE-стриминг) пока остаётся
//! здесь — он не тяжёлый и не вызывает UI-заморозок.
//!
//! Без `windows_subsystem = "windows"` Windows создала бы рядом пустое консольное окно
//! с заголовком-путём к exe. Ничего в консоль всё равно не пишем — прогресс виден
//! только в GUI-чате.
#![windows_subsystem = "windows"]

use std::sync::{Arc, Mutex};

use vanguard_os_lib::core::telemetry::{spawn_telemetry_daemon, SystemMetrics};

mod app;
mod desktop;
mod ui_window;
mod ipc_client;
mod settings;

use app::VanguardOSApp;

// Ход работы показываем только в GUI-чате: баннер и логи инициализации
// в консоль больше не дублируются.
fn main() -> eframe::Result<()> {
    // Телеметрия на этом этапе остаётся локальной (оболочка рисует графики CPU/RAM/DISK
    // прямо из этого потока). Ядро держит собственный экземпляр для /hardware —
    // когда оболочка полностью перейдёт на IPC, этот daemon переедет в ядро.
    let shared_metrics = Arc::new(Mutex::new(SystemMetrics::new()));
    spawn_telemetry_daemon(Arc::clone(&shared_metrics));

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            // 1024×768 — опорный размер раскладки (см. app::BASE_*), но не
            // потолок: update() каждый кадр читает реальный вьюпорт, поэтому
            // окно можно тянуть, разворачивать и уводить в F11-полный экран.
            .with_inner_size([app::BASE_W, app::BASE_H])
            .with_min_inner_size([app::BASE_W, app::BASE_H])
            .with_resizable(true)
            .with_decorations(true)
            .with_maximized(true)
            .with_title("Vanguard Sovereign OS — Terminal Link"),
        ..Default::default()
    };

    eframe::run_native(
        "vanguard_os_shell",
        native_options,
        Box::new(|cc| Ok(Box::new(VanguardOSApp::new(cc, shared_metrics)))),
    )
}
