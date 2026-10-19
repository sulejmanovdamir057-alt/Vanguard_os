use std::collections::VecDeque;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender}; // Объединенный импорт каналов
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use glob::glob;
use rand::Rng;

// Подключение внутренних модулей проекта
use vanguard_os_lib::ai_core::{AiEvent, VanguardAction, configured_model, configured_url};
use vanguard_os_lib::core::telemetry::SystemMetrics;
use vanguard_os_lib::core::shell::{script_id_from_path, ShellAction, VanguardShell};
use vanguard_os_lib::ipc::{FsEntry, FsFile, FsListing, Request};
use crate::desktop::{Desktop, DesktopHost, DesktopIntent, INPUT_FIELD_ID};
use crate::settings::{self, AppSettings};
use crate::ui_window::{VanguardWindows, WindowId};

/// Проводник «Файлы»: состояние окна, IPC-хелперы `fs_*` и само окно. Дочерний
/// модуль, а не братский — ему нужны приватные поля `VanguardOSApp` (см. шапку
/// модуля): private-видимость распространяется на предка и его потомков.
mod fs_explorer;
use fs_explorer::FsExplorer;


/// Файл заметков блокнота. Пишется относительно CWD — как и все артефакты проекта.
const NOTES_FILE: &str = "vanguard_notes.txt";

/// Прогулка по истории команд: `dir` = -1 (↑, старше) или +1 (↓, новее). За последней
/// командой вниз — пустой черновик. Текст поля заменяется, курсор — в конец.
///
/// Свободная функция, а не метод: вызывается из замыкания окна терминала, где
/// `&mut self` целиком уже занят отрисовкой этого окна — здесь заимствуются
/// только три нужных поля, и это компилируется.
fn walk_history(
    history: &[String],
    hist_pos: &mut Option<usize>,
    input: &mut String,
    dir: i32,
    ctx: &egui::Context,
) {
    if history.is_empty() {
        return;
    }
    let len = history.len();
    let pos = hist_pos.unwrap_or(len);
    let new_pos = if dir < 0 { pos.saturating_sub(1) } else { (pos + 1).min(len) };
    if new_pos < len {
        *input = history[new_pos].clone();
        *hist_pos = Some(new_pos);
    } else {
        input.clear();
        *hist_pos = None;
    }
    // egui держит курсор там, где был: после подстановки ставим его в конец,
    // иначе править просмотренную команду неудобно.
    let n = input.chars().count();
    if let Some(mut state) = egui::TextEdit::load_state(ctx, egui::Id::new(INPUT_FIELD_ID)) {
        let end = egui::text::CCursor::new(n);
        state.cursor = egui::text::CCursorRange::one(end).into();
        state.store(ctx, egui::Id::new(INPUT_FIELD_ID));
    }
}

/// Реестр сгенерированных скриптов, свежие сверху — раздел «Рекомендуется»
/// меню «Пуск». ID берём тем же разбором имени, что и шелл (`/run <id>`).
fn recent_scripts_scan() -> Vec<(String, String)> {
    let mut items: Vec<(std::time::SystemTime, String, String)> = Vec::new();
    if let Ok(paths) = glob("vanguard_gen_*.py") {
        for p in paths.flatten() {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let id = script_id_from_path(&name).unwrap_or_default();
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            items.push((mtime, id, name));
        }
    }
    items.sort_by(|a, b| b.0.cmp(&a.0));
    items.into_iter().take(6).map(|(_, id, name)| (id, name)).collect()
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum BootStage { BarLoading, KernelLogs, Ready, Explosion, Desktop }

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum CleanerState { Idle, ScanSwap, EraseCache, OptimizeVram, Completed }

pub struct Particle { pub x: f32, pub y: f32, pub vx: f32, pub vy: f32, pub life: f32 }

/// "http://localhost:11434/v1/chat/completions" -> ("localhost", 11434).
/// Нужен только для пробника бэкенда: грузить полноценный URL-парсер
/// ради одного host:port не стоит. Статус-строка стола берёт его из desktop.rs.
pub(crate) fn endpoint_host_port(url: &str) -> (String, u16) {
    let no_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = no_scheme.split('/').next().unwrap_or(no_scheme);
    if let Some((host, port)) = authority.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            return (host.to_string(), port);
        }
    }
    (authority.to_string(), 80)
}

/// Опорные размеры окна и центр заставки — те же числа, что уходят в
/// `ViewportBuilder::with_inner_size` (см. `main.rs`). Стол больше не привязан
/// к ним: он берёт реальный вьюпорт каждый кадр, а отсюда считаются пропорции
/// логотипа. На 1024×768 всё стоит ровно там же, что и до адаптивной
/// раскладки — это и есть опорная точка для всех констант раскладки.
pub const BASE_W: f32 = 1024.0;
pub const BASE_H: f32 = 768.0;
const BASE_CX: f32 = 512.0;
const BASE_CY: f32 = 284.0;

pub struct VanguardOSApp {
    /// Размер реального вьюпорта (обновляется каждый кадр в `update`) и центр,
    /// от которого рисуется логотип заставки. Не константы: в развёрнутом и
    /// полноэкранном окне они должны следовать за экраном.
    win_w: f32,
    win_h: f32,
    cx: f32,
    cy: f32,
    /// Текущий этап загрузки; пока не наступил `Desktop`, рисуется заставка.
    pub boot_stage: BootStage,
    /// Прогресс полосы `BarLoading` в процентах (0..100).
    progress: f32,
    /// Фаза вращения орбитального кольца вокруг логотипа.
    angle: f32,
    /// Клавиша нажата на заставке — следующий кадр усиливает глитч.
    user_glitch_trigger: bool,
    /// Частицы взрыва логотипа при переходе к рабочему столу.
    particles: Vec<Particle>,
    /// Смещение логотипа и цвет в рамках текущего глитча (ноль, когда глитча нет).
    current_dx: f32,
    current_dy: f32,
    current_logo_color: egui::Color32,
    is_glitching_now: bool,
    /// Лог загрузки: `system_logs` — весь текст, `log_lines` — последние 4 строки на экране.
    system_logs: Vec<&'static str>,
    log_lines: Vec<String>,
    current_log_index: usize,
    /// Строка ввода команд и буфер вывода чата (рисует desktop.rs).
    user_input: String,
    ai_text_area: String,
    /// Фоновая телеметрия и история загрузки CPU для графика.
    metrics_link: Arc<Mutex<SystemMetrics>>,
    cpu_history: VecDeque<f32>,
    /// Шелл: слеш-команды, диалог с моделью, выполнение действий ИИ.
    shell: VanguardShell,
    /// Все окна приложения одной группой — панель задач и иконки перебирают их
    /// через `WindowId::ALL`, поэтому раньше пришлось держать `window_ref(i)` и
    /// константу `TASKBAR_WINDOWS`, которые легко рассинхронизировать.
    windows: VanguardWindows,
    /// Состояние окон: Блокнот, Матрица, Очиститель (текст, капли, таймер).
    notes_text: String,
    /// Заметки менялись с последней записи на диск (автосейв в `update()`).
    notes_dirty: bool,
    matrix_drops: Vec<i32>,
    matrix_symbols: Vec<&'static str>,
    cleaner_state: CleanerState,
    cleaner_timer: Option<Instant>,
    // 🗂 Проводник скриптов: выбранный файл и его содержимое (читается с диска по клику).
    // explorer_status живёт отдельно от ai_text_area намеренно: писать в чат из окна нельзя,
    // пока там идёт стрим — маркер в check_ai_stream перезаписал бы этот текст.
    explorer_sel: Option<String>,
    explorer_code: String,
    explorer_status: String,
    // 🗂 Проводник «Файлы»: настоящая файловая система через IPC-ядро.
    // Отдельно от explorer_* выше — те рисуют плоский список vanguard_gen_*.py,
    // это — все диски машины с колонками и редактором.
    fs: FsExplorer,
    // 🧬 Самообучение (окно Neural Trainer): HF-модель для дообучения, счётчик
    // реплик в журнале диалогов, текстовый статус и флаг «тренировка идёт».
    trainer_hf_model: String,
    trainer_count: usize,
    trainer_status: String,
    trainer_running: Arc<AtomicBool>,
    // ⌨️ История команд строки ввода (↑/↓) — переехала сюда из Desktop вместе
    // с терминалом, ставшим обычным окном.
    cmd_history: Vec<String>,
    cmd_hist_pos: Option<usize>,
    // Просьба поставить фокус в строку ввода: TextEdit живёт в окне терминала,
    // поэтому фокус запрашиваем после создания виджета, а не по хоткею напрямую.
    focus_input_pending: bool,
    // Реестр скриптов для «Рекомендуется» меню «Пуск»; обновляется в update(),
    // пока меню открыто (glob на каждый кадр был бы лишней работой).
    recent_scripts: Vec<(String, String)>,
    /// Рабочий стол: раскладка иконок, контекстное меню и прочее собственное
    /// состояние. Всё, что рисуется на CentralPanel после заставки, живёт в desktop.rs.
    desktop: Desktop,
    // 💚 Онлайн ли бэкенд ИИ: обновляет фоновый TCP-пробник из new(), здесь только читаем
    backend_alive: Arc<AtomicBool>,
    // Кэш конфига бэкенда для статус-строки: env читаем один раз при старте, а не каждый кадр
    ai_model_name: String,
    ai_endpoint: String,
    // ⚙️ Окно «Настройки»: бэкенд ИИ и автозапуск Ollama. Применение — через env,
    // см. src/settings.rs: spawn_ai_request читает окружение на каждый запрос.
    settings: AppSettings,
    // Строка обратной связи внутри окна (запись файла, запуск демона) — как
    // explorer_status/trainer_status: писать в чат из окна нельзя, пока идёт стрим.
    settings_status: String,
    /// События фонового потока ИИ; дренажится каждый кадр в `check_ai_stream`.
    ai_channel_rx: Receiver<AiEvent>,
    /// Пока `true`, новый запрос не принимается (сбрасывается по Finished/Error).
    is_ai_streaming: bool,
    // Длина ai_text_area на момент начала стриминга: показанный превью-участок по
    // готовности ответа заменяется финальным результатом, а не дублируется.
    ai_stream_marker: Option<usize>,
    pub ai_event_tx: Sender<AiEvent>,
}


