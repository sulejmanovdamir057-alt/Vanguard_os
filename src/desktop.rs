//! Рабочий стол Vanguard OS в стиле Windows 11: обои с киберпанк-подложкой,
//! иконки с двойным кликом, центрированная панель задач и меню «Пуск».
//!
//! Стол не владеет ни окнами, ни шеллом, ни чатом — он получает доступ к ним
//! через [`DesktopHost`] (набор заимствований на один кадр) и возвращает
//! [`DesktopIntent`] — список того, что приложение должно выполнить
//! (`VanguardOSApp::apply_desktop_intent`). Благодаря этому раскладка не тянет
//! за собой `&mut self` и не дерётся с borrow-checker'ом.

use chrono::{Datelike, Local, NaiveDate};
use eframe::egui;

use crate::ui_window::{VanguardWindows, WindowId, TASKBAR_H};

/// Идентификатор строки ввода в памяти egui. Общая константа: стол запрашивает
/// фокус для неё по хоткею Enter и по иконке, а приложение — при старте.
pub const INPUT_FIELD_ID: &str = "ai_input_field";

/// Цвет подсветки наведения на тёмном хроме (белый с малой альфой).
fn hover_bg() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 22)
}

/// Месяцы для заголовка календаря (именительный: «Октябрь 2026»).
const MONTHS_RU: [&str; 12] = [
    "Январь", "Февраль", "Март", "Апрель", "Май", "Июнь", "Июль", "Август", "Сентябрь", "Октябрь",
    "Ноябрь", "Декабрь",
];

/// Те же месяцы в родительном падеже — для строки полной даты («13 октября 2026»).
/// Русский требует двух форм, поэтому массивов два, а не один.
const MONTHS_RU_GEN: [&str; 12] = [
    "января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября",
    "октября", "ноября", "декабря",
];

/// Полные названия дней недели, индекс = `weekday().num_days_from_monday()` (пн = 0).
const WEEKDAYS_RU_FULL: [&str; 7] = [
    "Понедельник", "Вторник", "Среда", "Четверг", "Пятница", "Суббота", "Воскресенье",
];

/// Иконка рабочего стола. Позицию в сетке считает `draw_icons` от размера
/// окна — здесь только то, что объявляет автор иконки.
pub struct DesktopIcon {
    pub glyph: &'static str,
    pub label: &'static str,
    /// Подсказка при наведении (Windows-привычка: всегда говорить, что под курсором).
    pub hint: &'static str,
    pub action: IconAction,
}

/// Что делает двойной клик по иконке. Терминал — особый случай: иконка не
/// просто открывает окно, а ещё и ставит фокус в строку ввода.
pub enum IconAction {
    FocusInput,
    OpenWindow(WindowId),
    /// Браузер (стадия 1): системный браузер на localhost:8787 — не окно egui,
    /// поэтому не живёт в WindowId.
    OpenBrowser,
}

/// Намерение стола. Стол сам не вызывает шелл и не пишет в чат — он лишь
/// перечисляет, что нужно сделать, а `app.rs` исполняет.
pub enum DesktopIntent {
    /// Открыть терминал и перенести фокус в строку ввода (Enter на столе,
    /// иконка, «Пуск»). Сама отправка запроса живёт в draw_terminal_window:
    /// строка ввода — часть окна терминала, а не стола.
    FocusInput,
    /// Слеш-команда: `/help`, `/hardware`, `/run <id>` и т. п.
    RunCommand(String),
    /// Очистить чат и память диалога (Ctrl+L, пункт меню).
    ClearChat,
    /// Развернуть окно (иконка, пункт меню, «Пуск»).
    OpenWindow(WindowId),
    /// Клик по кнопке панели задач: свернуть/развернуть.
    ToggleWindowMinimized(WindowId),
    /// Тонкая полоска справа внизу: свернуть все окна (как Show Desktop).
    MinimizeAll,
    /// Спросить ИИ (пункт контекстного меню): открывает панель ИИ с фокусом в
    /// её строке ввода. Сам запрос панель собирает и отправляет уже в `app.rs` —
    /// это состояние приложения, а не поверхность стола.
    AskAi,
    /// Запустить установленное приложение из «Все приложения» меню «Пуск»
    /// (id из `apps/`, не окно — у окон свои намерения через `open_intent`).
    LaunchApp(String),
    /// Открыть браузер (стадия 1): системный браузер на localhost:8787; сервер
    /// FastAPI поднимает сама оболочка при первом открытии.
    OpenBrowser,
}

/// Всё, что столу нужно от приложения. Живёт ровно один кадр, собирается
/// в `update()` из полей `VanguardOSApp`.
pub struct DesktopHost<'a> {
    /// Реальный размер вьюпорта в этом кадру — берётся из `ctx.screen_rect()`
    /// в `update()`, а не из константы: в развёрнутом/полноэкранном окне
    /// панель задач, меню «Пуск» и календарь должны тянуться на весь экран.
    pub win_w: f32,
    pub win_h: f32,
    /// Окна целиком: панель задач, «Пуск» и иконки обращаются через `WindowId`.
    pub windows: &'a mut VanguardWindows,
    /// Онлайн ли бэкенд ИИ — обновляет фоновый TCP-пробник из `new()`.
    pub backend_online: bool,
    pub is_ai_streaming: bool,
    pub bg_tasks: usize,
    /// Реестр сгенерированных скриптов (id, имя) — раздел «Рекомендуется»
    /// меню «Пуск»; собирает app.rs, и только пока открыто меню.
    pub recent_scripts: &'a [(String, String)],
    /// Установленные приложения (id, имя, глиф из манифеста) — секция «Все
    /// приложения» меню «Пуск»; собирает app.rs, только пока открыто меню.
    pub apps: &'a [(String, String, String)],
}

