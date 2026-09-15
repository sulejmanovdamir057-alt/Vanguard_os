use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
// ФИКС: Добавляем импорт thread прямо в эту строку:
use std::thread;
use std::time::{Duration, Instant};
use chrono::Local;
use eframe::egui;
use glob::glob;
use rand::Rng;

use crate::telemetry::SystemMetrics;
use crate::shell::{VanguardShell, ShellAction};
use crate::ui_window::VanguardWindow;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum BootStage { BarLoading, KernelLogs, Ready, Explosion, Desktop }

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum CleanerState { Idle, ScanSwap, EraseCache, OptimizeVram, Completed }

pub struct Particle { pub x: f32, pub y: f32, pub vx: f32, pub vy: f32, pub life: f32 }

pub struct VanguardOSApp {
    win_w: f32, win_h: f32, cx: f32, cy: f32,
    pub boot_stage: BootStage, progress: f32, angle: f32,
    user_glitch_trigger: bool, particles: Vec<Particle>,
    current_dx: f32, current_dy: f32, current_logo_color: egui::Color32, is_glitching_now: bool,
    system_logs: Vec<&'static str>, log_lines: Vec<String>, current_log_index: usize,
    user_input: String, ai_text_area: String,
    metrics_link: Arc<Mutex<SystemMetrics>>, cpu_history: VecDeque<f32>,
    shell: VanguardShell,
    notes_window: VanguardWindow, matrix_window: VanguardWindow, cleaner_window: VanguardWindow, apps_window: VanguardWindow,
    notes_text: String, matrix_drops: Vec<i32>, matrix_symbols: Vec<&'static str>,
    cleaner_state: CleanerState, cleaner_timer: Option<Instant>,
    ai_channel_rx: Option<Receiver<String>>, is_ai_streaming: bool,
}

impl VanguardOSApp {
    pub fn new(cc: &eframe::CreationContext<'_>, metrics: Arc<Mutex<SystemMetrics>>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(0, 0, 0);
        cc.egui_ctx.set_visuals(visuals);

        let mut cpu_hist = VecDeque::with_capacity(20);
        for _ in 0..20 { cpu_hist.push_back(0.0); }

        Self {
            win_w: 1024.0, win_h: 768.0, cx: 512.0, cy: 284.0,
            boot_stage: BootStage::BarLoading, progress: 0.0, angle: 0.0,
            user_glitch_trigger: false, particles: Vec::new(),
            current_dx: 0.0, current_dy: 0.0, current_logo_color: egui::Color32::WHITE, is_glitching_now: false,
            system_logs: vec![
                ">> INITIALIZING VANGUARD MICROKERNEL...", ">> CHECKING CPU CORES... 8/8 ONLINE [V_HYPER_MODE ENABLED]",
                ">> MOUNTING ENCRYPTED FILE SYSTEM (ZFS_SUVEREIGN_STORAGE)...", ">> BYPASSING MICROSOFT TELEMETRY BLOCKS... SUCCESS",
                ">> DETECTING GRAPHICS ADAPTER... NVIDIA CUDA CORES ACTIVE", ">> LOADING LOCAL AI MODULE: QWEN2.5-CODER (7B NEURAL CORE)...",
                ">> ALLOCATING 6.2GB VRAM FOR ON-DEVICE LINGUISTIC AGENT...", ">> AI CORE COUPLING... 100% STABLE",
                ">> SECURING NETWORK INFRASTRUCTURE (SEVERSK_GATE_V2)...", ">> VANGUARD ENVIRONMENT INITIALIZED SUCCESSFULLY.",
            ],
            log_lines: Vec::new(), current_log_index: 0, user_input: String::new(),
            ai_text_area: String::from("VANGUARD CORE: Awaiting input command...\n"),
            metrics_link: metrics, cpu_history: cpu_hist, shell: VanguardShell::new(),
            notes_window: VanguardWindow::new("Vanguard Notes", 400.0, 350.0),
            matrix_window: VanguardWindow::new("SysMatrix Stream", 300.0, 400.0),
            cleaner_window: VanguardWindow::new("Core Cleaner v1.1", 400.0, 250.0),
            apps_window: VanguardWindow::new("Vanguard App Registry", 450.0, 300.0),
            notes_text: String::from("# VANGUARD OS USER NOTES\n# Write your code architecture here...\n\n"),
            matrix_drops: vec![0; 15], matrix_symbols: vec!["0", "1", "X", "V", "A", "N", "G", "U", "A", "R", "D"],
            cleaner_state: CleanerState::Idle, cleaner_timer: None, ai_channel_rx: None, is_ai_streaming: false,
        }
    }

