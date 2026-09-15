// =========================================================================
// ДИСПЕТЧЕР ДЕКЛАРАЦИЙ И ТОЧКА ВХОДА СУВЕРЕННОГО ЯДРА VANGUARD OS
// =========================================================================

// 1. Подключаем все наши новые внешние скрипты-модули
mod telemetry;
mod executor;
mod shell;
mod ui_window;
mod app;

use std::sync::{Arc, Mutex};
use std::process::Command;
use std::thread;
use std::time::Duration;

// Подтягиваем типы данных из модулей для инициализации в main
use telemetry::{SystemMetrics, spawn_telemetry_daemon};
use app::VanguardOSApp;

fn main() -> eframe::Result<()> {
    // Включаем поддержку ANSI-escape последовательностей (цветного текста) в консоли Windows
    let _ = upper_bar_ctrl();

    // Выводим хакерский баннер лаунчера в консоль при старте
    println!("\033[91m==================================================");
    println!("    VANGUARD OS: MULTI-THREADED TERMINAL LAUNCHER ");
    println!("==================================================\033[0m");
    println!("\033[92m[KERNEL INTERFACE ACTIVE]\033[0m Инициализация системных абстракций...");
    thread::sleep(Duration::from_millis(300));

    println!(">> [MEM] Выделение атомарного блока памяти SystemMetrics...");
    let shared_metrics = Arc::new(Mutex::new(SystemMetrics::new()));

    println!(">> [SYS] Запуск изолированного демона телеметрии железа...");
    spawn_telemetry_daemon(Arc::clone(&shared_metrics));

    println!(">> [GUI] Инициализация графического сервера egui Viewport...");
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_resizable(false)
            .with_decorations(true)
            .with_maximized(true) // Разворачиваем графическое окно на весь экран
            .with_title("Vanguard Sovereign OS — Terminal Link"),
        ..Default::default()
    };

    println!("\033[93m>> [RUN] Передача квантов времени в GUI-поток. Запуск графического ядра...\033[0m\n");
    thread::sleep(Duration::from_millis(400));

    // Запускаем окно
    eframe::run_native(
        "vanguard_os_core",
        native_options,
        Box::new(|cc| Ok(Box::new(VanguardOSApp::new(cc, shared_metrics)))),
    )
}


/// Сервисный метод выравнивания потоков вывода командной строки CMD
fn upper_bar_ctrl() -> bool {
    if cfg!(target_os = "windows") {
        return Command::new("cmd").args(&["/c", "echo off"]).status().is_ok();
    }
    true
}