/// Состояние стола, принадлежащее только ему.
pub struct Desktop {
    pub icons: Vec<DesktopIcon>,
    /// Контекстное меню: `Some(позиция курсора)` — открыто, `None` — закрыто.
    pub menu_pos: Option<egui::Pos2>,
    /// Меню «Пуск» открыто.
    pub start_open: bool,
    /// Строка поиска внутри «Пуска» (фильтрует список закреплённых).
    pub start_query: String,
    /// Вид внутри «Пуска»: `false` — сетка закреплённых плиток, `true` —
    /// плоский список «Все приложения». Сбрасывается при закрытии меню.
    pub start_all_apps: bool,
    /// Выбранная (одиночный клик) иконка; двойной клик по ней открывает приложение.
    pub selected_icon: Option<usize>,
    /// Календарь над часами в трее открыт.
    pub clock_open: bool,
    /// Год и месяц, которые листает пользователь. Сбрасываются на текущие
    /// при каждом открытии — календарь начинается с «сегодня», как в Windows.
    clock_year: i32,
    clock_month: u32,
}

impl Default for Desktop {
    fn default() -> Self {
        Self::new()
    }
}

impl Desktop {
    pub fn new() -> Self {
        let now = Local::now();
        Self {
            icons: default_icons(),
            menu_pos: None,
            start_open: false,
            start_query: String::new(),
            start_all_apps: false,
            selected_icon: None,
            clock_open: false,
            clock_year: now.year(),
            clock_month: now.month(),
        }
    }

    /// Главная точка входа стола. Возвращает намерения, которые приложение
    /// должно исполнить.
    pub fn draw(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, host: &mut DesktopHost) -> Vec<DesktopIntent> {
        let mut intents = Vec::new();
        self.draw_wallpaper(ui);
        self.draw_icons(ui, &mut intents);
        self.draw_context_menu(ctx, &mut intents);
        // «Пуск» и календарь взаимоисключающиы: клик по одному закрывает другой,
        // поэтому клики возвращаются наружу и гасятся внутри соответствующих методов.
        let (start_clicked, clock_clicked) = self.draw_taskbar(ui, host, &mut intents);
        self.draw_start_menu(ctx, host, &mut intents, start_clicked);
        self.draw_clock_popup(ctx, host, clock_clicked);
        intents
    }

