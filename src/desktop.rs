//! Рабочий стол Vanguard OS в стиле Windows 11: обои с киберпанк-подложкой,
//! иконки с двойным кликом, центрированная панель задач и меню «Пуск».
//!
//! Стол не владеет ни окнами, ни шеллом, ни чатом — он получает доступ к ним
//! через [`DesktopHost`] (набор заимствований на один кадр) и возвращает
//! [`DesktopIntent`] — список того, что приложение должно выполнить
//! (`VanguardOSApp::apply_desktop_intent`). Благодаря этому раскладка не тянет
//! за собой `&mut self` и не дерётся с borrow-checker'ом.

use chrono::Local;
use eframe::egui;

use crate::ui_window::{VanguardWindows, WindowId, TASKBAR_H};

/// Идентификатор строки ввода в памяти egui. Общая константа: стол запрашивает
/// фокус для неё по хоткею Enter и по иконке, а приложение — при старте.
pub const INPUT_FIELD_ID: &str = "ai_input_field";

/// Цвет подсветки наведения на тёмном хроме (белый с малой альфой).
fn hover_bg() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 22)
}

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
}

/// Всё, что столу нужно от приложения. Живёт ровно один кадр, собирается
/// в `update()` из полей `VanguardOSApp`.
pub struct DesktopHost<'a> {
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
    /// Выбранная (одиночный клик) иконка; двойной клик по ней открывает приложение.
    pub selected_icon: Option<usize>,
}

impl Default for Desktop {
    fn default() -> Self {
        Self::new()
    }
}

impl Desktop {
    pub fn new() -> Self {
        Self {
            icons: default_icons(),
            menu_pos: None,
            start_open: false,
            start_query: String::new(),
            selected_icon: None,
        }
    }

    /// Главная точка входа стола. Возвращает намерения, которые приложение
    /// должно исполнить.
    pub fn draw(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, host: &mut DesktopHost) -> Vec<DesktopIntent> {
        let mut intents = Vec::new();
        self.draw_wallpaper(ui);
        self.draw_icons(ui, &mut intents);
        self.draw_context_menu(ctx, &mut intents);
        let start_clicked = self.draw_taskbar(ui, host, &mut intents);
        self.draw_start_menu(ctx, host, &mut intents, start_clicked);
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
    /// самого края. Возвращает true, если в этом кадре кликнули «Пуск» —
    /// меню использует это, чтобы не закрыться от собственного открытия.
    fn draw_taskbar(&mut self, ui: &egui::Ui, host: &DesktopHost, intents: &mut Vec<DesktopIntent>) -> bool {
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
            start_clicked = true;
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
        clock_resp.on_hover_text(now.format("%A, %d %B %Y").to_string());
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

        start_clicked
    }

    /// Меню «Пуск» по центру над панелью задач: поиск, закреплённые приложения
    /// и последние сгенерированные скрипты («Рекомендуется»).
    fn draw_start_menu(&mut self, ctx: &egui::Context, host: &DesktopHost, intents: &mut Vec<DesktopIntent>, start_clicked: bool) {
        if !self.start_open {
            return;
        }
        const MENU_W: f32 = 560.0;
        const MENU_H: f32 = 470.0;
        let x = ((host.win_w - MENU_W) / 2.0).max(8.0);
        let y = (host.win_h - TASKBAR_H - MENU_H - 10.0).max(8.0);

        let mut close = false;
        let menu = egui::Area::new(egui::Id::new("start_menu"))
            .fixed_pos(egui::pos2(x, y))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(egui::Color32::from_rgb(43, 43, 49))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(70, 70, 78)))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.set_min_width(MENU_W - 32.0);
                        ui.set_max_width(MENU_W - 32.0);