impl VanguardOSApp {
    pub fn new(cc: &eframe::CreationContext<'_>, metrics: Arc<Mutex<SystemMetrics>>) -> Self {
        let mut visuals = egui::Visuals::dark();
        // Палитра Windows 11 dark: окна #202020, рамка #3A3A3A, акцент #0078D4.
        // Красные акценты Vanguard OS остаются там, где их рисует код приложения
        // (терминал, графики, лого), а не здесь.
        visuals.panel_fill = egui::Color32::from_rgb(15, 17, 24);   // подложка под обоями
        visuals.window_fill = egui::Color32::from_rgb(32, 32, 32);
        visuals.window_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(58, 58, 58));
        visuals.window_rounding = egui::Rounding::same(8.0);
        visuals.menu_rounding = egui::Rounding::same(6.0);
        visuals.popup_shadow = egui::epaint::Shadow {
            offset: [0.0, 8.0].into(),
            blur: 24.0,
            spread: 0.0,
            color: egui::Color32::from_black_alpha(120),
        };
        visuals.extreme_bg_color = egui::Color32::from_rgb(42, 42, 46);   // поля ввода
        visuals.faint_bg_color = egui::Color32::from_rgb(38, 38, 42);
        visuals.code_bg_color = egui::Color32::from_rgb(24, 24, 28);
        visuals.selection.bg_fill = egui::Color32::from_rgb(0, 120, 212);
        visuals.hyperlink_color = egui::Color32::from_rgb(77, 166, 255);
        for w in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            w.rounding = egui::Rounding::same(4.0);
        }
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(58, 58, 62);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(74, 74, 80);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0, 120, 212);
        cc.egui_ctx.set_visuals(visuals);

        // Windows-шрифты: Segoe UI (интерфейс) и Consolas (терминал). Если файлов
        // нет (не Windows) — остаются шрифты egui, приложение не падает.
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
            fonts.font_data.insert("segoe".to_string(), egui::FontData::from_owned(bytes).into());
            fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "segoe".to_string());
        }
        // Segoe UI Symbol: ✎ ❯ ⚙ ▦ и прочие знаки интерфейса, которых нет в
        // самом Segoe UI. Стоит сразу после него в цепочке подстановки.
        if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\seguisym.ttf") {
            fonts.font_data.insert("seguisym".to_string(), egui::FontData::from_owned(bytes).into());
            fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(1, "seguisym".to_string());
        }
        let mono_path = ["C:\\Windows\\Fonts\\consola.ttf", "C:\\Windows\\Fonts\\CascadiaMono.ttf"]
            .into_iter()
            .find(|p| std::fs::metadata(p).is_ok());
        if let Some(path) = mono_path {
            if let Ok(bytes) = std::fs::read(path) {
                fonts.font_data.insert("winmono".to_string(), egui::FontData::from_owned(bytes).into());
                fonts.families.get_mut(&egui::FontFamily::Monospace).unwrap().insert(0, "winmono".to_string());
            }
        }
        cc.egui_ctx.set_fonts(fonts);

        let mut cpu_hist = VecDeque::with_capacity(20);
        for _ in 0..20 { cpu_hist.push_back(0.0); }

        // Канал событий ИИ: фоновый поток шлёт сюда Delta/Finished/Error,
        // а чат читает их каждый кадр в check_ai_stream.
        let (ai_event_tx, ai_event_rx) = mpsc::channel::<AiEvent>();

        // Шелл создаётся до сборки структуры: его счётчик bg_tasks нужен и
        // проводнику «Файлы» — там свой Arc, но именно на этот же атом.
        let shell = VanguardShell::new();
        let fs_bg = Arc::clone(&shell.bg_tasks);

        let mut app = Self {
            win_w: BASE_W, win_h: BASE_H, cx: BASE_CX, cy: BASE_CY,
            boot_stage: BootStage::BarLoading, progress: 0.0, angle: 0.0,
            user_glitch_trigger: false, particles: Vec::new(),
            current_dx: 0.0, current_dy: 0.0, current_logo_color: egui::Color32::WHITE, is_glitching_now: false,
            system_logs: vec![
                ">> INITIALIZING VANGUARD MICROKERNEL...", ">> CHECKING CPU CORES... 8/8 ONLINE [V_HYPER_MODE ENABLED]",
                ">> MOUNTING ENCRYPTED FILE SYSTEM (ZFS_SUVEREIGN_STORAGE)...", ">> BYPASSING MICROSOFT TELEMETRY BLOCKS... SUCCESS",
                ">> DETECTING GRAPHICS ADAPTER... NVIDIA CUDA CORES ACTIVE", ">> LOADING LOCAL AI MODULE: QWEN3.5 HYBRID (9B NEURAL CORE)...",
                ">> ALLOCATING 6.6GB VRAM FOR ON-DEVICE LINGUISTIC AGENT...", ">> AI CORE COUPLING... 100% STABLE",
                ">> SECURING NETWORK INFRASTRUCTURE (SEVERSK_GATE_V2)...", ">> VANGUARD ENVIRONMENT INITIALIZED SUCCESSFULLY.",
            ],
            log_lines: Vec::new(), current_log_index: 0, user_input: String::new(),
            ai_text_area: String::from("VANGUARD CORE: Awaiting input command...\n"),
            metrics_link: metrics, cpu_history: cpu_hist, shell,
            windows: VanguardWindows::new(),
            explorer_sel: None,
            explorer_code: String::new(),
            fs: FsExplorer::new(fs_bg),
            explorer_status: String::from("Выберите скрипт из списка слева."),
            // 🧬 Самообучение: модель по умолчанию — та же, что в Ollama, но в
            // формате HuggingFace; переопределяется env VANGUARD_TRAIN_HF_MODEL
            // или полем в окне Trainer.
            trainer_hf_model: std::env::var("VANGUARD_TRAIN_HF_MODEL")
                .unwrap_or_else(|_| String::from("Qwen/Qwen3.5-9B")),
            trainer_count: vanguard_os_lib::core::shell::count_dialogue_lines(),
            trainer_status: String::from("Готов к тренировке."),
            trainer_running: Arc::new(AtomicBool::new(false)),
            cmd_history: Vec::new(),
            cmd_hist_pos: None,
            focus_input_pending: false,
            recent_scripts: Vec::new(),
            desktop: Desktop::new(),
            backend_alive: Arc::new(AtomicBool::new(false)),
            ai_model_name: configured_model(),
            ai_endpoint: configured_url(),
            settings: AppSettings::load(),
            settings_status: String::new(),
            // Заметки переживают перезапуск: при отсутствии файла — прежний заголовок.
            notes_text: std::fs::read_to_string(NOTES_FILE)
                .unwrap_or_else(|_| String::from("# VANGUARD OS USER NOTES\n# Write your code architecture here...\n\n")),
            notes_dirty: false,
            matrix_drops: vec![0; 15], matrix_symbols: vec!["0", "1", "X", "V", "A", "N", "G", "U", "A", "R", "D"],
            cleaner_state: CleanerState::Idle, cleaner_timer: None,

            ai_channel_rx: ai_event_rx,
            is_ai_streaming: false,
            ai_stream_marker: None,
            ai_event_tx,
        };
        // Логи фоновых задач (/run) показываем в GUI-чате, а не в консоли
        let log_tx = app.ai_event_tx.clone();
        app.shell.log_tx = Some(log_tx);

        // Терминал открывается сразу — раньше он жил прямо на столе, а с оконным
        // хромом стал обычным приложением. Позиция детерминированная, не случайная.
        app.windows.terminal.is_open = true;
        app.windows.terminal.default_pos = egui::pos2(380.0, 56.0);

        // ⚙️ Настройки вступают в силу до первого запроса ИИ: set_var здесь
        // достаточен, потому что spawn_ai_request читает env на каждый вызов.
        // Файл победил env — поэтому и статус-строку бэкенда перечитываем.
        app.settings.apply_to_env();
        app.ai_model_name = app.settings.model.clone();

        // 💚 Автозапуск демона Ollama: без него любой запрос ИИ падает с
        // os error 10061, а Ollama на этой машине не входит в автозагрузку.
        // Порт проверяем один раз, на старте — это не кадр egui, сеть тут уместна.
        if app.settings.ollama_autostart && !settings::ollama_listening() {
            match settings::spawn_ollama() {
                Ok(()) => app.ai_text_area.push_str(">> OLLAMA: автозапуск демона…\n"),
                Err(e) => app
                    .ai_text_area
                    .push_str(&format!(">> OLLAMA: автозапуск не удался: {}\n", e)),
            }
        }

        // 💚 Пробник бэкенда ИИ: TCP-коннект к host:port раз в 2 с. Статус-строка стола
        // показывает ONLINE/OFFLINE, поэтому нужен живой признак — но проверять его прямо
        // в кадре нельзя (DNS + connect оборвали бы egui-фрейм), потому поток свой, а
        // результат кладёт в AtomicBool. Первый коннект отрабатывает почти сразу.
        let (host, port) = endpoint_host_port(&configured_url());
        let alive = Arc::clone(&app.backend_alive);
        std::thread::spawn(move || loop {
            let mut ok = false;
            if let Ok(addrs) = (host.as_str(), port).to_socket_addrs() {
                for addr in addrs {
                    if TcpStream::connect_timeout(&addr, Duration::from_millis(700)).is_ok() {
                        ok = true;
                        break;
                    }
                }
            }
            alive.store(ok, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(2));
        });
        app
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
            // Esc — пропуск заставки целиком. Её приходится смотреть на каждом
            // запуске, а без подсказки экран "PRESS ANY KEY" выглядит как зависание.
            if self.boot_stage != BootStage::Desktop && i.key_pressed(egui::Key::Escape) {
                self.boot_stage = BootStage::Desktop;
            }
            if !i.keys_down.is_empty()
                && (self.boot_stage == BootStage::BarLoading || self.boot_stage == BootStage::KernelLogs) { self.user_glitch_trigger = true; }
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

        // Подсказка о пропуске — на стадиях загрузки; у Ready строка своя.
        if self.boot_stage == BootStage::BarLoading || self.boot_stage == BootStage::KernelLogs {
            painter.text(egui::pos2(cx, cy + 365.0), egui::Align2::CENTER_CENTER, "[ ESC — SKIP ]", egui::FontId::monospace(10.0), egui::Color32::from_rgb(85, 85, 85));
        }

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
                painter.text(egui::pos2(cx, cy + 350.0), egui::Align2::CENTER_CENTER, "[ PRESS ANY KEY TO ENTER ENVIRONMENT · ESC — SKIP ]", egui::FontId::monospace(12.0), prompt_color);
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

    /// Исполняет намерение, возвращённое столом (desktop.rs). Стол сам не трогает
    /// ни шелл, ни чат, ни окна — он лишь перечисляет, что сделать, а здесь это
    /// выполняется. Так раскладка не тянет за собой `&mut self` целиком и не
    /// дралась бы с borrow-checker'ом о заимствование нескольких полей сразу.
    fn apply_desktop_intent(&mut self, intent: DesktopIntent, _ctx: &egui::Context) {
        match intent {
            DesktopIntent::FocusInput => {
                // Терминал — обычное окно: сначала раскрываем его, а фокус
                // попросим в draw_terminal_window, когда поле будет создано.
                self.windows.open(WindowId::Terminal);
                self.focus_input_pending = true;
            }
            DesktopIntent::RunCommand(cmd) => self.run_shell_command(&cmd),
            DesktopIntent::ClearChat => self.clear_chat(),
            DesktopIntent::OpenWindow(id) => self.windows.open(id),
            DesktopIntent::ToggleWindowMinimized(id) => {
                // Кнопка панели задач — это и «свернуть/развернуть» открытое
                // окно, и «запустить» закрытое: раньше здесь переключался
                // только minimized, поэтому по закрытым приложениям (а это
                // почти все кнопки) клик не делал ничего вовсе.
                let just_opened = {
                    let w = self.windows.get_mut(id);
                    if w.is_open {
                        w.minimized = !w.minimized;
                        false
                    } else {
                        w.is_open = true;
                        w.minimized = false;
                        true
                    }
                };
                // Запущенный из панели задач терминал получает фокус в строке
                // ввода — так же, как его иконка на столе и пункт «Пуска».
                if just_opened && id == WindowId::Terminal {
                    self.focus_input_pending = true;
                }
            }
            DesktopIntent::MinimizeAll => {
                for wid in WindowId::ALL {
                    self.windows.get_mut(wid).minimized = true;
                }
            }
        }
    }

    /// Прогоняет слеш-команду через шелл и кладёт её вывод в чат. Это то же, что делает
    /// send_ai_query, но без строки приглашения и без стриминга — контекстное меню и
    /// проводник шлют сюда только команды, возвращающие DisplayOutput.
    fn run_shell_command(&mut self, cmd: &str) {
        // `/run` уходит в ядро (IPC): скрипт крутится в отдельном процессе,
        // лог приходит потоком в чат. Остальные команды — локальный шелл.
        if self.try_core_run(cmd) {
            return;
        }
        let action = self.shell.handle_command(cmd);
        self.apply_shell_command(action);
    }

    /// `/run <id>` исполняет ядро — отдельный процесс `vanguard_core.exe`, куда
    /// оболочка ушла по IPC. Оболочка только печатает строку инициации (как это
    /// делал шелл) и держит `bg_tasks`, пока лог не дотечёт токеном в чат.
    /// Возвращает false для всего, что не `/run <id>` — такое ведёт себя локально.
    fn try_core_run(&mut self, cmd: &str) -> bool {
        let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
        if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("/run") {
            return false;
        }
        let id = parts[1..].join(" ");
        // В точности строка, которую раньше вернул DisplayOutput (он добавлял
        // второй \n через apply_shell_command) — раскладка чата не меняется.
        self.ai_text_area
            .push_str(&format!(">> INITIATING BACKGROUND TASK FOR APP ID: {}...\n\n", id));

        let tx = self.ai_event_tx.clone();
        let bg = Arc::clone(&self.shell.bg_tasks);
        bg.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let request = serde_json::to_string(&Request::RunScript { id: id.clone() })
                .unwrap_or_default();
            let reply = crate::ipc_client::call(&request);
            if reply.ok {
                if !reply.text.is_empty() {
                    let _ = tx.send(AiEvent::Token(reply.text));
                }
            } else {
                // Ядро не поднялось (не собран vanguard_core.exe?) — запускаем
                // локально, чтобы /run не сдох совсем; причина тоже в чате.
                let _ = tx.send(AiEvent::Token(format!(
                    "{}\n>> Ядро недоступно — скрипт выполнен локально.\n",
                    reply.text
                )));
                let log = vanguard_os_lib::core::executor::VanguardExecutor::execute_script(&id);
                let _ = tx.send(AiEvent::Token(format!("\n{}\n", log)));
            }
            bg.fetch_sub(1, Ordering::SeqCst);
        });
        true
    }

    /// Общая судьба не-AI действий шелла: вывод — в чат, выход — завершение процесса.
    /// `AiStreamAllowed` здесь не обрабатывается: стрим запускает только send_ai_query.
    fn apply_shell_command(&mut self, action: ShellAction) {
        match action {
            ShellAction::SystemExit => {
                self.ai_text_area.push_str(">> SHUTTING DOWN INFRASTRUCTURE... BYE.\n");
                std::process::exit(0);
            }
            ShellAction::DisplayOutput(text) => self.ai_text_area.push_str(&format!("{}\n", text)),
            ShellAction::AiStreamAllowed => {}
        }
    }

    // ─── Содержимое окон ─────────────────────────────────────────────
    // Раньше всё это жило анонимными замыканиями прямо в update(); вынесено
    // в методы, чтобы кадр читался как список шагов, а не как стена кода.

    /// 🧠 Окно ИИ-терминала: чат с моделью и строка ввода команд. Раньше это
    /// жило прямо на столе (draw_chat_and_input в desktop.rs) — с переходом на
    /// оконный хром виндовс-стиля стало обычным окном.
    fn draw_terminal_window(&mut self, ctx: &egui::Context) {
        let mut submit = false;
        self.windows.terminal.show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .id_source("ai_chat_scroll")
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.ai_text_area)
                            .font(egui::FontId::monospace(11.0))
                            .text_color(egui::Color32::WHITE)
                            .desired_width(f32::INFINITY)
                            .lock_focus(true),
                    );
                });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let input_field = ui.add(
                    egui::TextEdit::singleline(&mut self.user_input)
                        .id(egui::Id::new(INPUT_FIELD_ID))
                        .desired_width(ui.available_width() - 70.0)
                        .hint_text("/help, /apps или запрос к ИИ..."),
                );
                // Просьба от иконки/Enter на столе: поле уже нарисовано — фокусуем.
                if self.focus_input_pending {
                    input_field.request_focus();
                    self.focus_input_pending = false;
                }

                // История команд: ↑/↓ перебирают прошлые запросы. TextEdit в
                // singleline-режиме эти стрелки не использует, поэтому забираем их себе.
                if input_field.has_focus() {
                    let dir = ui.input(|i| {
                        if i.key_pressed(egui::Key::ArrowUp) { Some(-1i32) }
                        else if i.key_pressed(egui::Key::ArrowDown) { Some(1i32) }
                        else { None }
                    });
                    if let Some(dir) = dir {
                        walk_history(
                            &self.cmd_history,
                            &mut self.cmd_hist_pos,
                            &mut self.user_input,
                            dir,
                            ui.ctx(),
                        );
                    }
                }

                if (input_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || ui.button("EXEC").clicked()
                {
                    submit = true;
                }
            });
        });
        if submit {
            self.push_history(&self.user_input.clone());
            // send_ai_query сам чистит поле, эхо-строку пишет в чат и решает,
            // слеш-команда это или запрос к модели.
            self.send_ai_query();
        }
    }

    /// 📊 Окно «Монитор системы»: график CPU, память, диск и статус бэкенда.
    /// Виджеты жили прямо на столе — с оконным хромом они стали приложением
    /// «Монитор», как диспетчер задач в Windows.
    fn draw_monitor_window(&mut self, ctx: &egui::Context) {
        self.windows.monitor.show(ctx, |ui| {
            // Сэмпл телеметрии — раз за кадр окна (как раньше раз за кадр стола).
            let mut ram_text = String::new();
            let mut disk_text = String::new();
            let mut ram_percent = 0.0f32;
            if let Ok(metrics) = self.metrics_link.lock() {
                self.cpu_history.pop_front();
                self.cpu_history.push_back(metrics.current_cpu_load);
                ram_percent = metrics.current_ram_percent;
                ram_text = metrics.ram_info_text.clone();
                disk_text = metrics.disk_info_text.clone();
            }
            let w = ui.available_width();

            // — График CPU: код перенесён из бывшего draw_chrome стола,
            //   координаты теперь относительные (окно может менять размер) —
            ui.label(egui::RichText::new("CPU LOAD").monospace().size(11.0));
            // allocate_space возвращает (Id, Response) — прямоугольник берём у Response.
            let rect = ui.allocate_space(egui::vec2(w, 110.0)).1;
            let painter = ui.painter();
            painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(24, 24, 28));
            painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(50, 50, 56)));
            if self.cpu_history.len() > 1 {
                let plot_bottom = rect.max.y - 16.0;
                let scale = (rect.height() - 30.0).max(20.0) / 100.0;
                let steps = self.cpu_history.len().saturating_sub(1).max(1) as f32;
                let step_x = (w - 24.0) / steps;
                let line = egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 60, 60));
                for i in 0..self.cpu_history.len() - 1 {
                    let x1 = rect.min.x + 12.0 + (i as f32 * step_x);
                    let y1 = plot_bottom - (self.cpu_history[i] * scale);
                    let x2 = rect.min.x + 12.0 + ((i + 1) as f32 * step_x);
                    let y2 = plot_bottom - (self.cpu_history[i + 1] * scale);
                    painter.line_segment([egui::pos2(x1, y1), egui::pos2(x2, y2)], line);
                }
            }
            painter.text(
                egui::pos2(rect.min.x + 12.0, rect.max.y - 8.0),
                egui::Align2::LEFT_CENTER,
                format!("CURRENT_LOAD: {:.1}%", self.cpu_history.back().copied().unwrap_or(0.0)),
                egui::FontId::monospace(9.0),
                egui::Color32::from_rgb(130, 130, 135),
            );
            ui.add_space(8.0);

            // — Память: строка + полоса —
            ui.label(egui::RichText::new(ram_text).monospace().size(11.0));
            ui.add(
                egui::ProgressBar::new(ram_percent / 100.0)
                    .desired_width(w)
                    .fill(egui::Color32::from_rgb(255, 60, 60)),
            );
            ui.add_space(8.0);

            // — Диск —
            ui.label(egui::RichText::new(disk_text).monospace().size(11.0));
            ui.add_space(10.0);

            // — Статус бэкенда ИИ —
            let online = self.backend_alive.load(Ordering::Relaxed);
            let (txt, col) = if online {
                (
                    format!("AI CORE: ONLINE — {} @ {}", self.ai_model_name, self.ai_endpoint),
                    egui::Color32::from_rgb(80, 220, 120),
                )
            } else {
                (
                    format!("AI CORE: OFFLINE — {}", self.ai_endpoint),
                    egui::Color32::from_rgb(255, 80, 80),
                )
            };
            ui.colored_label(col, egui::RichText::new(txt).monospace().size(10.0));
        });
    }

    /// Запомнить команду перед отправкой. Пустые строки не храним, повтор
    /// последней команды не дублируем.
    fn push_history(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if cmd.is_empty() { return; }
        if self.cmd_history.last().map(String::as_str) != Some(cmd) {
            self.cmd_history.push(cmd.to_string());
            if self.cmd_history.len() > 50 { self.cmd_history.remove(0); }
        }
        self.cmd_hist_pos = None;
    }

    /// 📝 Окно Блокнота (Notes Workspace)
    fn draw_notes_window(&mut self, ctx: &egui::Context) {
        self.windows.notes.show(ctx, |ui| {
            ui.heading("📝 VANGUARD USER NOTES");
            ui.add_space(5.0);
            egui::ScrollArea::vertical().id_source("notes_scroll").show(ui, |ui| {
                let edit = ui.add(egui::TextEdit::multiline(&mut self.notes_text).font(egui::FontId::monospace(11.0)).text_color(egui::Color32::from_rgb(0, 255, 66)).desired_width(f32::INFINITY));
                // Диск тронем в update(), один раз за кадр, а не на каждый символ.
                if edit.changed() { self.notes_dirty = true; }
            });
        });
    }

    /// 🟢 Окно Матрицы (Digital Rain Stream). Капли падают сами, без ввода
    /// пользователя, поэтому окно просит repaint собственноручно.
    fn draw_matrix_window(&mut self, ctx: &egui::Context) {
        self.windows.matrix.show(ctx, |ui| {
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
    }

    /// 🧹 Окно Очистителя: три стадии по таймеру; по завершении действительно
    /// выполняет /wipe (удаляет все сгенерированные скрипты).
    fn draw_cleaner_window(&mut self, ctx: &egui::Context) {
        self.windows.cleaner.show(ctx, |ui| {
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
    }

    /// 🧬 Окно Neural Trainer: HF-модель, статистика журнала диалогов, кнопка старта.
    /// Подробный лог обучения идёт в GUI-чат (как у /run) — в окне только статус,
    /// чтобы не дублировать поток и не спорить с маркером стриминга.
    fn draw_trainer_window(&mut self, ctx: &egui::Context) {
        // Флаги вместо прямых вызовов: замыкание показа занято borrow'ом
        // self.windows.trainer, а start_trainer берёт &mut self целиком.
        let mut want_start = false;
        let mut want_refresh = false;
        self.windows.trainer.show(ctx, |ui| {
            ui.heading(">> SELF-FINETUNE PROTOCOL (QLoRA)");
            ui.add_space(6.0);

            ui.label("HuggingFace-модель для дообучения:");
            ui.add(egui::TextEdit::singleline(&mut self.trainer_hf_model).desired_width(320.0));
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label(format!("Реплик в журнале диалогов: {}", self.trainer_count));
                if ui.button("⟳ пересчитать").clicked() {
                    want_refresh = true;
                }
            });
            ui.add_space(6.0);

            let running = self.trainer_running.load(Ordering::SeqCst);
            let btn = ui.add_enabled(!running, egui::Button::new("▶ ЗАПУСТИТЬ ДООБУЧЕНИЕ"));
            if btn.clicked() {
                want_start = true;
            }
            if running {
                ui.add(egui::Spinner::new());
            }

            ui.add_space(6.0);
            ui.colored_label(
                egui::Color32::from_rgb(120, 220, 120),
                egui::RichText::new(self.trainer_status.as_str()).monospace().small(),
            );
            ui.add_space(6.0);
            ui.small("Дообучение идёт на Ваших собственных диалогах из терминала.\nБаза не изменяется: адаптеры копятся в trainer/runs/.");
        });
        if want_refresh {
            self.trainer_count = vanguard_os_lib::core::shell::count_dialogue_lines();
        }
        if want_start {
            self.start_trainer();
        }
    }

    /// 🧬 Запуск самообучения: python trainer/vanguard_train.py в фоновом потоке.
    /// Построчный лог уходит в GUI-чат через log_tx (та же механика, что у /run),
    /// счётчик bg_tasks держит панель задач и принудительную перерисовку живыми,
    /// а trainer_running не пускает второй параллельный запуск.
    fn start_trainer(&mut self) {
        if self.trainer_running.load(Ordering::SeqCst) {
            self.trainer_status = String::from("Тренировка уже идёт — дождитесь конца.");
            return;
        }
        if self.is_ai_streaming {
            // Обучение выгрузит модель из Ollama ради VRAM — текущий ответ ИИ
            // должно пройти до начала, иначе он упадёт посередине.
            self.trainer_status = String::from("Дождитесь готовности текущего ответа ИИ.");
            return;
        }
        self.trainer_running.store(true, Ordering::SeqCst);
        self.shell.bg_tasks.fetch_add(1, Ordering::SeqCst);
        self.trainer_status = format!("Запуск {}… лог — в чате.", self.trainer_hf_model);

        let hf = self.trainer_hf_model.clone();
        let running = Arc::clone(&self.trainer_running);
        let bg = Arc::clone(&self.shell.bg_tasks);
        let tx = self.ai_event_tx.clone();
        std::thread::spawn(move || {
            let send = |text: String| {
                let _ = tx.send(AiEvent::Token(text));
            };
            send(String::from(">> TRAINER: запуск python trainer/vanguard_train.py\n"));
            let child = std::process::Command::new("python")
                .args(["trainer/vanguard_train.py", "--hf-model", &hf])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn();
            match child {
                Ok(mut child) => {
                    // stdout построчно прямо во время обучения; stderr добираем
                    // по завершении — он короткий (ошибки загрузки модели и т.п.).
                    if let Some(out) = child.stdout.take() {
                        use std::io::{BufRead, BufReader};
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            send(format!("T: {}\n", line));
                        }
                    }
                    let err = child
                        .wait_with_output()
                        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                        .unwrap_or_default();
                    if !err.is_empty() {
                        send(format!("T: [stderr] {}\n", err));
                    }
                }
                Err(e) => send(format!("T: не удалось запустить python: {}\n", e)),
            }
            running.store(false, Ordering::SeqCst);
            bg.fetch_sub(1, Ordering::SeqCst);
        });
    }

    /// ⚙️ Окно «Настройки»: бэкенд ИИ (модель, reasoning_effort, keep_alive) и
    /// автозапуск демона Ollama.
    ///
    /// Любая правка сразу кладётся в окружение и в `vanguard_settings.json` —
    /// отдельной кнопки «Сохранить» нет, как и у блокнота. Всё, что трогается
    /// внутри замыкания, — только поля `self`: `show()` держит `self.windows.settings`,
    /// поэтому вызывать отсюда методы `&mut self` нельзя (паттерн окна Тренера).
    fn draw_settings_window(&mut self, ctx: &egui::Context) {
        let mut changed = false;
        let mut want_spawn = false;

        self.windows.settings.show(ctx, |ui| {
            ui.heading(">> BACKEND & DAEMON SETTINGS");
            ui.add_space(6.0);

            // Статус-строка: зелёная точка + тот же endpoint, что у пробника бэкенда.
            let online = self.backend_alive.load(Ordering::Relaxed);
            ui.horizontal(|ui| {
                let dot = if online {
                    egui::Color32::from_rgb(80, 220, 120)
                } else {
                    egui::Color32::from_rgb(255, 80, 80)
                };
                ui.colored_label(dot, if online { "● ONLINE" } else { "● OFFLINE" });
                ui.label(egui::RichText::new(self.ai_endpoint.as_str()).monospace().small());
            });
            ui.add_space(8.0);

            ui.label("Модель Ollama (тег):");
            if ui
                .add(egui::TextEdit::singleline(&mut self.settings.model).desired_width(300.0))
                .changed()
            {
                changed = true;
            }
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label("reasoning_effort:");
                egui::ComboBox::from_id_source("settings_effort")
                    .selected_text(self.settings.reasoning_effort.clone())
                    .show_ui(ui, |ui| {
                        for v in ["none", "low", "medium", "high"] {
                            if ui
                                .selectable_value(
                                    &mut self.settings.reasoning_effort,
                                    v.to_string(),
                                    v,
                                )
                                .changed()
                            {
                                changed = true;
                            }
                        }
                    });
            });
            ui.small(
                "none — мышление выключено (~1 с). low/medium/high на гибридных моделях\n\
                 добавляют 10–40 с, а на моделях без thinking Ollama ответит HTTP 400.",
            );
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label("keep_alive:");
                if ui
                    .add(egui::TextEdit::singleline(&mut self.settings.keep_alive).desired_width(120.0))
                    .changed()
                {
                    changed = true;
                }
                ui.small("-1 = модель не покидает VRAM никогда");
            });
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);

            // ── Демон ──
            if online {
                ui.colored_label(egui::Color32::from_rgb(80, 220, 120), "Ollama уже запущен.");
            } else if ui.button("▶ ЗАПУСТИТЬ OLLAMA СЕЙЧАС").clicked() {
                want_spawn = true;
            }
            if ui
                .checkbox(
                    &mut self.settings.ollama_autostart,
                    "Поднимать Ollama при старте Vanguard OS",
                )
                .changed()
            {
                changed = true;
            }
            ui.add_space(6.0);

            if !self.settings_status.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 200, 80),
                    egui::RichText::new(self.settings_status.as_str()).monospace().small(),
                );
            }
            ui.add_space(4.0);
            ui.small(
                "URL и ключ — переменные VANGUARD_AI_URL / VANGUARD_AI_KEY.\n\
                 Править их из окна нельзя: пробник бэкенда резолвит host:port один раз.",
            );
        });

        // Всё, что вышло за рамки замыкания, — здесь: на &mut self целиком.
        if changed {
            self.settings.apply_to_env();
            // Статус-строка стола показывает модель — держим её в актуальном виде.
            self.ai_model_name = self.settings.model.clone();
            self.settings_status = match self.settings.save() {
                Ok(()) => format!("Сохранено в {}", settings::SETTINGS_FILE),
                Err(e) => format!("Не удалось сохранить: {}", e),
            };
        }
        if want_spawn {
            self.settings_status = match settings::spawn_ollama() {
                Ok(()) => String::from("Ollama запускается… статус обновится через ~2 с."),
                Err(e) => format!("Не удалось запустить Ollama: {}", e),
            };
        }
    }

    /// 🚀 Окно Лаунчера / Реестра ИИ-софта: список с кнопкой немедленного запуска.
    /// Запуск возвращается наружу (как в проводнике): замыкание show() занято
    /// borrow'ом self.windows.apps, а /run теперь исполняется через try_core_run,
    /// которому нужно &mut self целиком.
    fn draw_apps_window(&mut self, ctx: &egui::Context) -> Option<String> {
        let mut launch_id: Option<String> = None;
        self.windows.apps.show(ctx, |ui| {
            ui.vertical_centered(|ui| { ui.heading("CHOOSE APPLICATION TO RUN:"); ui.add_space(5.0); });
            let mut files = Vec::new();
            if let Ok(entries) = glob("vanguard_gen_*.py") { for e in entries.flatten() { files.push(e); } }

            if files.is_empty() {
                ui.vertical_centered(|ui| { ui.colored_label(egui::Color32::from_rgb(85, 85, 85), "No compiled scripts found."); });
                return;
            }

            egui::ScrollArea::vertical().id_source("registry_scroll").show(ui, |ui| {
                for path in files {
                    let id = script_id_from_path(&path.to_string_lossy()).unwrap_or_else(|| "???".to_string());
                    let btn = ui.add(egui::Button::new(format!("▶ LAUNCH: vanguard_gen_{}.py", id)).fill(egui::Color32::from_rgb(16, 16, 16)).stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0, 255, 66))));
                    if btn.clicked() { launch_id = Some(id); }
                    ui.add_space(4.0);
                }
            });
        });
        launch_id
    }

    /// 🗂 Окно проводника скриптов: список vanguard_gen_*.py слева, их код справа.
    /// Перечень читается с диска каждый кадр — файлов единицы, а кэш пришлось бы
    /// инвалидировать и после удаления из самого окна, и после /wipe.
    /// Запуск не делается прямо здесь: run_shell_command берёт &mut self целиком,
    /// а окно уже занято borrow'ом, поэтому ID возвращается наружу и исполняется
    /// вызывающим кодом после show().
    fn draw_scripts_window(&mut self, ctx: &egui::Context) -> Option<String> {
        let mut explorer_run: Option<String> = None;
        self.windows.scripts.show(ctx, |ui| {
            let mut files: Vec<String> = Vec::new();
            if let Ok(entries) = glob("vanguard_gen_*.py") {
                for e in entries.flatten() { files.push(e.to_string_lossy().into_owned()); }
            }

            // Выбранного файла могло не стать — удалили из этого окна или через /wipe.
            if let Some(sel) = &self.explorer_sel {
                if !files.iter().any(|f| f == sel) {
                    self.explorer_sel = None;
                    self.explorer_code.clear();
                    self.explorer_status = String::from("Файл исчез — выбор сброшен.");
                }
            }

            ui.horizontal(|ui| {
                let has_sel = self.explorer_sel.is_some();
                if ui.add_enabled(has_sel, egui::Button::new("▶ RUN")).clicked() {
                    if let Some(path) = self.explorer_sel.clone() {
                        match script_id_from_path(&path) {
                            Some(id) => {
                                explorer_run = Some(id);
                                self.explorer_status = format!("Запущен в фоне: {}", path);
                            }
                            None => self.explorer_status = String::from("Не удалось разобрать ID из имени файла."),
                        }
                    }
                }
                if ui.add_enabled(has_sel, egui::Button::new("✕ DELETE")).clicked() {
                    if let Some(path) = self.explorer_sel.clone() {
                        match std::fs::remove_file(&path) {
                            Ok(_) => {
                                self.explorer_sel = None;
                                self.explorer_code.clear();
                                self.explorer_status = format!("Удалён: {}", path);
                            }
                            Err(e) => self.explorer_status = format!("Ошибка удаления {}: {}", path, e),
                        }
                    }
                }
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(85, 85, 85), format!("{} scripts", files.len()));
            });
            ui.colored_label(egui::Color32::from_rgb(0, 255, 66), self.explorer_status.as_str());
            ui.separator();

            if files.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(85, 85, 85),
                    "REGISTRY EMPTY — попросите ИИ написать скрипт, потом /apps.");
                return;
            }

            ui.horizontal(|ui| {
                // Слева — перечень файлов
                egui::ScrollArea::vertical().id_source("explorer_list").max_width(190.0).show(ui, |ui| {
                    // top_down вместо унаследованного left_to_right: имена должны
                    // идти столбиком, а не одной строкой (подробнее — в draw_fs_window).
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        for f in &files {
                            let short = std::path::Path::new(f)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| f.clone());
                            let selected = self.explorer_sel.as_deref() == Some(f.as_str());
                            if ui.selectable_label(selected, short).clicked() {
                                self.explorer_sel = Some(f.clone());
                                self.explorer_code = std::fs::read_to_string(f)
                                    .unwrap_or_else(|e| format!("// ошибка чтения {}: {}", f, e));
                                self.explorer_status = format!("Открыт: {}", f);
                            }
                        }
                    });
                });
                ui.separator();
                // Справа — содержимое выбранного файла
                egui::ScrollArea::both().id_source("explorer_code").show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut self.explorer_code)
                        .code_editor()
                        .desired_width(f32::INFINITY));
                });
            });
        });
        explorer_run
    }

    /// Очистка чата: сбрасывает буфер вывода и память диалога (как /reset) и снимает
    /// маркер стрима, иначе следующий Delta перезаписал бы уже пустой буфер по старому смещению.
    fn clear_chat(&mut self) {
        self.shell.ai_core.clear_memory();
        self.ai_text_area = String::from("VANGUARD CORE: Awaiting input command...\n");
        self.ai_stream_marker = None;
    }

    pub fn send_ai_query(&mut self) {
        let query = self.user_input.trim().to_string();
        if query.is_empty() { return; }

        // 1. Дублируем ввод в интерфейс окна
        self.ai_text_area.push_str(&format!("\nvanguard_root@kernel_sh:~# {}\n", query));

        self.user_input.clear();

        // `/run` уходит в ядро (фон + лог токеном), остальное — локальный шелл;
        // не-AI ветки (вывод текста, выход) исполняет apply_shell_command —
        // та же логика, что для контекстного меню и проводника.
        if self.try_core_run(&query) {
            return;
        }
        let action = self.shell.handle_command(&query);
        match action {
            ShellAction::AiStreamAllowed => {
                // Если пользователь ввел обычный текст, а не слеш-команду — будим Ви!
                if self.is_ai_streaming {
                    self.ai_text_area.push_str(">> [CORE] Прерывание: Предыдущий нейро-поток еще активен.\n");
                    return;
                }

                self.is_ai_streaming = true;
                // Префикс "V: " добавляет сам execute_ai_action, поэтому здесь не пушим —
                // иначе в чате выйдет "V: V: ..." и лишняя строка перед баннером CORE LNK.

                // Характер Ви захардкожен: поля personality в приложении нет,
                // телеметрия в него не пишет, поэтому шлём фиксированный набор.
                let current_personality = vanguard_os_lib::ai_core::DronePersonality {
                    loyalty: 1.0,       // Максимальная верность системе
                    irritability: 0.1,  // Минимальная раздражительность на старте
                    energy: 1.0,        // Полный заряд батареи/энергии ядра
                };

                // URL/модель/ключ spawn_ai_request читает сам из env (VANGUARD_AI_*).
                // Диалог: запоминаем вопрос ДО отправки, чтобы модель увидела его вместе
                // с предыдущими репликами. Ответ добавляется в check_ai_stream по Finished.
                self.shell.ai_core.push_user(&query);
                // События уходят в канал, чат читает их в check_ai_stream.
                vanguard_os_lib::ai_core::spawn_ai_request(
                    query,
                    self.shell.ai_core.messages.clone(),
                    current_personality,
                    self.ai_event_tx.clone(),
                );
            }
            // Остальное (вывод команды или выход) — та же логика, что у run_shell_command.
            other => self.apply_shell_command(other),
        }
    }

    /// Отправляет компиляцию Rust в ядро и возвращает управление сразу: `rustc -O`
    /// занимает секунды и единственный из действий ИИ замораживает рендер. Поток
    /// отдельный, `bg_tasks` держит кадр живым до `AiEvent::ActionOutput`.
    ///
    /// Истории диалога сразу отдаём заглушку — сама компиляция в неё не попадает,
    /// иначе медленный `rustc` встал бы в историю уже после новой реплики, а писать
    /// финальный ответ в чат в разгар нового стрима нельзя: маркер бы его перетёр.
    fn dispatch_rust_compile(&mut self, description: &str, code: String) {
        let gen_id: u32 = rand::thread_rng().gen_range(100..1000);
        let ack = format!(
            ">> [ИИ] {}\n>> Компиляция Rust ушла в ядро (vanguard_core), ждём...\n",
            description
        );
        self.shell.ai_core.push_assistant(&ack);
        self.ai_text_area.push_str(&ack);

        let tx = self.ai_event_tx.clone();
        let bg = Arc::clone(&self.shell.bg_tasks);
        bg.fetch_add(1, Ordering::SeqCst);
        // Всё клонируется до spawn: замыкание уходит в 'static-поток, а
        // description — ссылка на локальный action вызывающего.
        let description = description.to_string();
        std::thread::spawn(move || {
            let request = serde_json::to_string(&Request::CompileRust {
                code,
                description,
                gen_id,
            })
            .unwrap_or_default();
            let reply = crate::ipc_client::call(&request);
            let _ = tx.send(AiEvent::ActionOutput(reply.text));
            bg.fetch_sub(1, Ordering::SeqCst);
        });
    }

    pub fn check_ai_stream(&mut self) {
        // Дренажим канал всегда, а не только пока идёт ответ ИИ: иначе лог
        // фоновой задачи (/run) зависнет в очереди до следующего запроса.
        while let Ok(event) = self.ai_channel_rx.try_recv() {
            match event {
                // Служебный токен (лог подключения или вывод /run) — обычное дописывание.
                AiEvent::Token(text) => {
                    self.ai_text_area.push_str(&text);
                }

                // Нарастающий превью ответа. Каждый раз заново замещаем участок,
                // начиная с маркера: событие несёт ПОЛНОЕ текущее состояние, поэтому
                // порядок и потери чанков не накапливают ошибку.
                AiEvent::Delta(preview) => {
                    let marker = *self.ai_stream_marker.get_or_insert(self.ai_text_area.len());
                    self.ai_text_area.truncate(marker);
                    // Префикс "V: " дублируем тот же, что вернёт execute_ai_action,
                    // — иначе по готовности ответа вся строка сдвинется вправо на 3 знака.
                    // Пустой превью сюда не приходит: ai_core шлёт Delta только при изменении.
                    self.ai_text_area.push_str("V: ");
                    self.ai_text_area.push_str(&preview);
                }

                // ИИ запросил действие (создать скрипт, ответить текстом) — выполняем через шелл.
                // через шелл и показываем результат в чате.
                AiEvent::ExecuteAction(action) => {
                    // Превью стриминга уже показано — убираем его, чтобы не дублировать
                    // с финальным ответом (иначе в чате будет и обрезок JSON, и текст).
                    if let Some(marker) = self.ai_stream_marker.take() {
                        self.ai_text_area.truncate(marker);
                    }
                    // Компиляция Rust уходит в ядро: rustc -O занимает секунды.
                    if let VanguardAction::CreateScript { language, code, description } = &action {
                        if language.eq_ignore_ascii_case("rust") {
                            self.dispatch_rust_compile(description, code.clone());
                            continue;
                        }
                    }
                    // Остальное (текст, python/bash, системные команды) — локально:
                    // это не тяжело, выносить в ядро незачем. Результат содержит сам
                    // ответ ИИ (или лог создания скрипта).
                    let output = self.shell.execute_ai_action(action);
                    // Ответ уходит и в историю диалога: без него контекст состоял бы
                    // из одных вопросов, и модель не помнила бы, что сама ответила.
                    self.shell.ai_core.push_assistant(&output);
                    self.ai_text_area.push_str(&output);
                }

                // Результат тяжёлой операции из ядра (компиляция Rust). В историю он
                // не идёт — там уже лежит заглушка из ExecuteAction; в чат дописываем,
                // а если пользователь уже начал новый ответ ИИ — вставляем ПЕРЕД его
                // превью (по маркеру), иначе следующий Delta перетёр бы результат.
                AiEvent::ActionOutput(text) => match self.ai_stream_marker {
                    Some(marker) => {
                        self.ai_text_area.insert_str(marker, &text);
                        self.ai_stream_marker = Some(marker + text.len());
                    }
                    None => self.ai_text_area.push_str(&text),
                },

                // Генерация завершена: маркер снимается, терминал разблокируется,
                // а если в чате остался python-код — он сохраняется на диск.
                AiEvent::Finished => {
                    self.ai_stream_marker = None;
                    self.is_ai_streaming = false;
                    if let Some(filename) = self.shell.ai_core.save_code_if_exists(&self.ai_text_area) {
                        self.ai_text_area.push_str(&format!("\n\n[SYSTEM] CODE AUTO-COMPILED: {}", filename));
                    }
                    break;
                }

                AiEvent::Error(err_msg) => {
                    // Оборванный превью не оставляем в чате — вместо него уйдёт причина
                    if let Some(marker) = self.ai_stream_marker.take() {
                        self.ai_text_area.truncate(marker);
                    }
                    // Запрос не состоялся — забираем последний вопрос из истории, иначе
                    // модель в следующий раз увидит реплику, на которую никогда не отвечала.
                    if self.shell.ai_core.messages.last().is_some_and(|(role, _)| role == "user") {
                        self.shell.ai_core.messages.pop();
                    }
                    self.ai_text_area.push_str(&format!("\n\n[CRITICAL AI ERROR]: {}", err_msg));
                    self.is_ai_streaming = false;
                    break;
                }
            }
        }
    }
}