    /// Горячие клавиши стола. Отдельный метод: их проверка жила в `update()`
    /// до отрисовки окон и должна остаться там же, чтобы порядок ввода не сместился.
    pub fn handle_hotkeys(&self, ctx: &egui::Context) -> Vec<DesktopIntent> {
        let mut intents = Vec::new();
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            intents.push(DesktopIntent::RunCommand("/help".into()));
        }
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::L)) {
            intents.push(DesktopIntent::ClearChat);
        }
        // Enter, когда клавиатура ничья — переносим фокус в строку ввода терминала.
        // wants_keyboard_input() означает «кто-то держит клавиатуру»: так мы не
        // перехватываем Enter у набранной команды и не дублируем отправку.
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) && !ctx.wants_keyboard_input() {
            intents.push(DesktopIntent::FocusInput);
        }
        intents
    }

    /// Обои: база, едва заметная сетка, красное свечение и водяной знак логотипа.
    /// Клик по пустому месту снимает выделение с иконки.
    fn draw_wallpaper(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(15, 17, 24));

        // Киберпанк-сетка — очень слабая, чтобы не спорить с окнами поверх.
        let grid = egui::Stroke::new(1.0_f32, egui::Color32::from_rgba_unmultiplied(140, 160, 200, 12));
        let mut x = 0.0;
        while x < rect.max.x {
            painter.line_segment([egui::pos2(x, 0.0), egui::pos2(x, rect.max.y)], grid);
            x += 110.0;
        }
        let mut y = 0.0;
        while y < rect.max.y {
            painter.line_segment([egui::pos2(0.0, y), egui::pos2(rect.max.x, y)], grid);
            y += 110.0;
        }

        // Красное свечение в правом нижнем углу — «реактор» под логотипом.
        // Альфы спадают к краю, чтобы центр не сливался в плотное пятно: каждое
        // кольцо чуть прозрачнее предыдущего, ядро ярче, но не заливает.
        let glow_c = egui::pos2(rect.max.x * 0.76, rect.max.y * 0.66);
        for (n, r) in (60..=300).step_by(15).enumerate() {
            let a = (3 - (n as i32 / 5)).clamp(0, 3) as u8; // 3..0 по мере удаления
            if a == 0 {
                continue;
            }
            painter.circle_filled(glow_c, r as f32, egui::Color32::from_rgba_unmultiplied(255, 30, 30, a));
        }

        // Водяной знак логотипа.
        let wm = egui::Color32::from_rgba_unmultiplied(255, 70, 70, 26);
        painter.text(
            egui::pos2(rect.max.x - 36.0, rect.max.y - TASKBAR_H - 84.0),
            egui::Align2::RIGHT_CENTER,
            "VANGUARD",
            egui::FontId::monospace(56.0),
            wm,
        );
        painter.text(
            egui::pos2(rect.max.x - 40.0, rect.max.y - TASKBAR_H - 42.0),
            egui::Align2::RIGHT_CENTER,
            "SOVEREIGN ENVIRONMENT v0.2",
            egui::FontId::monospace(11.0),
            egui::Color32::from_rgba_unmultiplied(200, 200, 200, 40),
        );

        // Клик по обоям — снять выделение с иконок. Интерact регистрируется
        // раньше иконок, поэтому они перехватывают свои клики первыми.
        let resp = ui.interact(rect, egui::Id::new("wallpaper"), egui::Sense::click());
        if resp.clicked() {
            self.selected_icon = None;
        }
    }

    /// Иконки слева сверху, как в Windows: сетка 1–2 столбца (второй появляется,
    /// если первый не вмещает все иконки). Одиночный клик — выделение,
    /// двойной — открытие.
    fn draw_icons(&mut self, ui: &egui::Ui, intents: &mut Vec<DesktopIntent>) {
        const CELL_W: f32 = 92.0;
        const CELL_H: f32 = 98.0;
        let painter = ui.painter();
        let max_rows = (((ui.max_rect().height() - TASKBAR_H - 16.0) / CELL_H) as usize).max(1);

        for i in 0..self.icons.len() {
            let col = i / max_rows;
            let row = i % max_rows;
            let x = 10.0 + col as f32 * CELL_W;
            let y = 8.0 + row as f32 * CELL_H;
            let cell = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(CELL_W - 4.0, CELL_H - 6.0));
            let id = ui.make_persistent_id(("desk_icon", i));
            let resp = ui.interact(cell, id, egui::Sense::click());

            let selected = self.selected_icon == Some(i);
            let hover = resp.hovered();
            // Подсветка всей ячейки — синим акцентом Windows при наведении/выделении.
            if selected {
                painter.rect_filled(cell, 6.0, egui::Color32::from_rgba_unmultiplied(0, 120, 212, 60));
            } else if hover {
                painter.rect_filled(cell, 6.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 16));
            }

            // Плитка-иконка: скруглённый квадрат с глифом по центру. Прижата
            // к верху ячейки, а подпись идёт сразу под ней — тогда каждая
            // подпись визуально принадлежит своей иконке, а не соседней ниже.
            const TILE: f32 = 50.0;
            let tile = egui::Rect::from_min_size(
                egui::pos2(cell.center().x - TILE / 2.0, cell.min.y),
                egui::vec2(TILE, TILE),
            );
            let tile_bg = if hover || selected {
                egui::Color32::from_rgb(46, 50, 60)
            } else {
                egui::Color32::from_rgb(36, 39, 47)
            };
            painter.rect_filled(tile, 10.0, tile_bg);
            if selected {
                painter.rect_stroke(tile, 10.0, egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(0, 120, 212)));
            }
            painter.text(tile.center(), egui::Align2::CENTER_CENTER, self.icons[i].glyph, egui::FontId::proportional(22.0), egui::Color32::WHITE);

            // Подпись под своей плиткой (не по cell.max.y — иначе уезжает к
            // следующей иконке). Белая при наведении/выделении, серая иначе.
            let label_c = if hover || selected { egui::Color32::WHITE } else { egui::Color32::from_rgb(210, 210, 215) };
            painter.text(
                egui::pos2(cell.center().x, tile.max.y + 13.0),
                egui::Align2::CENTER_CENTER,
                self.icons[i].label,
                egui::FontId::proportional(11.0),
                label_c,
            );

            let hint = self.icons[i].hint.to_string();
            let clicked = resp.clicked();
            let dbl = resp.double_clicked();
            resp.on_hover_text(hint);

            if clicked && !dbl {
                self.selected_icon = Some(i);
            }
            if dbl {
                match self.icons[i].action {
                    IconAction::FocusInput => intents.push(DesktopIntent::FocusInput),
                    IconAction::OpenWindow(wid) => intents.push(DesktopIntent::OpenWindow(wid)),
                    IconAction::OpenBrowser => intents.push(DesktopIntent::OpenBrowser),
                }
            }
        }
    }

    /// Контекстное меню рабочего стола (ПКМ) в духе Windows 11.
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
                    if ui.button("❯  Открыть терминал").clicked() {
                        intents.push(DesktopIntent::FocusInput);
                        close_menu = true;
                    }
                    if ui.button("»  Спросить ИИ").clicked() {
                        intents.push(DesktopIntent::AskAi);
                        close_menu = true;
                    }
                    if ui.button("▦  Монитор системы").clicked() {
                        intents.push(DesktopIntent::OpenWindow(WindowId::Monitor));
                        close_menu = true;
                    }
                    if ui.button("≡  Проводник скриптов").clicked() {
                        intents.push(DesktopIntent::OpenWindow(WindowId::Scripts));
                        close_menu = true;
                    }
                    ui.separator();
                    if ui.button("⚙  О системе (/hardware)").clicked() {
                        intents.push(DesktopIntent::RunCommand("/hardware".into()));
                        close_menu = true;
                    }
                    if ui.button("ⓘ  Справка (/help)").clicked() {
                        intents.push(DesktopIntent::RunCommand("/help".into()));
                        close_menu = true;
                    }
                });
            });
        // Клик основной кнопкой мимо меню и Esc — закрытие. clicked_elsewhere()
        // ловит только primary-клик, поэтому ПКМ меню переставляет, а не закрывает.
        if menu.response.clicked_elsewhere() {
            close_menu = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close_menu = true;
        }
        if close_menu {
            self.menu_pos = None;
        }
    }

    /// Панель задач Windows 11: кнопка «Пуск» и закреплённые приложения — по
    /// центру, часы и индикаторы — справа, тонкая полоска «Свернуть все» у
    /// самого края. Возвращает `(клик по «Пуску», клик по часам)` — оба меню
    /// используют это, чтобы не закрыться от собственного открытия.
    fn draw_taskbar(&mut self, ui: &egui::Ui, host: &DesktopHost, intents: &mut Vec<DesktopIntent>) -> (bool, bool) {
        let painter = ui.painter();
        let tb_y = host.win_h - TASKBAR_H;
        let tb_rect = egui::Rect::from_min_max(egui::pos2(0.0, tb_y), egui::pos2(host.win_w, host.win_h));
        painter.rect_filled(tb_rect, 0.0, egui::Color32::from_rgb(28, 28, 30));
        painter.line_segment(
            [egui::pos2(0.0, tb_y), egui::pos2(host.win_w, tb_y)],
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(56, 56, 60)),
        );

        // ── Центральный блок: Пуск + закреплённые приложения ──
        const BTN: f32 = 44.0;
        let n = (WindowId::ALL.len() + 1) as f32;
        let mut cx = ((host.win_w - n * BTN) / 2.0).max(8.0);
        let btn_y = tb_y + (TASKBAR_H - BTN) / 2.0;

        // Кнопка «Пуск»: наше красное лого-V.
        let start_rect = egui::Rect::from_min_size(egui::pos2(cx, btn_y), egui::vec2(BTN, BTN));
        let start_resp = ui.interact(start_rect, egui::Id::new("taskbar_start"), egui::Sense::click());
        if start_resp.hovered() || self.start_open {
            painter.rect_filled(start_rect, 6.0, hover_bg());
        }
        painter.text(start_rect.center(), egui::Align2::CENTER_CENTER, "V", egui::FontId::proportional(20.0), egui::Color32::from_rgb(255, 60, 60));
        let mut start_clicked = false;
        if start_resp.clicked() {
            self.start_open = !self.start_open;
            self.start_query.clear();
            self.start_all_apps = false;
            start_clicked = true;
            // «Пуск» и календарь не живут одновременно — как в Windows.
            self.clock_open = false;
        }
        start_resp.on_hover_text("Пуск");
        cx += BTN;

        // Кнопки приложений: глиф, под ним «пилюля» состояния (как в Win 11).
        for wid in WindowId::ALL {
            let w = host.windows.get(wid);
            let (is_open, is_min, title) = (w.is_open, w.minimized, w.title.clone());
            let rect = egui::Rect::from_min_size(egui::pos2(cx, btn_y), egui::vec2(BTN, BTN));
            let resp = ui.interact(rect, egui::Id::new(("taskbar_app", wid)), egui::Sense::click());
            if resp.hovered() {
                painter.rect_filled(rect, 6.0, hover_bg());
            }
            painter.text(rect.center_top() + egui::vec2(0.0, 21.0), egui::Align2::CENTER_CENTER, wid.glyph(), egui::FontId::proportional(18.0), egui::Color32::WHITE);
            // Активное окно — широкая пилюля, свёрнутое — короткая, закрыто — нет.
            if is_open && !is_min {
                let pill = egui::Rect::from_min_size(egui::pos2(cx + 14.0, tb_y + TASKBAR_H - 4.0), egui::vec2(16.0, 3.0));
                painter.rect_filled(pill, 1.5, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 220));
            } else if is_open {
                let pill = egui::Rect::from_min_size(egui::pos2(cx + 20.0, tb_y + TASKBAR_H - 4.0), egui::vec2(5.0, 3.0));
                painter.rect_filled(pill, 1.5, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 130));
            }
            // on_hover_text потребляет Response (self) — читаем клик до подсказки.
            let clicked = resp.clicked();
            resp.on_hover_text(title);
            if clicked {
                intents.push(DesktopIntent::ToggleWindowMinimized(wid));
            }
            cx += BTN;
        }

        // ── Правый трей: индикаторы задач и ИИ, часы, полоска «Свернуть все» ──
        let mut rx = host.win_w - 20.0;

        // Часы: две строки, как в Windows (время над датой).
        let now = Local::now();
        let (time_s, date_s) = (now.format("%H:%M").to_string(), now.format("%d.%m.%Y").to_string());
        let clock_rect = egui::Rect::from_min_max(
            egui::pos2(rx - 70.0, tb_y + 4.0),
            egui::pos2(rx + 10.0, tb_y + TASKBAR_H - 2.0),
        );
        let clock_resp = ui.interact(clock_rect, egui::Id::new("taskbar_clock"), egui::Sense::click());
        if clock_resp.hovered() {
            painter.rect_filled(clock_rect, 4.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12));
        }
        painter.text(egui::pos2(rx, tb_y + 16.0), egui::Align2::RIGHT_CENTER, time_s, egui::FontId::proportional(12.0), egui::Color32::WHITE);
        painter.text(egui::pos2(rx, tb_y + 33.0), egui::Align2::RIGHT_CENTER, date_s, egui::FontId::proportional(10.0), egui::Color32::from_rgb(180, 180, 185));
        // Клик по часам открывает/закрывает календарь. `on_hover_text` потребляет
        // Response, поэтому клик читаем заранее — тот же приём, что у кнопок
        // приложений и полоски «Свернуть все».
        let clock_clicked = clock_resp.clicked();
        clock_resp.on_hover_text(now.format("%A, %d %B %Y").to_string());
        if clock_clicked {
            self.clock_open = !self.clock_open;
            if self.clock_open {
                // Листание всегда начинается с текущего месяца.
                let n = Local::now();
                self.clock_year = n.year();
                self.clock_month = n.month();
            }
            self.start_open = false;
            self.start_query.clear();
            self.start_all_apps = false;
        }
        rx -= 86.0;

        // Индикаторы: состояние ИИ и фоновые задачи (как «иконки трея»).
        if host.is_ai_streaming {
            painter.text(egui::pos2(rx, tb_y + TASKBAR_H / 2.0), egui::Align2::RIGHT_CENTER, "◉", egui::FontId::proportional(13.0), egui::Color32::from_rgb(255, 200, 0));
            rx -= 18.0;
        }
        if host.bg_tasks > 0 {
            painter.text(egui::pos2(rx, tb_y + TASKBAR_H / 2.0), egui::Align2::RIGHT_CENTER, format!("⚙{}", host.bg_tasks), egui::FontId::proportional(12.0), egui::Color32::from_rgb(255, 200, 0));
            rx -= 26.0;
        }

        // Точка бэкенда ИИ: зелёная — ядро на связи, красная — Ollama молчит.
        let dot_c = egui::pos2(rx - 4.0, tb_y + TASKBAR_H / 2.0);
        painter.circle_filled(
            dot_c,
            4.0,
            if host.backend_online {
                egui::Color32::from_rgb(80, 220, 120)
            } else {
                egui::Color32::from_rgb(255, 80, 80)
            },
        );
        let dot_resp = ui.interact(
            egui::Rect::from_center_size(dot_c, egui::vec2(16.0, 16.0)),
            egui::Id::new("taskbar_backend_dot"),
            egui::Sense::click(),
        );
        dot_resp.on_hover_text(if host.backend_online {
            "Ядро ИИ: ONLINE"
        } else {
            "Ядро ИИ: OFFLINE — запустите ollama serve"
        });

        // Тонкая полоска у края — «Свернуть все окна» (Show Desktop).
        let sd_rect = egui::Rect::from_min_max(egui::pos2(host.win_w - 5.0, tb_y + 2.0), egui::pos2(host.win_w - 1.0, host.win_h - 2.0));
        let sd_resp = ui.interact(sd_rect, egui::Id::new("taskbar_show_desktop"), egui::Sense::click());
        painter.rect_filled(sd_rect, 1.0, if sd_resp.hovered() { egui::Color32::from_rgb(120, 120, 120) } else { egui::Color32::from_rgb(70, 70, 74) });
        // on_hover_text потребляет Response — клик читаем заранее.
        let sd_clicked = sd_resp.clicked();
        sd_resp.on_hover_text("Свернуть все окна");
        if sd_clicked {
            intents.push(DesktopIntent::MinimizeAll);
        }

        (start_clicked, clock_clicked)
    }

    /// Меню «Пуск» в духе Windows 11: панель на 640 px, прижатая низом к панели
    /// задач, поиск-пилюля, сетка закреплённых плиток «иконка сверху», раздел
    /// «Рекомендуется» и футер с пользователем и питанием.
    ///
    /// Якорь — `Area::anchor(CENTER_BOTTOM)`, а не вычисленный `fixed_pos`: он
    /// считается от `ctx.screen_rect()`, то есть от реального вьюпорта, поэтому
    /// низ меню сам встаёт над панелью задач при любом размере окна и при F11.
    /// Старая схема (`win_h - TASKBAR_H - MENU_H`) держала высоту контента в
    /// константе, а при фильтре поиска сетка теряла ряды и меню дёргалось —
    /// поэтому ниже пустые ряды и пустые ячейки **резервируются** явно.
    fn draw_start_menu(
        &mut self,
        ctx: &egui::Context,
        host: &DesktopHost,
        intents: &mut Vec<DesktopIntent>,
        start_clicked: bool,
    ) {
        if !self.start_open {
            return;
        }
        const MENU_W: f32 = 640.0;
        // Оценка на самый первый кадр: Area ещё не измерила контент и берёт
        // default_size. Дальше она помнит свой размер сама.
        const MENU_H: f32 = 520.0;
        // Скольким над панелью задач — как в Windows, меню почти касается её.
        const GAP: f32 = 6.0;
        // Геометрия плиток: 4 колонки, три ряда резервируются всегда.
        const TILE_H: f32 = 74.0;
        const TILE_GAP: f32 = 8.0;
        const GRID_ROWS: usize = 3;
        // Ячейка «Рекомендуется» и её сетка 2×2 — тоже всегда четыре.
        const CELL_H: f32 = 26.0;
        const REC_ROWS: usize = 2;

        let mut close = false;
        let mut open_first = false;
        let menu = egui::Area::new(egui::Id::new("start_menu"))
            .order(egui::Order::Foreground)
            .anchor(
                egui::Align2::CENTER_BOTTOM,
                egui::vec2(0.0, -(TASKBAR_H + GAP)),
            )
            .default_size(egui::vec2(MENU_W, MENU_H))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(egui::Color32::from_rgb(36, 36, 40))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(70, 70, 76)))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.set_min_width(MENU_W - 32.0);
                        ui.set_max_width(MENU_W - 32.0);

                        // ── Поиск: пилюля с рамкой, как в Windows 11 ──
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(32, 32, 36))
                            .stroke(egui::Stroke::new(
                                1.0_f32,
                                egui::Color32::from_rgb(72, 72, 80),
                            ))
                            .rounding(egui::Rounding::same(6.0))
                            .inner_margin(10.0_f32)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    ui.label(
                                        egui::RichText::new("🔍")
                                            .size(13.0)
                                            .color(egui::Color32::from_rgb(170, 170, 178)),
                                    );
                                    let r = ui.add(
                                        egui::TextEdit::singleline(&mut self.start_query)
                                            .hint_text("Поиск: приложения, настройки и документы")
                                            .frame(false)
                                            .desired_width(f32::INFINITY),
                                    );
                                    // Enter открывает первое совпадение. Проверяем
                                    // и has_focus, и lost_focus: однострочный TextEdit
                                    // не снимает фокус с Enter, так что lost_focus
                                    // один в этом состоянии никогда не сработал бы.
                                    if (r.has_focus() || r.lost_focus())
                                        && ctx.input(|i| i.key_pressed(egui::Key::Enter))
                                    {
                                        open_first = true;
                                    }
                                });
                            });

                        if open_first {
                            let q = self.start_query.to_lowercase();
                            if let Some(icon) = self
                                .icons
                                .iter()
                                .find(|ic| ic.label.to_lowercase().contains(&q))
                            {
                                intents.push(match icon.action {
                                    IconAction::FocusInput => DesktopIntent::FocusInput,
                                    IconAction::OpenWindow(wid) => DesktopIntent::OpenWindow(wid),
                                    IconAction::OpenBrowser => DesktopIntent::OpenBrowser,
                                });
                                close = true;
                            }
                        }
                        ui.add_space(14.0);

                        // ── Заголовок: «Закреплено» слева, «Все приложения» справа ──
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Закреплено").size(13.0).strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let label = if self.start_all_apps {
                                        "‹  К закреплённым"
                                    } else {
                                        "Все приложения  ›"
                                    };
                                    if ui.small_button(label).clicked() {
                                        self.start_all_apps = !self.start_all_apps;
                                    }
                                },
                            );
                        });
                        ui.add_space(6.0);

                        let q = self.start_query.to_lowercase();
                        if self.start_all_apps {
                            // ── Все приложения: один колонкой список, без буквенных
                            // разделителей — высота фиксирована прокруткой, чтобы
                            // якорь меню не дёргался при вводе в поиске. ──
                            let mut all: Vec<WindowId> = WindowId::ALL
                                .iter()
                                .copied()
                                .filter(|w| q.is_empty() || w.label().to_lowercase().contains(&q))
                                .collect();
                            all.sort_by_key(|w| w.label().to_lowercase());
                            egui::ScrollArea::vertical()
                                .id_source("start_all_apps")
                                .max_height((TILE_H + TILE_GAP) * GRID_ROWS as f32 - TILE_GAP)
                                .auto_shrink([true, false])
                                .show(ui, |ui| {
                                    ui.with_layout(
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            for wid in all {
                                                if start_app_row(ui, wid, CELL_H) {
                                                    intents.push(open_intent(wid));
                                                    close = true;
                                                }
                                            }
                                            // Браузер — не окно: строка вне
                                            // WindowId::ALL, с тем же фильтром поиска.
                                            if (q.is_empty() || "браузер".contains(&q))
                                                && start_named_row(ui, "◎", "Браузер", CELL_H)
                                            {
                                                intents.push(DesktopIntent::OpenBrowser);
                                                close = true;
                                            }
                                            // Установленные приложения из apps/:
                                            // своя строка и своё намерение — это
                                            // не окна, их не открывает open_intent.
                                            if !host.apps.is_empty() {
                                                ui.add_space(8.0);
                                                ui.label(
                                                    egui::RichText::new("Установленные")
                                                        .size(11.0)
                                                        .color(egui::Color32::from_rgb(150, 150, 160)),
                                                );
                                                for (id, name, glyph) in host.apps {
                                                    if !q.is_empty()
                                                        && !name.to_lowercase().contains(&q)
                                                    {
                                                        continue;
                                                    }
                                                    if start_named_row(ui, glyph, name, CELL_H) {
                                                        intents.push(DesktopIntent::LaunchApp(
                                                            id.clone(),
                                                        ));
                                                        close = true;
                                                    }
                                                }
                                            }
                                        },
                                    );
                                });
                        } else {
                            // ── Закреплено: сетка 4 колонки, «иконка сверху» ──
                            let pinned: Vec<WindowId> = WindowId::ALL
                                .iter()
                                .copied()
                                .filter(|w| q.is_empty() || w.label().to_lowercase().contains(&q))
                                .collect();
                            let cw = ((ui.available_width() - TILE_GAP * 3.0) / 4.0).max(90.0);
                            let mut used = 0usize;
                            for chunk in pinned.chunks(4) {
                                used += 1;
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = TILE_GAP;
                                    for wid in chunk {
                                        let id =
                                            ui.make_persistent_id(("start_tile", *wid));
                                        if start_tile(ui, wid.glyph(), wid.label(), id, cw, TILE_H)
                                        {
                                            intents.push(open_intent(*wid));
                                            close = true;
                                        }
                                    }
                                });
                            }
                            // Браузер — не окно (стадия 1 открывает системный
                            // браузер), поэтому отдельной плиткой, а не в ALL;
                            // счётчик used держит высоту меню стабильной.
                            if q.is_empty() || "браузер".contains(&q) {
                                used += 1;
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = TILE_GAP;
                                    let id = ui.make_persistent_id("start_tile_browser");
                                    if start_tile(ui, "◎", "Браузер", id, cw, TILE_H) {
                                        intents.push(DesktopIntent::OpenBrowser);
                                        close = true;
                                    }
                                });
                            }
                            // Пустые ряды: без них высота меню падала бы при
                            // вводе в поиске (два совпавших = один ряд вместо трёх),
                            // и панель прыгала бы вверх-вниз.
                            let rest = GRID_ROWS.saturating_sub(used);
                            if rest > 0 {
                                ui.add_space(rest as f32 * (TILE_H + TILE_GAP));
                            }
                        }

                        ui.add_space(14.0);

                        // ── Рекомендуется: сетка 2×2, пустые ячейки заняты
                        // подсказкой/просто местом — высота не зависит от того,
                        // сколько скриптов уже сгенерировано. ──
                        ui.label(egui::RichText::new("Рекомендуется").size(13.0).strong());
                        ui.add_space(6.0);
                        let cell_w = (ui.available_width() - TILE_GAP) / 2.0;
                        for row in 0..REC_ROWS {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = TILE_GAP;
                                for col in 0..2 {
                                    let i = row * 2 + col;
                                    match host.recent_scripts.get(i) {
                                        Some((id, name)) => {
                                            let btn = ui.add_sized(
                                                [cell_w, CELL_H],
                                                egui::Button::new(
                                                    egui::RichText::new(format!("▸  {}", name))
                                                        .size(11.0),
                                                )
                                                .frame(false),
                                            );
                                            if btn.clicked() {
                                                intents.push(DesktopIntent::RunCommand(
                                                    format!("/run {}", id),
                                                ));
                                                close = true;
                                            }
                                        }
                                        // Пустая ячейка: отключённая кнопка держит
                                        // ровно ту же высоту, что и настоящая строка.
                                        None => {
                                            let hint = if i == 0 {
                                                "Спросите ИИ: напишите скрипт — он появится здесь."
                                            } else {
                                                ""
                                            };
                                            ui.add_enabled(
                                                false,
                                                egui::Button::new(
                                                    egui::RichText::new(hint).size(11.0),
                                                )
                                                .frame(false),
                                            );
                                        }
                                    }
                                }
                            });
                        }

                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);

                        // ── Нижняя строка: аватар с именем слева, питание справа ──
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(26.0, 26.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(
                                rect.center(),
                                13.0,
                                egui::Color32::from_rgb(64, 104, 176),
                            );
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "D",
                                egui::FontId::proportional(13.0),
                                egui::Color32::WHITE,
                            );
                            ui.label(egui::RichText::new("DamirSeversk").size(12.0));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("⏻  Выключение").clicked() {
                                        intents
                                            .push(DesktopIntent::RunCommand("/exit".into()));
                                        close = true;
                                    }
                                },
                            );
                        });
                    });
            });

        // Закрытие: клик мимо (кроме клика по самой кнопке «Пуск», который
        // уже переключил состояние) и Esc.
        if menu.response.clicked_elsewhere() && !start_clicked {
            close = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !start_clicked {
            close = true;
        }
        if close {
            self.start_open = false;
            self.start_query.clear();
            // Вид «Все приложения» живёт ровно до закрытия меню.
            self.start_all_apps = false;
        }
    }

    /// Часы в трее: клик открывает календарь месяца с текущим временем над
    /// панелью задач, прижатый вправо — как в Windows 11.
    ///
    /// Расположение, закрытие по клику мимо и по Esc — тот же паттерн, что у
    /// меню «Пуск», включая защиту через `clock_clicked`: без неё
    /// `clicked_elsewhere()` закрыл бы календарь тем же кликом, что его открыл.
    fn draw_clock_popup(&mut self, ctx: &egui::Context, host: &DesktopHost, clock_clicked: bool) {
        if !self.clock_open {
            return;
        }
        const W: f32 = 300.0;
        // Не высота окна (Area ресайзится под контент сама), а отступ от низа:
        // сколько места отвести сверху, чтобы низ оказался над панелью задач.
        const H: f32 = 320.0;
        let x = (host.win_w - W - 12.0).max(8.0);
        let y = (host.win_h - TASKBAR_H - H - 8.0).max(8.0);

        let mut close = false;
        let menu = egui::Area::new(egui::Id::new("clock_popup"))
            .fixed_pos(egui::pos2(x, y))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(egui::Color32::from_rgb(43, 43, 49))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(70, 70, 78)))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(14.0)
                    .show(ui, |ui| {
                        ui.set_min_width(W - 28.0);
                        ui.set_max_width(W - 28.0);

                        // ── Текущее время. Секунды идут сами: update() держит
                        // кадр живым раз в секунду, пока стол открыт. ──
                        let now = Local::now();
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(now.format("%H:%M:%S").to_string())
                                    .size(34.0)
                                    .strong(),
                            );
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}, {} {} {}",
                                    WEEKDAYS_RU_FULL[now.weekday().num_days_from_monday() as usize],
                                    now.day(),
                                    MONTHS_RU_GEN[(now.month() - 1) as usize],
                                    now.year()
                                ))
                                .size(12.0)
                                .color(egui::Color32::from_rgb(180, 180, 185)),
                            );
                        });
                        ui.add_space(10.0);

                        // ── Заголовок месяца с листанием по краям ──
                        ui.horizontal(|ui| {
                            if ui.button("‹").clicked() {
                                self.step_clock_month(-1);
                            }
                            ui.with_layout(
                                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} {}",
                                            MONTHS_RU[(self.clock_month - 1) as usize],
                                            self.clock_year
                                        ))
                                        .strong()
                                        .size(13.0),
                                    );
                                },
                            );
                            if ui.button("›").clicked() {
                                self.step_clock_month(1);
                            }
                        });
                        ui.add_space(6.0);

                        self.draw_month_grid(ui, &now);
                    });
            });

        if menu.response.clicked_elsewhere() && !clock_clicked {
            close = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !clock_clicked {
            close = true;
        }
        if close {
            self.clock_open = false;
        }
    }

    /// Листание календаря на месяц; перенос года через январь/декабрь.
    fn step_clock_month(&mut self, delta: i32) {
        let mut m = self.clock_month as i32 + delta;
        let mut y = self.clock_year;
        if m < 1 {
            m = 12;
            y -= 1;
        } else if m > 12 {
            m = 1;
            y += 1;
        }
        self.clock_month = m as u32;
        self.clock_year = y;
    }

    /// Сетка месяца: строка названий дней недели (пн = 1, русская неделя) и
    /// числа. Сегодняшнее — акцентной «таблеткой», суббота/воскресенье — красные.
    /// Чистая графика: дни по клику ничего не делают, событий в системе нет.
    fn draw_month_grid(&self, ui: &mut egui::Ui, today: &chrono::DateTime<Local>) {
        const WEEKDAYS: [&str; 7] = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"];

        let (y, m) = (self.clock_year, self.clock_month);
        let days: u32 = match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            // Високосное правило считаем сами: короче и нагляднее, чем две
            // даты через вычитание, и совпадает с NaiveDate по определению.
            2 => {
                if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                    29
                } else {
                    28
                }
            }
            _ => 30,
        };
        // Сколько пустых клеток перед первым числом: пн = 0 … вс = 6.
        let lead: u32 = NaiveDate::from_ymd_opt(y, m, 1)
            .map(|d| d.weekday().number_from_monday() - 1)
            .unwrap_or(0);

        let avail_w = ui.available_width();
        let cell_w = avail_w / 7.0;
        let cell_h = 26.0;
        // Один блок painter'ом, а не 42 отдельных виджета: дни и так
        // некликабельны, а хитрый hit-test им только мешал бы.
        let (_, rect) = ui.allocate_space(egui::vec2(avail_w, cell_h * 7.0));
        let p = ui.painter();

        for (i, wd) in WEEKDAYS.iter().enumerate() {
            let r = egui::Rect::from_min_size(
                rect.min + egui::vec2(i as f32 * cell_w, 0.0),
                egui::vec2(cell_w, cell_h),
            );
            let c = if i >= 5 {
                egui::Color32::from_rgb(255, 120, 120)
            } else {
                egui::Color32::from_rgb(150, 150, 155)
            };
            p.text(r.center(), egui::Align2::CENTER_CENTER, *wd, egui::FontId::proportional(11.0), c);
        }

        for d in 1..=days {
            let idx = lead + d - 1;
            let (col, row) = (idx % 7, idx / 7);
            let cell = egui::Rect::from_min_size(
                rect.min + egui::vec2(col as f32 * cell_w, (row + 1) as f32 * cell_h),
                egui::vec2(cell_w, cell_h),
            );
            let is_today = today.year() == y && today.month() == m && today.day() == d;
            if is_today {
                p.rect_filled(
                    egui::Rect::from_center_size(cell.center(), egui::vec2(cell_w - 6.0, cell_h - 6.0)),
                    6.0,
                    egui::Color32::from_rgb(0, 120, 212),
                );
            }
            let c = if is_today {
                egui::Color32::WHITE
            } else if col >= 5 {
                egui::Color32::from_rgb(255, 130, 130)
            } else {
                egui::Color32::from_rgb(225, 225, 230)
            };
            p.text(cell.center(), egui::Align2::CENTER_CENTER, d.to_string(), egui::FontId::proportional(12.0), c);
        }
    }
}