    fn init_explosion_particles(&mut self) {
        let mut rng = rand::thread_rng();
        self.particles.clear();
        for _ in 0..100 {
            let angle: f32 = rng.gen_range(0.0..std::f32::consts::TAU);
            let speed: f32 = rng.gen_range(3.0..9.0);
            self.particles.push(Particle { x: self.cx, y: self.cy, vx: angle.cos() * speed, vy: angle.sin() * speed, life: rng.gen_range(10.0..25.0) });
        }
    }

    pub fn handle_boot_input(&mut self, ctx: &egui::Context) {
        ctx.input(|i| {
            if !i.keys_down.is_empty() {
                if self.boot_stage == BootStage::BarLoading || self.boot_stage == BootStage::KernelLogs { self.user_glitch_trigger = true; }
            }
            if self.boot_stage == BootStage::Ready && (i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space) || !i.keys_down.is_empty()) {
                self.boot_stage = BootStage::Explosion;
                self.init_explosion_particles();
            }
        });
    }

    pub fn step_boot_logic(&mut self) {
        if self.boot_stage == BootStage::Desktop { return; }
        let mut rng = rand::thread_rng();
        self.is_glitching_now = if self.user_glitch_trigger { rng.gen_bool(0.45) } else { rng.gen_bool(0.07) };
        self.user_glitch_trigger = false;

        self.current_dx = if self.is_glitching_now { rng.gen_range(-25.0..25.0) } else { 0.0 };
        self.current_dy = if self.is_glitching_now { rng.gen_range(-8.0..8.0) } else { 0.0 };
        self.current_logo_color = if self.is_glitching_now {
            match rng.gen_range(0..3) {
                0 => egui::Color32::WHITE, 1 => egui::Color32::from_rgb(255, 0, 0), _ => egui::Color32::from_rgb(20, 20, 20)
            }
        } else { egui::Color32::WHITE };

        match self.boot_stage {
            BootStage::BarLoading => {
                if self.progress < 100.0 { self.progress += 2.0; } else { self.boot_stage = BootStage::KernelLogs; }
                self.angle += 0.15;
            }
            BootStage::KernelLogs => {
                self.angle += 0.15;
                if rng.gen_bool(0.4) && self.current_log_index < self.system_logs.len() {
                    self.log_lines.push(self.system_logs[self.current_log_index].to_string());
                    self.current_log_index += 1;
                    if self.log_lines.len() > 4 { self.log_lines.remove(0); }
                }
                if self.current_log_index >= self.system_logs.len() { self.boot_stage = BootStage::Ready; }
            }
            BootStage::Explosion => {
                let mut active_particles = false;
                for p in &mut self.particles {
                    if p.life > 0.0 { active_particles = true; p.x += p.vx; p.y += p.vy; p.life -= 0.45; }
                }
                if !active_particles { self.boot_stage = BootStage::Desktop; }
            }
            _ => {}
        }
    }

    pub fn draw_boot_screen(&self, painter: &egui::Painter) {
        let cx = self.cx; let cy = self.cy; let dx = self.current_dx; let dy = self.current_dy; let logo_color = self.current_logo_color;

        if self.boot_stage != BootStage::Explosion {
            painter.add(egui::Shape::convex_polygon(vec![egui::pos2(cx - 50.0 + dx, cy - 60.0 + dy), egui::pos2(cx - 25.0 + dx, cy - 60.0 + dy), egui::pos2(cx - 5.0 + dx, cy + 10.0 + dy), egui::pos2(cx - 30.0 + dx, cy + 10.0 + dy)], logo_color, egui::Stroke::NONE));
            painter.add(egui::Shape::convex_polygon(vec![egui::pos2(cx - 30.0 + dx, cy + 15.0 + dy), egui::pos2(cx - 5.0 + dx, cy + 15.0 + dy), egui::pos2(cx + dx, cy + 30.0 + dy), egui::pos2(cx - 25.0 + dx, cy + 30.0 + dy)], logo_color, egui::Stroke::NONE));
            painter.add(egui::Shape::convex_polygon(vec![egui::pos2(cx + 25.0 + dx, cy - 60.0 + dy), egui::pos2(cx + 50.0 + dx, cy - 60.0 + dy), egui::pos2(cx + 30.0 + dx, cy + 10.0 + dy), egui::pos2(cx + 5.0 + dx, cy + 10.0 + dy)], logo_color, egui::Stroke::NONE));
            painter.add(egui::Shape::convex_polygon(vec![egui::pos2(cx + 5.0 + dx, cy + 15.0 + dy), egui::pos2(cx + 30.0 + dx, cy + 15.0 + dy), egui::pos2(cx + 25.0 + dx, cy + 30.0 + dy), egui::pos2(cx + dx, cy + 30.0 + dy)], logo_color, egui::Stroke::NONE));

            let mut rng = rand::thread_rng();
            let radius = if self.is_glitching_now { 75.0 + rng.gen_range(-20.0..20.0) } else { 75.0 };
            let mut points = Vec::with_capacity(3);
            for i in 0..3 {
                let local_angle = self.angle + (i as f32 * (2.0 * std::f32::consts::PI / 3.0));
                points.push(egui::pos2(cx + radius * local_angle.cos(), cy + 5.0 + radius * local_angle.sin()));
            }
            let orb_color = if self.is_glitching_now && rng.gen_bool(0.5) { egui::Color32::WHITE } else { egui::Color32::from_rgb(255, 0, 0) };
            painter.add(egui::Shape::convex_polygon(points, egui::Color32::TRANSPARENT, egui::Stroke::new(3.0_f32, orb_color)));

            let text_dx = if self.is_glitching_now { rng.gen_range(-10.0..10.0) } else { 0.0 };
            painter.text(egui::pos2(cx + text_dx, cy + 220.0), egui::Align2::CENTER_CENTER, "VANGUARD", egui::FontId::monospace(64.0), egui::Color32::WHITE);
        }

        match self.boot_stage {
            BootStage::BarLoading => {
                let bar_width = 400.0;
                let bx1 = cx - bar_width / 2.0;
                let by1 = cy + 300.0;
                let bx2 = cx + bar_width / 2.0;
                let by2 = by1 + 6.0;

                painter.rect_filled(egui::Rect::from_min_max(egui::pos2(bx1, by1), egui::pos2(bx2, by2)), 0.0, egui::Color32::from_rgb(34, 34, 34));
                let fill_width = (bar_width * self.progress) / 100.0;

                if fill_width > 0.0 {
                    painter.rect_filled(egui::Rect::from_min_max(egui::pos2(bx1, by1), egui::pos2(bx1 + fill_width, by2)), 0.0, egui::Color32::from_rgb(255, 0, 0));
                }

                painter.text(egui::pos2(cx, cy + 340.0), egui::Align2::CENTER_CENTER, format!(">> INITIATING BOOT SEQUENCE... {:.0}%", self.progress), egui::FontId::monospace(12.0), egui::Color32::from_rgb(255, 0, 0));
            }
            BootStage::KernelLogs => {
                let start_y = cy + 290.0;
                for (i, line) in self.log_lines.iter().enumerate() {
                    let color = if i == self.log_lines.len() - 1 {
                        egui::Color32::from_rgb(255, 0, 0)
                    } else {
                        egui::Color32::from_rgb(85, 0, 0)
                    };
                    painter.text(egui::pos2(cx, start_y + (i as f32 * 22.0)), egui::Align2::CENTER_CENTER, line, egui::FontId::monospace(11.0), color);
                }
            }
            BootStage::Ready => {
                painter.text(egui::pos2(cx, cy + 310.0), egui::Align2::CENTER_CENTER, ">> VANGUARD CORE IS FULLY ARMED", egui::FontId::monospace(14.0), egui::Color32::WHITE);
                let prompt_color = if rand::thread_rng().gen_bool(0.6) { egui::Color32::from_rgb(255, 0, 0) } else { egui::Color32::TRANSPARENT };
                painter.text(egui::pos2(cx, cy + 350.0), egui::Align2::CENTER_CENTER, "[ PRESS ANY KEY TO ENTER ENVIRONMENT ]", egui::FontId::monospace(12.0), prompt_color);
            }
            BootStage::Explosion => {
                for p in &self.particles {
                    if p.life > 0.0 {
                        painter.circle_filled(egui::pos2(p.x, p.y), p.life / 2.0, egui::Color32::from_rgb(255, 0, 0));
                    }
                }
            }
            _ => {}
        }
    }
    pub fn draw_desktop_gui(&mut self, ui: &mut egui::Ui) {
        let painter = ui.painter();
        let w = self.win_w;
        let h = self.win_h;

        // 1. Отрисовка фоновой неоновой сетки киберпанка
        let grid_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(5, 5, 5));
        for x in (0..(w as i32)).step_by(80) {
            painter.line_segment([egui::pos2(x as f32, 0.0), egui::pos2(x as f32, h)], grid_stroke);
        }
        for y in (0..(h as i32)).step_by(80) {
            painter.line_segment([egui::pos2(0.0, y as f32), egui::pos2(w, y as f32)], grid_stroke);
        }

        // 2. Верхний статус-бар операционной системы
        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(w, 45.0)), 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(w, 45.0)), 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(34, 34, 34)));
        painter.text(egui::pos2(30.0, 22.0), egui::Align2::LEFT_CENTER, "VANGUARD CORE ENVIRONMENT v0.2_NATIVE (RUST)", egui::FontId::monospace(12.0), egui::Color32::from_rgb(255, 0, 0));

        let current_time = Local::now().format("%H:%M:%S | %d.%m.%Y").to_string();
        painter.text(egui::pos2(w - 30.0, 22.0), egui::Align2::RIGHT_CENTER, current_time, egui::FontId::monospace(12.0), egui::Color32::WHITE);

        // 3. Атомарное извлечение метрик из фонового демона телеметрии
        let mut ram_text = String::new();
        let mut disk_text = String::new();
        let mut ram_percent = 0.0;

        if let Ok(metrics) = self.metrics_link.lock() {
            self.cpu_history.pop_front();
            self.cpu_history.push_back(metrics.current_cpu_load);
            ram_percent = metrics.current_ram_percent;
            ram_text = metrics.ram_info_text.clone();
            disk_text = metrics.disk_info_text.clone();
        }

        // 4. Отрисовка виджета графиков CPU
        let wx = 40.0;
        let wy = 80.0;
        let graph_rect = egui::Rect::from_min_max(egui::pos2(wx, wy), egui::pos2(wx + 300.0, wy + 200.0));
        painter.rect_filled(graph_rect, 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(graph_rect, 0.0, egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 0, 0)));
        painter.text(egui::pos2(wx + 15.0, wy + 20.0), egui::Align2::LEFT_CENTER, ">> CPU CORE INSTABILITY GRAPH", egui::FontId::monospace(11.0), egui::Color32::WHITE);

        let line_stroke = egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 0, 0));
        for i in 0..(self.cpu_history.len() - 1) {
            let x1 = wx + 15.0 + (i as f32 * 14.0);
            let y1 = wy + 170.0 - (self.cpu_history[i] * 1.2);
            let x2 = wx + 15.0 + ((i + 1) as f32 * 14.0);
            let y2 = wy + 170.0 - (self.cpu_history[i + 1] * 1.2);
            painter.line_segment([egui::pos2(x1, y1), egui::pos2(x2, y2)], line_stroke);
        }
        painter.text(egui::pos2(wx + 15.0, wy + 185.0), egui::Align2::LEFT_CENTER, format!("CURRENT_LOAD: {:.1}% | KERNEL: ACTIVE", self.cpu_history.back().unwrap_or(&0.0)), egui::FontId::monospace(9.0), egui::Color32::from_rgb(85, 85, 85));

        // 5. Виджет оперативной памяти (RAM)
        let rx = 40.0;
        let ry = 340.0;
        let ram_rect = egui::Rect::from_min_max(egui::pos2(rx, ry), egui::pos2(rx + 300.0, ry + 80.0));
        painter.rect_filled(ram_rect, 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(ram_rect, 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(255, 0, 0)));
        painter.text(egui::pos2(rx + 15.0, ry + 20.0), egui::Align2::LEFT_CENTER, &ram_text, egui::FontId::monospace(10.0), egui::Color32::WHITE);
        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rx + 15.0, ry + 40.0), egui::pos2(rx + 285.0, ry + 50.0)), 0.0, egui::Color32::from_rgb(34, 34, 34));

        let fill_w = (270.0 * ram_percent) / 100.0;
        if fill_w > 0.0 {
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rx + 15.0, ry + 40.0), egui::pos2(rx + 15.0 + fill_w, ry + 50.0)), 0.0, egui::Color32::from_rgb(255, 0, 0));
        }

        // 6. Виджет накопителя (Disk Mappings)
        let dx = 40.0;
        let dy = 440.0;
        let disk_rect = egui::Rect::from_min_max(egui::pos2(dx, dy), egui::pos2(dx + 300.0, dy + 60.0));
        painter.rect_filled(disk_rect, 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(disk_rect, 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(51, 51, 51)));
        painter.text(egui::pos2(dx + 15.0, dy + 30.0), egui::Align2::LEFT_CENTER, &disk_text, egui::FontId::monospace(10.0), egui::Color32::WHITE);

        // 7. Декоративная рамка ИИ терминала
        let tx = 370.0;
        let ty = 80.0;
        let tw = w - tx - 40.0;
        let th = h - 180.0;
        painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(tx, ty), egui::pos2(tx + tw, ty + th)), 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(51, 51, 51)));
        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(tx, ty), egui::pos2(tx + tw, ty + 35.0)), 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(tx, ty), egui::pos2(tx + tw, ty + 35.0)), 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(51, 51, 51)));
        painter.text(egui::pos2(tx + 15.0, ty + 17.0), egui::Align2::LEFT_CENTER, "INTEGRATED LOCAL AI KERNEL (QWEN2.5-CODER NATIVE)", egui::FontId::monospace(11.0), egui::Color32::from_rgb(255, 0, 0));
    }

    pub fn send_ai_query(&mut self, ctx: &egui::Context) {
        let query = self.user_input.trim().to_string();
        if query.is_empty() { return; }

        // 1. Дублируем ввод в интерфейс окна
        self.ai_text_area.push_str(&format!("\nvanguard_root@kernel_sh:~# {}\n", query));

        // 2. ДУБЛИРУЕМ ВВОД В КОНСОЛЬНЫЙ ЛАУНЧЕР (Middle-фича)
        println!("\033[91mvanguard_root@vsh_terminal:~\033[0m {}", query);

        self.user_input.clear();

        // Передаем команду в shell.rs
        let action = self.shell.handle_command(&query);
        match action {
            ShellAction::SystemExit => {
                println!("\033[91m[SHUTDOWN]\033[0m Получен сигнал принудительного закрытия ядра.");
                self.ai_text_area.push_str(">> SHUTTING DOWN INFRASTRUCTURE... BYE.\n");
                std::process::exit(0);
            }
            ShellAction::DisplayOutput(text) => {
                // Выводим лог и в окно, и параллельно в лаунчер терминала
                self.ai_text_area.push_str(&format!("{}\n", text));
                println!("{}\n", text); // Дублирование в CMD
            }
            ShellAction::AiStreamAllowed => {
                self.ai_text_area.push_str(">> CORE LNK: Establishing neural stream...\n\n");
                println!("\033[92m[AI_STREAM]\033[0m Открытие асинхронного канала связи mpsc с Qwen Core...");
                self.is_ai_streaming = true;

                let (tx, rx): (Sender<String>, Receiver<String>) = mpsc::channel();
                self.ai_channel_rx = Some(rx);
                let egui_ctx_clone = ctx.clone();

                thread::spawn(move || {
                    let simulated_tokens = vec![
                        ">> QWEN CORE DETACHED:\n", "Привет! ", "Я ", "нативное ", "ядро ",
                        "VanguardOS, ", "работающее ", "на ", "языке ", "Rust.\n",
                        "Модуль ", "выполнения ", "команд ", "автоматизации ", "готов ", "к ", "работе.\n",
                        "```python\n", "print('Hello from Vanguard AI Automatic Coder!')\n", "```"
                    ];
                    for token in simulated_tokens {
                        if tx.send(token.to_string()).is_err() { break; }
                        egui_ctx_clone.request_repaint();
                        thread::sleep(Duration::from_millis(40));
                    }
                    let _ = tx.send("[STREAM_END]".to_string());
                    egui_ctx_clone.request_repaint();
                });
            }
        }
    }


    pub fn check_ai_stream(&mut self) {
        if !self.is_ai_streaming { return; }
        if let Some(ref rx) = self.ai_channel_rx {
            while let Ok(token) = rx.try_recv() {
                if token == "[STREAM_END]" {
                    self.is_ai_streaming = false;
                    if let Some(filename) = self.shell.ai_core.save_code_if_exists(&self.ai_text_area) {
                        self.ai_text_area.push_str(&format!("\n\n[SYSTEM] CODE AUTO-COMPILED: {}", filename));
                    }
                    break;
                }
                if self.ai_text_area.starts_with(">> CORE LNK:") { self.ai_text_area.clear(); }
                self.ai_text_area.push_str(&token);
            }
        }
    }
}

