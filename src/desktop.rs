//! Рабочий стол Vanguard OS: сетка фона, верхняя строка, виджеты, иконки,
//! контекстное меню, панель задач, область вывода ИИ и строка ввода.
//!
//! Сюда перенесено всё, что раньше рисовалось прямо в `app.rs` — метод
//! `VanguardOSApp::draw_desktop_gui` плюс ветка `CentralPanel`. Стол не владеет
//! ни окнами, ни шеллом, ни чатом: он получает к ним доступ через [`DesktopHost`]
//! (набор заимствований на один кадр) и возвращает [`DesktopIntent`] — список
//! того, что приложение должно выполнить. Благодаря этому раскладка не тянет
//! за собой `&mut self` целиком и не дралась бы с borrow-checker'ом.

use std::collections::VecDeque;
use std::sync::Mutex;

use chrono::Local;
use eframe::egui;

use crate::app::endpoint_host_port;
use crate::telemetry::SystemMetrics;
use crate::ui_window::{VanguardWindows, WindowId};

/// Идентификатор строки ввода в памяти egui. Общая константа: стол запрашивает
/// фокус для неё по хоткею Enter и по иконке AI CORE, а приложение — при старте.
pub const INPUT_FIELD_ID: &str = "ai_input_field";

/// Иконка рабочего стола. Позицию в сетке считает [`Layout`] — здесь только то,
/// что объявляет автор иконки: подпись, подсказка и действие.
pub struct DesktopIcon {
    /// Отображаемая подпись; заодно служит ключом для `make_persistent_id`.
    pub label: &'static str,
    /// Подсказка при наведении (Windows-привычка: всегда говорить, что именно под курсором).
    pub hint: &'static str,
    pub action: IconAction,
}

/// Что делает клик по иконке. Строка ввода — особый случай: иконка AI CORE не
/// открывает окна, а переносит фокус в терминал.
pub enum IconAction {
    FocusInput,
    OpenWindow(WindowId),
}

/// Намерение стола. Стол сам не вызывает шелл и не пишет в чат — он лишь
/// перечисляет, что нужно сделать, а `app.rs` исполняет через
/// `VanguardOSApp::apply_desktop_intent`.
pub enum DesktopIntent {
    /// Enter в строке ввода или кнопка EXEC.
    SubmitInput,
    /// Перенести фокус в строку ввода (Enter на столе, иконка AI CORE, пункт меню).
    FocusInput,
    /// Слеш-команда: `/help`, `/hardware`, `/run <id>` и т. п.
    RunCommand(String),
    /// Очистить чат и память диалога (Ctrl+L, пункт меню).
    ClearChat,
    /// Развернуть окно (иконка, пункт меню).
    OpenWindow(WindowId),
    /// Клик по кнопке панели задач: свернуть/развернуть.
    ToggleWindowMinimized(WindowId),
}

/// Всё, что столу нужно от приложения. Живёт ровно один кадр, собирается в
/// `update()` из полей `VanguardOSApp`.
pub struct DesktopHost<'a> {
    pub win_w: f32,
    pub win_h: f32,
    /// Гвард общей телеметрии; стол только читает метрики.
    pub metrics: &'a Mutex<SystemMetrics>,
    /// История загрузки CPU для графика; стол пишет новые сэмple и читает старые.
    pub cpu_history: &'a mut VecDeque<f32>,
    /// Онлайн ли бэкенд ИИ — обновляет фоновый TCP-пробник из `new()`.
    pub backend_online: bool,
    pub ai_model: &'a str,
    pub ai_endpoint: &'a str,
    /// Копия `VanguardOSApp::is_ai_streaming` и `shell.bg_tasks` для панели задач.
    pub is_ai_streaming: bool,
    pub bg_tasks: usize,
    /// Окна целиком: панель задач и иконки обращаются к ним через `WindowId`.
    pub windows: &'a mut VanguardWindows,
    pub ai_text_area: &'a mut String,
    pub user_input: &'a mut String,
}

/// Состояние стола, принадлежащее только ему.
pub struct Desktop {
    /// Список иконок. Позиции в сетке считаются из [`Layout`] от размера окна,
    /// поэтому здесь — только подписи и действия, без координат под 1024×768.
    pub icons: Vec<DesktopIcon>,
    /// Контекстное меню: `Some(позиция курсора)` — открыто, `None` — закрыто.
    pub menu_pos: Option<egui::Pos2>,
    /// История команд строки ввода (↑/↓ перебирают прошлые запросы). Живёт здесь,
    /// а не в приложении: это состояние строки ввода, а не шелла.
    pub cmd_history: Vec<String>,
    /// Позиция в истории: `None` — обычный ввод.
    pub cmd_hist_pos: Option<usize>,
}