/// Плитка приложения в меню «Пуск»: иконка сверху, имя под ней и подсветка при
/// наведении — так раскладаны плитки Windows 11. Возвращает true по клику.
///
/// Ширина приходит снаружи: сетка считает её от доступного места (4 колонки на
/// 640 px), а не берёт из константы — иначе плитки не дотянули бы до края.
fn start_tile(
    ui: &mut egui::Ui,
    glyph: &str,
    label: &str,
    id: egui::Id,
    w: f32,
    h: f32,
) -> bool {
    // allocate_space возвращает (Id, Rect) — прямоугольник берём у `.1`.
    let rect = ui.allocate_space(egui::vec2(w, h)).1;
    let resp = ui.interact(rect, id, egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, hover_bg());
    }
    ui.painter().text(
        rect.center() - egui::vec2(0.0, 9.0),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(26.0),
        egui::Color32::WHITE,
    );
    ui.painter().text(
        rect.center() + egui::vec2(0.0, 21.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        egui::Color32::from_rgb(220, 220, 225),
    );
    resp.clicked()
}

/// Строка приложения в виде «Все приложения»: иконка и имя в одну линию,
/// подсветка при наведении. Возвращает true по клику — намерение собирает
/// вызывающий, потому что терминал открывается особым образом ([`open_intent`]).
fn start_app_row(ui: &mut egui::Ui, wid: WindowId, h: f32) -> bool {
    start_named_row(ui, wid.glyph(), wid.label(), h)
}