// =========================================================================
// ГЛАВНЫЙ ИНТЕРФЕЙСНЫЙ ЦИКЛ ОБНОВЛЕНИЯ ЭКРАНА СРЕДЫ EFRAME
// =========================================================================
impl eframe::App for VanguardOSApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_boot_input(ctx);
        self.step_boot_logic();
        self.check_ai_stream();

        // 📝 Окно Блокнота (Notes Workspace)
        self.notes_window.show(ctx, |ui| {
            ui.heading("📝 VANGUARD USER NOTES");
            ui.add_space(5.0);
            egui::ScrollArea::vertical().id_source("notes_scroll").show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut self.notes_text).font(egui::FontId::monospace(11.0)).text_color(egui::Color32::from_rgb(0, 255, 66)).desired_width(f32::INFINITY));
            });
        });

        // 🟢 Окно Матрицы (Digital Rain Stream)
        self.matrix_window.show(ctx, |ui| {
            ctx.request_repaint();
            let painter = ui.painter();
            let mut rng = rand::thread_rng();
            for i in 0..self.matrix_drops.len() {
                let symbol_idx = rng.gen_range(0..self.matrix_symbols.len());
                let symbol = self.matrix_symbols[symbol_idx];
                let x = (i as f32) * 20.0 + 15.0;
                let y = (self.matrix_drops[i] as f32) * 20.0 + 35.0;
                painter.text(egui::pos2(x, y), egui::Align2::CENTER_CENTER, symbol, egui::FontId::monospace(13.0), egui::Color32::from_rgb(0, 255, 66));
                if rng.gen_bool(0.12) { self.matrix_drops[i] += 1; }
                if y > 380.0 || rng.gen_bool(0.02) { self.matrix_drops[i] = 0; }
            }
        });

        // 🧹 Окно Очистителя Логов И Кэша VRAM (ФИКС МУСОРНОГО ТЕКСТА)
        self.cleaner_window.show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(5.0);
                ui.heading(">> PROTOCOL CLEANUP WORKSPACE");
                ui.add_space(10.0);

                if let Some(start) = self.cleaner_timer {
                    ctx.request_repaint();
                    let elapsed = start.elapsed().as_millis();
                    if elapsed < 800 { self.cleaner_state = CleanerState::ScanSwap; }
                    else if elapsed < 1600 { self.cleaner_state = CleanerState::EraseCache; }
                    else if elapsed < 2400 { self.cleaner_state = CleanerState::OptimizeVram; }
                    else if self.cleaner_state != CleanerState::Completed {
                        self.cleaner_state = CleanerState::Completed;
                        let _ = self.shell.handle_command("/wipe");
                    }
                }

                let (text, color) = match self.cleaner_state {
                    CleanerState::Idle => ("Status: IDLE", egui::Color32::from_rgb(85, 85, 85)),
                    CleanerState::ScanSwap => ("[1/3] Scanning swap partition...", egui::Color32::from_rgb(255, 0, 0)),
                    CleanerState::EraseCache => ("[2/3] Purging temp log caches...", egui::Color32::from_rgb(255, 0, 0)),
                    CleanerState::OptimizeVram => ("[3/3] Compacting VRAM layers...", egui::Color32::from_rgb(255, 0, 0)),
                    CleanerState::Completed => (">> ENVIRONMENT FULLY OPTIMIZED!", egui::Color32::from_rgb(0, 255, 66)),
                };

                ui.colored_label(color, egui::RichText::new(text).font(egui::FontId::monospace(11.0)));
                ui.add_space(15.0);

                if self.cleaner_state == CleanerState::ScanSwap || self.cleaner_state == CleanerState::EraseCache || self.cleaner_state == CleanerState::OptimizeVram {
                    ui.add(egui::Spinner::new());
                } else {
                    let is_idle = self.cleaner_state == CleanerState::Idle || self.cleaner_state == CleanerState::Completed;
                    let btn = ui.add_enabled(is_idle, egui::Button::new("EXECUTE OVERALL CLEANUP"));
                    if btn.clicked() {
                        self.cleaner_timer = Some(Instant::now());
                        self.cleaner_state = CleanerState::ScanSwap;
                    }
                }
            });
        });

        // 🚀 Окно Лаунчера / Реестра ИИ-софта
        self.apps_window.show(ctx, |ui| {
            ui.vertical_centered(|ui| { ui.heading("CHOOSE APPLICATION TO RUN:"); ui.add_space(5.0); });
            let mut files = Vec::new();
            if let Ok(entries) = glob("vanguard_gen_*.py") { for e in entries.flatten() { files.push(e); } }

            if files.is_empty() {
                ui.vertical_centered(|ui| { ui.colored_label(egui::Color32::from_rgb(85, 85, 85), "No compiled scripts found."); });
                return;
            }

            egui::ScrollArea::vertical().id_source("registry_scroll").show(ui, |ui| {
                for path in files {
                    let name = path.to_string_lossy().into_owned();
                    let id = if name.len() > 16 { &name[13..name.len() - 3] } else { "???" };
                    let btn = ui.add(egui::Button::new(format!("🚀 LAUNCH: vanguard_gen_{}.py", id)).fill(egui::Color32::from_rgb(16, 16, 16)).stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0, 255, 66))));
                    if btn.clicked() { let _ = self.shell.handle_command(&format!("/run {}", id)); }
                    ui.add_space(4.0);
                }
            });
        });

        // 🖥️ Рендеринг Центрального макета (Переключение Заставка / Стол)
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.boot_stage != BootStage::Desktop {
                ctx.request_repaint();
                self.draw_boot_screen(ui.painter());
            } else {
                self.draw_desktop_gui(ui);
                let icons_spec = vec![("🧠 AI CORE", 330.0), ("📝 NOTES", 390.0), ("🟢 MATRIX", 450.0), ("🧹 CLEANER", 510.0), ("🚀 LAUNCHER", 570.0)];
                let painter = ui.painter();

                for (name, iy) in icons_spec {
                    let rect = egui::Rect::from_min_max(egui::pos2(40.0, iy), egui::pos2(160.0, iy + 40.0));
                    let id = ui.make_persistent_id(name);
                    let resp = ui.interact(rect, id, egui::Sense::click());
                    let (bg, stroke, text_c) = if resp.hovered() { (egui::Color32::from_rgb(255, 0, 0), egui::Stroke::new(1.0_f32, egui::Color32::WHITE), egui::Color32::BLACK) }
                    else { (egui::Color32::from_rgb(10, 10, 10), egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(50, 50, 50)), egui::Color32::WHITE) };

                    painter.rect_filled(rect, 0.0, bg);
                    painter.rect_stroke(rect, 0.0, stroke);
                    painter.text(rect.center(), egui::Align2::CENTER_CENTER, name, egui::FontId::monospace(10.0), text_c);

                    if resp.clicked() {
                        match name {
                            "🧠 AI CORE" => { ctx.memory_mut(|m| m.request_focus(egui::Id::new("ai_input_field"))); }
                            "📝 NOTES" => self.notes_window.is_open = true,
                            "🟢 MATRIX" => self.matrix_window.is_open = true,
                            "🧹 CLEANER" => self.cleaner_window.is_open = true,
                            "🚀 LAUNCHER" => self.apps_window.is_open = true,
                            _ => {}
                        }
                    }
                }

                // Интерактивное поле вывода логов ИИ
                let tx = 370.0; let ty = 80.0; let tw = self.win_w - tx - 40.0; let th = self.win_h - 180.0;
                ui.allocate_ui_at_rect(egui::Rect::from_min_max(egui::pos2(tx + 10.0, ty + 45.0), egui::pos2(tx + tw - 10.0, ty + th - 20.0)), |ui| {
                    egui::ScrollArea::vertical().id_source("ai_chat_scroll").stick_to_bottom(true).show(ui, |ui| {
                        ui.add(egui::TextEdit::multiline(&mut self.ai_text_area).font(egui::FontId::monospace(11.0)).text_color(egui::Color32::WHITE).desired_width(f32::INFINITY).lock_focus(true));
                    });
                });

                // Строка ввода команд ШЕЛЛА
                let input_rect = egui::Rect::from_min_max(egui::pos2(370.0, self.win_h - 85.0), egui::pos2(self.win_w - 40.0, self.win_h - 45.0));
                ui.allocate_ui_at_rect(input_rect, |ui| {
                    ui.horizontal(|ui| {
                        let input_field = ui.add(egui::TextEdit::singleline(&mut self.user_input).id(egui::Id::new("ai_input_field")).desired_width(ui.available_width() - 80.0).hint_text("Введите команду ядра (/help, /apps) или запрос к ИИ..."));
                        if input_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) { self.send_ai_query(ctx); input_field.request_focus(); }
                        if ui.button("EXEC").clicked() { self.send_ai_query(ctx); }
                    });
                });
            }
        });
    }
}