impl Default for Desktop {
    fn default() -> Self {
        Self::new()
    }
}

/// Геометрия стола, вычисленная из фактического размера окна. Раньше координаты
/// левой колонки были захардкожены под 1024×768: при низком окне иконки наезжали
/// на панель задач, при высоком внизу зияла пустота. Правая часть (терминал,
/// строка ввода) и так считалась от `win_w/h` — здесь заданы и она, чтобы вся
/// раскладка жила в одном месте.
///
/// На 1024×768 значения совпадают со старой таблицей: колонка `x 40..340`,
/// график `y 80..280`, RAM `295`, Disk `390`, иконки `465` с шагом 60.
struct Layout {
    /// Левая колонка (виджеты + иконки): левый край и ширина.
    col_x: f32,
    col_w: f32,
    /// График CPU: верх и высота. Высота течёт — это единственное, что сжимается.
    graph_y: f32,
    graph_h: f32,
    /// Виджеты RAM и Disk: высоты константны (80 и 60), меняется только верх.
    ram_y: f32,
    disk_y: f32,
    /// Сетка иконок: верх и шаг строк.
    icons_top: f32,
    icon_pitch: f32,
    /// Терминал ИИ: лево, верх, ширина, высота.
    term_x: f32,
    term_y: f32,
    term_w: f32,
    term_h: f32,
}

impl Layout {
    fn new(w: f32, h: f32) -> Self {
        const COL_X: f32 = 40.0;
        const COL_W: f32 = 300.0;
        const TOP: f32 = 80.0;               // под верхним баром (45 px) с запасом
        const GAP: f32 = 15.0;
        const RAM_H: f32 = 80.0;
        const DISK_H: f32 = 60.0;
        const ICON_ROWS: f32 = 3.0;
        const ICON_H: f32 = 40.0;
        const TASKBAR_H: f32 = 40.0;

        // Низ колонки — над панелью задач с запасом.
        let bottom = h - TASKBAR_H - 20.0;
        // График получает всё, что останется после фиксированных виджетов и
        // номинальной сетки иконок (3×60): сжимается первым и единственный.
        // На 768 даёт 263 → clamp к исходным 200.
        let fixed = RAM_H + DISK_H + GAP * 3.0 + ICON_ROWS * 60.0;
        let graph_h = (bottom - TOP - fixed).clamp(100.0, 200.0);
        let graph_y = TOP;
        let ram_y = graph_y + graph_h + GAP;
        let disk_y = ram_y + RAM_H + GAP;
        let icons_top = disk_y + DISK_H + GAP;
        // Шаг строк иконок подстраивается под остаток высоты: на 768 — исходные
        // 60, на низком окне сжимается, чтобы не уехать под панель задач.
        let icon_pitch = ((bottom - icons_top) / ICON_ROWS).clamp(ICON_H + 4.0, 60.0);

        let term_x = COL_X + COL_W + 30.0;   // 370 на старой раскладке
        let term_y = TOP;
        let term_w = w - term_x - 40.0;
        let term_h = h - 180.0;              // низ — на 100 px выше края окна

        Layout { col_x: COL_X, col_w: COL_W, graph_y, graph_h, ram_y, disk_y,
                 icons_top, icon_pitch, term_x, term_y, term_w, term_h }
    }
}

impl Desktop {
    pub fn new() -> Self {
        Self {
            icons: default_icons(),
            menu_pos: None,
            cmd_history: Vec::new(),
            cmd_hist_pos: None,
        }
    }

    /// Главная точка входа. Рисует неподвижную часть, потом интерактивную и
    /// возвращает намерения, которые приложение должно исполнить.
    pub fn draw(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, host: &mut DesktopHost) -> Vec<DesktopIntent> {
        let lay = Layout::new(host.win_w, host.win_h);
        self.draw_chrome(ui, host, &lay);
        let mut intents = Vec::new();
        self.draw_icons(ui, &lay, &mut intents);
        self.draw_context_menu(ctx, &mut intents);
        self.draw_taskbar(ui, host, &mut intents);
        self.draw_chat_and_input(ui, host, &lay, &mut intents);
        intents
    }