                        // Поиск (реальный: фильтрует закреплённые приложения).
                        let search = ui.add(
                            egui::TextEdit::singleline(&mut self.start_query)
                                .hint_text("Поиск приложений")
                                .desired_width(f32::INFINITY),
                        );
                        if search.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                            // Enter по первому найденному — открыть его.
                            if let Some(icon) = self.icons.iter().find(|ic| {
                                ic.label.to_lowercase().contains(&self.start_query.to_lowercase())
                            }) {
                                match icon.action {
                                    IconAction::FocusInput => intents.push(DesktopIntent::FocusInput),
                                    IconAction::OpenWindow(wid) => intents.push(DesktopIntent::OpenWindow(wid)),
                                }
                                close = true;
                            }
                        }
                        ui.add_space(12.0);

                        // ── Закреплено: сетка 4 колонки ──
                        ui.label(egui::RichText::new("Закреплено").size(13.0).strong());
                        ui.add_space(4.0);
                        let q = self.start_query.to_lowercase();
                        let pinned: Vec<WindowId> = WindowId::ALL
                            .iter()
                            .copied()
                            .filter(|wid| q.is_empty() || wid.label().to_lowercase().contains(&q))
                            .collect();
                        for chunk in pinned.chunks(4) {
                            ui.horizontal(|ui| {
                                for wid in chunk {
                                    if start_tile(ui, wid.glyph(), wid.label(), ui.make_persistent_id(("start_tile", wid))) {
                                        match wid {
                                            // Терминал из «Пуска» — сразу с фокусом в строке ввода.
                                            WindowId::Terminal => intents.push(DesktopIntent::FocusInput),
                                            other => intents.push(DesktopIntent::OpenWindow(*other)),
                                        }
                                        close = true;
                                    }
                                }
                            });
                        }
                        ui.add_space(10.0);

                        // ── Рекомендуется: свежие сгенерированные скрипты ──
                        ui.label(egui::RichText::new("Рекомендуется").size(13.0).strong());
                        ui.add_space(4.0);
                        if host.recent_scripts.is_empty() {
                            ui.label(egui::RichText::new("Пока пусто: попросите ИИ написать скрипт, и он появится здесь.")
                                .size(11.0)
                                .color(egui::Color32::from_rgb(150, 150, 155)));
                        }
                        for (id, name) in host.recent_scripts.iter().take(4) {
                            // Плоская кнопка на всю ширину строки: клик по строке
                            // запускает скрипт, как в «Рекомендуется» Windows 11.
                            let label = format!("▸  {}", name);
                            let btn = ui.add_sized(
                                [ui.available_width(), 24.0],
                                egui::Button::new(egui::RichText::new(label).size(12.0)).frame(false),
                            );
                            if btn.clicked() {
                                intents.push(DesktopIntent::RunCommand(format!("/run {}", id)));
                                close = true;
                            }
                        }

                        ui.add_space(12.0);
                        ui.separator();
                        // ── Нижняя строка: пользователь слева, питание справа ──
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("●  DamirSeversk").size(12.0));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("⏻  Выключение").clicked() {
                                    intents.push(DesktopIntent::RunCommand("/exit".into()));
                                    close = true;
                                }
                            });
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
        }
    }
}

/// Плитка приложения в меню «Пуск»: глиф + имя, подсветка при наведении.
/// Возвращает true по клику.
fn start_tile(ui: &mut egui::Ui, glyph: &str, label: &str, id: egui::Id) -> bool {
    // allocate_space возвращает (Id, Response) — прямоугольник берём у Response.
    let rect = ui.allocate_space(egui::vec2(122.0, 70.0)).1;
    let resp = ui.interact(rect, id, egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, hover_bg());
    }
    ui.painter().text(
        rect.center() - egui::vec2(0.0, 10.0),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(22.0),
        egui::Color32::WHITE,
    );
    ui.painter().text(
        rect.center() + egui::vec2(0.0, 22.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        egui::Color32::from_rgb(220, 220, 225),
    );
    resp.clicked()
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
            glyph: WindowId::Trainer.glyph(),
            label: WindowId::Trainer.label(),
            hint: "QLoRA-самообучение модели",
            action: IconAction::OpenWindow(WindowId::Trainer),
        },
    ]
}