// =========================================================================
// ГЛАВНЫЙ ИНТЕРФЕЙСНЫЙ ЦИКЛ ОБНОВЛЕНИЯ ЭКРАНА СРЕДЫ EFRAME
// =========================================================================
impl eframe::App for VanguardOSApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Геометрия стола и заставки каждый кадр берётся из реального вьюпорта.
        // Раньше тут жили константы 1024×768 — и в развёрнутом или полноэкранном
        // окне панель задач, меню «Пуск» и календарь рисовались бы в углу того
        // же старого размера. Центр логотипа считаем пропорционально опорным
        // 512×284, поэтому на 1024×768 заставка не смещается ни на пиксель.
        let screen = ctx.screen_rect();
        self.win_w = screen.width();
        self.win_h = screen.height();
        self.cx = screen.width() * BASE_CX / BASE_W;
        self.cy = screen.height() * BASE_CY / BASE_H;

        // F11 — полный экран (borderless на текущем мониторе; egui-winit
        // превращает флаг в `Fullscreen::Borderless(None)`). Флаг читаем из
        // вьюпорта, а не храним свой: выйти из фулскрина могут и средства
        // Windows, и тогда собственный флаг врал бы про состояние окна.
        if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            let full = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
        }

        self.handle_boot_input(ctx);
        self.step_boot_logic();
        self.check_ai_stream();
        // Ответы IPC проводника — тот же принцип, что у ИИ-стрима: дренируем
        // каждый кадр без условий, иначе результат зависнет в канале до
        // следующего открытия окна «Файлы», а bg_tasks не упадёт.
        self.fs_drain();

        // egui уходит в сон, когда нет ввода (все request_repaint() живут в
        // окнах, закрытых по умолчанию). Пока ИИ стримится или крутится
        // фоновая задача (/run), держим кадр живым — иначе лог не дорисуется.
        if self.is_ai_streaming || self.shell.bg_tasks.load(Ordering::Relaxed) > 0 {
            ctx.request_repaint();
        } else if self.boot_stage == BootStage::Desktop {
            // Часы и графики телеметрии на столе идут сами: egui без ввода засыпает,
            // и до этой строки время/график CPU стояли, пока мышь не шевельнёшь.
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        // ⌨️ Горячие клавиши рабочего стола. Проверяем стадию: иначе среагируют ещё
        // на заставке, где стола и поля ввода не существует. Сам список живёт в
        // desktop.rs — здесь только исполняем вернувшиеся намерения.
        if self.boot_stage == BootStage::Desktop {
            let hotkey_intents = self.desktop.handle_hotkeys(ctx);
            for intent in hotkey_intents {
                self.apply_desktop_intent(intent, ctx);
            }
        }

        // 📝 Окно Блокнота (Notes Workspace)
        self.draw_notes_window(ctx);

        // 🟢 Окно Матрицы (Digital Rain Stream)
        self.draw_matrix_window(ctx);

        // 🧹 Окно Очистителя логов и кэша VRAM
        self.draw_cleaner_window(ctx);

        // 🚀 Окно Лаунчера / Реестра ИИ-софта: LAUNCH возвращается наружу и
        // исполняется после show() — /run уйдёт в ядро через try_core_run.
        if let Some(id) = self.draw_apps_window(ctx) {
            self.run_shell_command(&format!("/run {}", id));
        }

        // 🗂 Окно проводника скриптов: список vanguard_gen_*.py слева, их код справа.
        // Запуск возвращается наружу: run_shell_command берёт &mut self целиком,
        // а окно ещё занято borrow'ом, поэтому ID исполняется уже после show().
        if let Some(id) = self.draw_scripts_window(ctx) {
            self.run_shell_command(&format!("/run {}", id));
        }

        // 🗂 Окно «Файлы»: настоящий проводник по всем дискам. Список и
        // редактор перечитываются по клику, а не каждый кадр; сами вызовы идут
        // в фоне (см. app/fs_explorer.rs), здесь только рисование окна.
        self.draw_fs_window(ctx);

        // 🧬 Окно самообучения: журнал своих диалогов → QLoRA-дообучение модели.
        self.draw_trainer_window(ctx);

        // ⚙️ Окно настроек: бэкенд ИИ и автозапуск демона Ollama.
        self.draw_settings_window(ctx);

        // 🧠 ИИ-терминал и 📊 Монитор системы — появились с переходом на оконный
        // хром Windows 11: терминал больше не врезан в стол, а монитор заменил
        // виджеты, раньше висевшие прямо на обоях.
        self.draw_terminal_window(ctx);
        self.draw_monitor_window(ctx);

        // 📝 Автосохранение блокнота: пишем только когда текст менялся. На каждый
        // символ — лишняя запись, а полагаться на on_exit нельзя: выход через /exit —
        // это process::exit(0), eframe до конца не доходит.
        if self.notes_dirty {
            self.notes_dirty = false;
            if let Err(e) = std::fs::write(NOTES_FILE, &self.notes_text) {
                // Ошибку показываем вне стрима: маркер в check_ai_stream
                // перезаписал бы этот текст.
                if !self.is_ai_streaming {
                    self.ai_text_area.push_str(&format!("[NOTES] не удалось сохранить заметки: {}\n", e));
                }
            }
        }

        // 🖥️ Рендеринг Центрального макета (Переключение Заставка / Стол)
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.boot_stage != BootStage::Desktop {
                ctx.request_repaint();
                self.draw_boot_screen(ui.painter());
            } else {
                // Стол целиком живёт в desktop.rs: обои, иконки, контекстное меню,
                // центрированная панель задач и меню «Пуск». Он не владеет ни окнами,
                // ни шеллом, ни чатом — берёт к ним доступ через DesktopHost (набор
                // заимствований на один кадр) и возвращает намерения, которые мы
                // исполняем ниже. Так раскладка не тянет за собой &mut self.
                // Меню «Пуск» показывает свежие скрипты: перечисляем их только пока
                // меню открыто, чтобы не гонять glob каждый кадр.
                if self.desktop.start_open {
                    self.recent_scripts = recent_scripts_scan();
                }
                let mut host = DesktopHost {
                    win_w: self.win_w,
                    win_h: self.win_h,
                    windows: &mut self.windows,
                    backend_online: self.backend_alive.load(Ordering::Relaxed),
                    is_ai_streaming: self.is_ai_streaming,
                    bg_tasks: self.shell.bg_tasks.load(Ordering::Relaxed),
                    recent_scripts: &self.recent_scripts,
                };
                let intents = self.desktop.draw(ui, ctx, &mut host);
                // host держит заимствования полей self — отпускаем их до &mut self ниже.
                drop(host);
                for intent in intents {
                    self.apply_desktop_intent(intent, ctx);
                }
            }
        });
    }
}