    /// Горячие клавиши стола. Отдельный метод: их проверка жила в `update()`
    /// до отрисовки окон и должна остаться там же, чтобы порядок ввода не сместился.
    pub fn handle_hotkeys(&self, ctx: &egui::Context) -> Vec<DesktopIntent> {
        let mut intents = Vec::new();
        // F1 — справка по командам
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            intents.push(DesktopIntent::RunCommand("/help".into()));
        }
        // Ctrl+L — очистка чата, как в обычном терминале
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::L)) {
            intents.push(DesktopIntent::ClearChat);
        }
        // Enter, когда поле ввода не в фокусе — переносим фокус в него.
        // wants_keyboard_input() означает «кто-то держит клавиатуру», то есть само поле:
        // так мы не перехватываем Enter у набранной команды и не дублируем отправку.
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) && !ctx.wants_keyboard_input() {
            intents.push(DesktopIntent::FocusInput);
        }
        intents
    }

    /// Неподвижная часть: фоновая сетка, верхняя строка, виджеты и рамка терминала.
    /// Единственное место, где мутирует `host.cpu_history`. Координаты берутся
    /// из [`Layout`] — прямых чисел под 1024×768 в этом методе больше нет.
    fn draw_chrome(&self, ui: &egui::Ui, host: &mut DesktopHost, lay: &Layout) {
        let painter = ui.painter();
        let w = host.win_w;
        let h = host.win_h;

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

        // Часы — верхняя строка, статус бэкенда ИИ — под ними (бар в 45px вмещает обе).
        // Онлайн берём из `host`, который уже прочитал AtomicBool пробника: здесь
        // только покраска, поэтому кадр не блокируется ни на коннект, ни на DNS.
        let current_time = Local::now().format("%H:%M:%S | %d.%m.%Y").to_string();
        painter.text(egui::pos2(w - 30.0, 16.0), egui::Align2::RIGHT_CENTER, current_time, egui::FontId::monospace(11.0), egui::Color32::WHITE);

        let (host_name, port) = endpoint_host_port(host.ai_endpoint);
        let (state_txt, state_col) = if host.backend_online {
            ("ONLINE", egui::Color32::from_rgb(0, 255, 66))
        } else {
            ("OFFLINE", egui::Color32::from_rgb(255, 60, 60))
        };
        let backend_line = format!("AI: {} @ {}:{} | {}", host.ai_model, host_name, port, state_txt);
        painter.text(egui::pos2(w - 30.0, 34.0), egui::Align2::RIGHT_CENTER, backend_line, egui::FontId::monospace(9.5), state_col);

        // 3. Атомарное извлечение метрик из фонового демона телеметрии
        let mut ram_text = String::new();
        let mut disk_text = String::new();
        let mut ram_percent = 0.0;

        if let Ok(metrics) = host.metrics.lock() {
            host.cpu_history.pop_front();
            host.cpu_history.push_back(metrics.current_cpu_load);
            ram_percent = metrics.current_ram_percent;
            ram_text = metrics.ram_info_text.clone();
            disk_text = metrics.disk_info_text.clone();
        }

        // 4. Виджет графиков CPU. Рамка и построение относительны: базовая линия
        // за 30 px от низа (там подпись), амплитуда = высота минус заголовок и
        // подпись. На исходных 200 px это ровно прежние wy+170 и множитель 1.2.
        let wx = lay.col_x;
        let wy = lay.graph_y;
        let ww = lay.col_w;
        let graph_rect = egui::Rect::from_min_max(egui::pos2(wx, wy), egui::pos2(wx + ww, wy + lay.graph_h));
        painter.rect_filled(graph_rect, 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(graph_rect, 0.0, egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 0, 0)));
        painter.text(egui::pos2(wx + 15.0, wy + 20.0), egui::Align2::LEFT_CENTER, ">> CPU CORE INSTABILITY GRAPH", egui::FontId::monospace(11.0), egui::Color32::WHITE);

        let plot_bottom = wy + lay.graph_h - 30.0;
        let scale = (lay.graph_h - 80.0).max(20.0) / 100.0;
        let steps = host.cpu_history.len().saturating_sub(1).max(1) as f32;
        let step_x = (ww - 30.0) / steps;
        let line_stroke = egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 0, 0));
        for i in 0..(host.cpu_history.len() - 1) {
            let x1 = wx + 15.0 + (i as f32 * step_x);
            let y1 = plot_bottom - (host.cpu_history[i] * scale);
            let x2 = wx + 15.0 + ((i + 1) as f32 * step_x);
            let y2 = plot_bottom - (host.cpu_history[i + 1] * scale);
            painter.line_segment([egui::pos2(x1, y1), egui::pos2(x2, y2)], line_stroke);
        }
        painter.text(egui::pos2(wx + 15.0, wy + lay.graph_h - 15.0), egui::Align2::LEFT_CENTER, format!("CURRENT_LOAD: {:.1}% | KERNEL: ACTIVE", host.cpu_history.back().unwrap_or(&0.0)), egui::FontId::monospace(9.0), egui::Color32::from_rgb(85, 85, 85));

        // 5. Виджет оперативной памяти (RAM). Стартовый Y приходит из Layout:
        // ниже, под виджетами, живёт сетка иконок, и старое расположение
        // перекрывало RAM и Disk.
        let rx = lay.col_x;
        let ry = lay.ram_y;
        let ram_rect = egui::Rect::from_min_max(egui::pos2(rx, ry), egui::pos2(rx + lay.col_w, ry + 80.0));
        painter.rect_filled(ram_rect, 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(ram_rect, 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(255, 0, 0)));
        painter.text(egui::pos2(rx + 15.0, ry + 20.0), egui::Align2::LEFT_CENTER, &ram_text, egui::FontId::monospace(10.0), egui::Color32::WHITE);
        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rx + 15.0, ry + 40.0), egui::pos2(rx + lay.col_w - 15.0, ry + 50.0)), 0.0, egui::Color32::from_rgb(34, 34, 34));

        let fill_w = ((lay.col_w - 30.0) * ram_percent) / 100.0;
        if fill_w > 0.0 {
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rx + 15.0, ry + 40.0), egui::pos2(rx + 15.0 + fill_w, ry + 50.0)), 0.0, egui::Color32::from_rgb(255, 0, 0));
        }

        // 6. Виджет накопителя (Disk Mappings)
        let dx = lay.col_x;
        let dy = lay.disk_y;
        let disk_rect = egui::Rect::from_min_max(egui::pos2(dx, dy), egui::pos2(dx + lay.col_w, dy + 60.0));
        painter.rect_filled(disk_rect, 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(disk_rect, 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(51, 51, 51)));
        painter.text(egui::pos2(dx + 15.0, dy + 30.0), egui::Align2::LEFT_CENTER, &disk_text, egui::FontId::monospace(10.0), egui::Color32::WHITE);

        // 7. Декоративная рамка ИИ терминала
        let tx = lay.term_x;
        let ty = lay.term_y;
        let tw = lay.term_w;
        let th = lay.term_h;
        painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(tx, ty), egui::pos2(tx + tw, ty + th)), 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(51, 51, 51)));
        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(tx, ty), egui::pos2(tx + tw, ty + 35.0)), 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(tx, ty), egui::pos2(tx + tw, ty + 35.0)), 0.0, egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(51, 51, 51)));
        painter.text(egui::pos2(tx + 15.0, ty + 17.0), egui::Align2::LEFT_CENTER, "INTEGRATED LOCAL AI KERNEL (QWEN3.5 HYBRID NATIVE)", egui::FontId::monospace(11.0), egui::Color32::from_rgb(255, 0, 0));
    }

    /// Иконки в два столбца под виджетами. Старая раскладка держала их колонкой
    /// x 40..160, y 330..610 — там же лежали RAM и Disk: иконки рисуются позже,
    /// то есть перекрывали оба виджета и перехватывали по ним клики. Теперь
    /// позиция считается от [`Layout`]: сетка начинается под Disk и сжимает шаг
    /// строк, чтобы влезть над панелью задач при любом размере окна.
    fn draw_icons(&self, ui: &egui::Ui, lay: &Layout, intents: &mut Vec<DesktopIntent>) {
        let painter = ui.painter();
        const COLS: usize = 2;
        let cell_w = lay.col_w / COLS as f32;

        for (i, icon) in self.icons.iter().enumerate() {
            let col = i % COLS;
            let row = i / COLS;
            let x = lay.col_x + col as f32 * cell_w;
            let y = lay.icons_top + row as f32 * lay.icon_pitch;
            let rect = egui::Rect::from_min_max(
                egui::pos2(x, y),
                egui::pos2(x + cell_w - 2.0, y + 40.0),
            );
            let id = ui.make_persistent_id(icon.label);
            let resp = ui.interact(rect, id, egui::Sense::click());
            let (bg, stroke, text_c) = if resp.hovered() {
                (egui::Color32::from_rgb(255, 0, 0), egui::Stroke::new(1.0_f32, egui::Color32::WHITE), egui::Color32::BLACK)
            } else {
                (egui::Color32::from_rgb(10, 10, 10), egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(50, 50, 50)), egui::Color32::WHITE)
            };

            painter.rect_filled(rect, 0.0, bg);
            painter.rect_stroke(rect, 0.0, stroke);
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, icon.label, egui::FontId::monospace(10.0), text_c);

            // Тултип — как в Windows: всегда говорим, что именно под курсором.
            // on_hover_text потребляет Response, поэтому клик проверяем до него.
            let clicked = resp.clicked();
            resp.on_hover_text(icon.hint);

            if clicked {
                // Открытие через иконку должно также разворачивать свёрнутое окно,
                // иначе клик выглядит так, будто ничего не произошло.
                match icon.action {
                    IconAction::FocusInput => intents.push(DesktopIntent::FocusInput),
                    IconAction::OpenWindow(wid) => intents.push(DesktopIntent::OpenWindow(wid)),
                }
            }
        }
    }

    /// Контекстное меню рабочего стола (ПКМ). Позицию копируем в локальную
    /// переменную: замыкание ниже берёт `&mut self` целиком, и держать за ним
    /// заимствование поля нельзя.
    fn draw_context_menu(&mut self, ctx: &egui::Context, intents: &mut Vec<DesktopIntent>) {
        if ctx.input(|i| i.pointer.secondary_clicked()) {
            if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
                self.menu_pos = Some(pos);
            }
        }
        let Some(pos) = self.menu_pos else { return };

        let mut close_menu = false;
        let menu = egui::Area::new(egui::Id::new("desktop_ctx_menu"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(235.0);
                    if ui.button("🖥  Терминал (Enter)").clicked() {
                        intents.push(DesktopIntent::FocusInput);
                        close_menu = true;
                    }
                    if ui.button("🗂  Проводник скриптов").clicked() {
                        intents.push(DesktopIntent::OpenWindow(WindowId::Scripts));
                        close_menu = true;
                    }
                    if ui.button("🧹  Очистить чат (Ctrl+L)").clicked() {
                        intents.push(DesktopIntent::ClearChat);
                        close_menu = true;
                    }
                    ui.separator();
                    if ui.button("ℹ  О системе (/hardware)").clicked() {
                        intents.push(DesktopIntent::RunCommand("/hardware".into()));
                        close_menu = true;
                    }
                    if ui.button("❓  Справка (/help)").clicked() {
                        intents.push(DesktopIntent::RunCommand("/help".into()));
                        close_menu = true;
                    }
                });
            });
        // Клик основной кнопкой мимо меню и Esc — закрытие. clicked_elsewhere()
        // ловит только primary-клик, поэтому ПКМ, которым меню переставляют,
        // его не закрывает: позиция просто обновится на следующем кадре.
        if menu.response.clicked_elsewhere() { close_menu = true; }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) { close_menu = true; }
        if close_menu { self.menu_pos = None; }
    }

    /// Панель задач внизу: слева кнопки открытых окон, справа счётчики.
    /// Полоса занимает последние 40px, а строка ввода заканчивается на win_h-45 —
    /// они не пересекаются.
    fn draw_taskbar(&self, ui: &egui::Ui, host: &DesktopHost, intents: &mut Vec<DesktopIntent>) {
        let painter = ui.painter();
        let tb_y = host.win_h - 40.0;
        let tb_rect = egui::Rect::from_min_max(egui::pos2(0.0, tb_y), egui::pos2(host.win_w, host.win_h));
        let tb_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(34, 34, 34));
        painter.rect_filled(tb_rect, 0.0, egui::Color32::from_rgb(10, 10, 10));
        painter.rect_stroke(tb_rect, 0.0, tb_stroke);

        let tb_font = egui::FontId::monospace(11.0);
        let mut bx = 10.0;
        for (idx, wid) in WindowId::ALL.iter().enumerate() {
            let (title, is_open, is_min) = {
                let w = host.windows.get(*wid);
                (w.title.clone(), w.is_open, w.minimized)
            };
            if !is_open { continue; }
            // ▣ — свёрнуто, ▢ — развёрнуто: состояние читается с одного взгляда
            let label = if is_min { format!("▣ {}", title) } else { format!("▢ {}", title) };
            let label_w = painter.layout_no_wrap(label.clone(), tb_font.clone(), egui::Color32::WHITE).size().x;
            let rect = egui::Rect::from_min_max(egui::pos2(bx, tb_y + 8.0), egui::pos2(bx + label_w + 20.0, tb_y + 32.0));
            let id = ui.make_persistent_id(("taskbar_btn", idx));
            let resp = ui.interact(rect, id, egui::Sense::click());
            let (bg, text_c) = if resp.hovered() {
                (egui::Color32::from_rgb(255, 0, 0), egui::Color32::BLACK)
            } else if is_min {
                (egui::Color32::from_rgb(16, 16, 16), egui::Color32::from_rgb(120, 120, 120))
            } else {
                (egui::Color32::from_rgb(30, 30, 30), egui::Color32::WHITE)
            };
            painter.rect_filled(rect, 0.0, bg);
            painter.rect_stroke(rect, 0.0, tb_stroke);
            painter.text(egui::pos2(bx + 10.0, tb_y + 20.0), egui::Align2::LEFT_CENTER, label, tb_font.clone(), text_c);

            if resp.clicked() {
                intents.push(DesktopIntent::ToggleWindowMinimized(*wid));
            }
            bx += label_w + 28.0;
        }

        // Справа на панели: счётчик фоновых задач (/run) и состояние ИИ-ядра.
        // Пока поток печатается, кадр и так живой — см. request_repaint в начале update.
        let bg_tasks = host.bg_tasks;
        let (ai_txt, ai_col) = if host.is_ai_streaming {
            ("AI: ◉ ПЕЧАТАЕТ", egui::Color32::from_rgb(255, 200, 0))
        } else {
            ("AI: ○ СВОБОДЕН", egui::Color32::from_rgb(85, 85, 85))
        };
        let idle_col = egui::Color32::from_rgb(85, 85, 85);
        let mut rx = host.win_w - 10.0;
        rx = draw_right_aligned(painter, rx, tb_y + 20.0, &format!("BG: {}", bg_tasks), tb_font.clone(),
            if bg_tasks > 0 { egui::Color32::from_rgb(255, 200, 0) } else { idle_col });
        draw_right_aligned(painter, rx - 14.0, tb_y + 20.0, ai_txt, tb_font.clone(), ai_col);
    }

    /// Область вывода ИИ и строка ввода команд шелла. Геометрия — из [`Layout`],
    /// та же, что у рамки терминала в `draw_chrome`, чтобы рамка и содержимое
    /// не разъезжались.
    fn draw_chat_and_input(&mut self, ui: &mut egui::Ui, host: &mut DesktopHost, lay: &Layout, intents: &mut Vec<DesktopIntent>) {
        let tx = lay.term_x; let ty = lay.term_y; let tw = lay.term_w; let th = lay.term_h;
        ui.allocate_ui_at_rect(egui::Rect::from_min_max(egui::pos2(tx + 10.0, ty + 45.0), egui::pos2(tx + tw - 10.0, ty + th - 20.0)), |ui| {
            egui::ScrollArea::vertical().id_source("ai_chat_scroll").stick_to_bottom(true).show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut *host.ai_text_area).font(egui::FontId::monospace(11.0)).text_color(egui::Color32::WHITE).desired_width(f32::INFINITY).lock_focus(true));
            });
        });

        // Строка ввода команд ШЕЛЛА — по левому краю терминала, над панелью задач
        let input_rect = egui::Rect::from_min_max(egui::pos2(lay.term_x, host.win_h - 85.0), egui::pos2(lay.term_x + lay.term_w, host.win_h - 45.0));
        ui.allocate_ui_at_rect(input_rect, |ui| {
            ui.horizontal(|ui| {
                let input_field = ui.add(egui::TextEdit::singleline(&mut *host.user_input).id(egui::Id::new(INPUT_FIELD_ID)).desired_width(ui.available_width() - 80.0).hint_text("Введите команду ядра (/help, /apps) или запрос к ИИ..."));

                // История команд: ↑/↓ перебирают прошлые запросы. TextEdit в
                // singleline-режиме эти стрелки не использует, поэтому забираем их себе.
                if input_field.has_focus() {
                    let dir = ui.input(|i| {
                        if i.key_pressed(egui::Key::ArrowUp) { Some(-1i32) }
                        else if i.key_pressed(egui::Key::ArrowDown) { Some(1i32) }
                        else { None }
                    });
                    if let Some(dir) = dir { self.walk_history(dir, ui.ctx(), host); }
                }

                if input_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.push_history(&host.user_input);
                    intents.push(DesktopIntent::SubmitInput);
                    input_field.request_focus();
                }
                if ui.button("EXEC").clicked() {
                    self.push_history(&host.user_input);
                    intents.push(DesktopIntent::SubmitInput);
                }
            });
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

    /// Прогулка по истории: `dir` = -1 (↑, старше) или +1 (↓, новее). За последней
    /// командой вниз — пустой черновик. Текст поля заменяется, курсор — в конец.
    fn walk_history(&mut self, dir: i32, ctx: &egui::Context, host: &mut DesktopHost) {
        if self.cmd_history.is_empty() { return; }
        let len = self.cmd_history.len();
        // Текущая позиция: индекс показанной команды или len — «черновик».
        let pos = self.cmd_hist_pos.unwrap_or(len);
        let new_pos = if dir < 0 {
            pos.saturating_sub(1)
        } else {
            (pos + 1).min(len)
        };
        if new_pos < len {
            *host.user_input = self.cmd_history[new_pos].clone();
            self.cmd_hist_pos = Some(new_pos);
        } else {
            host.user_input.clear();
            self.cmd_hist_pos = None;
        }
        // egui держит курсор там, где был: после подстановки ставим его в конец,
        // иначе править просмотренную команду неудобно.
        let n = host.user_input.chars().count();
        if let Some(mut state) = egui::TextEdit::load_state(ctx, egui::Id::new(INPUT_FIELD_ID)) {
            let end = egui::text::CCursor::new(n);
            state.cursor = egui::text::CCursorRange::one(end).into();
            state.store(ctx, egui::Id::new(INPUT_FIELD_ID));
        }
    }
}