/// То же, но для установленного приложения: глиф и имя приходят из манифеста
/// (`apps/`), а не из [`WindowId`].
fn start_named_row(ui: &mut egui::Ui, glyph: &str, label: &str, h: f32) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, hover_bg());
    }
    ui.painter().text(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        glyph,
        egui::FontId::proportional(15.0),
        egui::Color32::WHITE,
    );
    ui.painter().text(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(220, 220, 225),
    );
    resp.clicked()
}

/// Что делает клик по приложению в «Пуске»: терминал — сразу с фокусом в строке
/// ввода (он для этого и существует), остальные окна — просто открываются.
fn open_intent(wid: WindowId) -> DesktopIntent {
    match wid {
        WindowId::Terminal => DesktopIntent::FocusInput,
        other => DesktopIntent::OpenWindow(other),
    }
}

/// Иконки рабочего стола: все приложения (окна) плюс терминал с фокусом.
fn default_icons() -> Vec<DesktopIcon> {
    vec![
        DesktopIcon {
            glyph: WindowId::Terminal.glyph(),
            label: WindowId::Terminal.label(),
            hint: "Локальный ИИ-терминал (Enter)",
            action: IconAction::FocusInput,
        },
        DesktopIcon {
            glyph: WindowId::Monitor.glyph(),
            label: WindowId::Monitor.label(),
            hint: "Графики CPU/RAM/Disk и статус ядра",
            action: IconAction::OpenWindow(WindowId::Monitor),
        },
        DesktopIcon {
            glyph: WindowId::Devices.glyph(),
            label: WindowId::Devices.label(),
            hint: "Диспетчер устройств: реальная аппаратура и драйверы",
            action: IconAction::OpenWindow(WindowId::Devices),
        },
        DesktopIcon {
            glyph: WindowId::Notes.glyph(),
            label: WindowId::Notes.label(),
            hint: "Блокнот с автосохранением",
            action: IconAction::OpenWindow(WindowId::Notes),
        },
        DesktopIcon {
            glyph: WindowId::Matrix.glyph(),
            label: WindowId::Matrix.label(),
            hint: "Поток Матрицы",
            action: IconAction::OpenWindow(WindowId::Matrix),
        },
        DesktopIcon {
            glyph: WindowId::Cleaner.glyph(),
            label: WindowId::Cleaner.label(),
            hint: "Очистка логов и кэша",
            action: IconAction::OpenWindow(WindowId::Cleaner),
        },
        DesktopIcon {
            glyph: WindowId::Apps.glyph(),
            label: WindowId::Apps.label(),
            hint: "Реестр сгенерированных приложений (/apps)",
            action: IconAction::OpenWindow(WindowId::Apps),
        },
        DesktopIcon {
            glyph: WindowId::Scripts.glyph(),
            label: WindowId::Scripts.label(),
            hint: "Проводник скриптов",
            action: IconAction::OpenWindow(WindowId::Scripts),
        },
        DesktopIcon {
            glyph: WindowId::Explorer.glyph(),
            label: WindowId::Explorer.label(),
            hint: "Файловый проводник по всем дискам",
            action: IconAction::OpenWindow(WindowId::Explorer),
        },
        DesktopIcon {
            // ◎ (U+25CE) — геометрический блок, весь в Segoe UI Symbol.
            glyph: "◎",
            label: "Браузер",
            hint: "Системный браузер на localhost:8787 (стадия 1)",
            action: IconAction::OpenBrowser,
        },
        DesktopIcon {
            glyph: WindowId::Trainer.glyph(),
            label: WindowId::Trainer.label(),
            hint: "QLoRA-самообучение модели",
            action: IconAction::OpenWindow(WindowId::Trainer),
        },
        DesktopIcon {
            glyph: WindowId::Settings.glyph(),
            label: WindowId::Settings.label(),
            hint: "Бэкенд ИИ и автозапуск Ollama",
            action: IconAction::OpenWindow(WindowId::Settings),
        },
    ]
}