/// Иконки по умолчанию: подписи, подсказки и действия. Позиции в сетке считаются
/// из [`Layout`] от размера окна — колонка `2 шт.`, шаг строк адаптивный.
fn default_icons() -> Vec<DesktopIcon> {
    vec![
        DesktopIcon {
            label: "🧠 AI CORE",
            hint: "Фокус в строку ввода (Enter)",
            action: IconAction::FocusInput,
        },
        DesktopIcon {
            label: "📝 NOTES",
            hint: "Открыть Блокнот",
            action: IconAction::OpenWindow(WindowId::Notes),
        },
        DesktopIcon {
            label: "🟢 MATRIX",
            hint: "Поток Матрицы",
            action: IconAction::OpenWindow(WindowId::Matrix),
        },
        DesktopIcon {
            label: "🧹 CLEANER",
            hint: "Очистка логов и кэша VRAM",
            action: IconAction::OpenWindow(WindowId::Cleaner),
        },
        DesktopIcon {
            label: "🚀 LAUNCHER",
            hint: "Реестр ИИ-софта (/apps)",
            action: IconAction::OpenWindow(WindowId::Apps),
        },
        DesktopIcon {
            label: "🗂 SCRIPTS",
            hint: "Проводник скриптов",
            action: IconAction::OpenWindow(WindowId::Scripts),
        },
        DesktopIcon {
            label: "🧬 TRAINER",
            hint: "Самообучение модели (QLoRA)",
            action: IconAction::OpenWindow(WindowId::Trainer),
        },
    ]
}

/// Рисует текст, выровненный по правому краю, и возвращает новую левую границу —
/// чтобы выкладывать рядом сегменты разного цвета справа налево.
fn draw_right_aligned(
    painter: &egui::Painter, right_x: f32, y: f32, text: &str,
    font: egui::FontId, color: egui::Color32,
) -> f32 {
    let width = painter.layout_no_wrap(text.to_string(), font.clone(), color).size().x;
    let x = right_x - width;
    painter.text(egui::pos2(x, y), egui::Align2::LEFT_CENTER, text, font, color);
    x
}
