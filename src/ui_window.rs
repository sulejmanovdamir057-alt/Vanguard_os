use eframe::egui;
use rand::Rng;

// =========================================================================
// 1. СТРУКТУРА ОКНА — визуальный хром Windows 11 (тёмная тема)
// =========================================================================

/// Высота панели задач. Общая константа: меню «Пуск» считает от неё свою
/// позицию, а окно в развёрнутом виде не заезжает на неё.
pub const TASKBAR_H: f32 = 48.0;

/// Окно приложения в стиле Windows 11: собственная заголовочная полоса с
/// кнопками «свернуть / развернуть / закрыть», перетаскивание за заголовок,
/// двойной клик по заголовку = развёртывание. Свёрнутое окно не рисуется,
/// но остаётся в панели задач и снова открывается кликом по его кнопке.
pub struct VanguardWindow {
    pub title: String,
    pub is_open: bool,
    pub minimized: bool,
    /// Развёрнутое окно занимает экран целиком (кроме панели задач) —
    /// как maximized в настоящей Windows.
    pub maximized: bool,
    pub default_pos: egui::Pos2,
    pub default_size: egui::Vec2,
    /// Смещение «хватки» заголовка при перетаскивании (указатель минус левый
    /// верхний угол окна). `Some` = окно тащат прямо сейчас. В egui 0.28 нет
    /// публичного способа сдвинуть Area из пользовательского кода, поэтому
    /// позиция поддерживается через `fixed_pos` от указателя.
    drag_grab: Option<egui::Vec2>,
}

// =========================================================================
// 2. РЕАЛИЗАЦИЯ: конструктор и отрисовка оконного хрома
// =========================================================================

impl VanguardWindow {
    /// Конструктор: создаёт окно со случайными хаотичными координатами появления
    /// (в духе «окна сами разбросаны по столу»).
    pub fn new(title: &str, width: f32, height: f32) -> Self {
        let mut rng = rand::thread_rng();
        Self {
            title: title.to_string(),
            is_open: false,
            minimized: false,
            maximized: false,
            default_pos: egui::pos2(rng.gen_range(60.0..260.0), rng.gen_range(40.0..160.0)),
            default_size: egui::vec2(width, height),
            drag_grab: None,
        }
    }

    /// Главный метод отрисовки: хром окна + содержимое через замыкание.
    pub fn show<F>(&mut self, ctx: &egui::Context, add_contents: F)
    where
        F: FnOnce(&mut egui::Ui),
    {
        if !self.is_open || self.minimized {
            return;
        }
        // Стабильный id Area: он нужен, чтобы перетаскивание могло двигать
        // позицию окна через память egui.
        let area_id = egui::Id::new(("vwin", self.title.clone()));

        // Позиция появления считается от реального экрана: `new()` задаёт
        // случайную точку в опорных 1024×768, а окно может быть развёрнутым
        // или уведённым в полный экран (F11) — тогда константа увала бы в угол.
        // egui читает default_pos только при первом показе, дальше Area помнит
        // свою позицию, так что масштаб не дёргает уже открытые окна. Считаем
        // здесь, а не в конструкторе: там экрана ещё не существует.
        let screen = ctx.screen_rect();
        let default_pos = egui::pos2(
            (self.default_pos.x * screen.width() / crate::app::BASE_W)
                .clamp(8.0, (screen.width() - self.default_size.x - 8.0).max(8.0)),
            (self.default_pos.y * screen.height() / crate::app::BASE_H)
                .clamp(8.0, (screen.height() - TASKBAR_H - self.default_size.y - 8.0).max(8.0)),
        );

        let mut window = egui::Window::new("")
            .id(area_id)
            .title_bar(false) // весь хром рисуем сами
            .resizable(true)
            .collapsible(false)
            .default_pos(default_pos)
            .default_size(self.default_size)
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(32, 32, 32))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(58, 58, 58)))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(0.0),
            );
        if self.maximized {
            // Maximised в Windows: на весь экран, но панель задач видна.
            let screen = ctx.screen_rect();
            window = window
                .fixed_pos(egui::pos2(screen.min.x, screen.min.y))
                .fixed_size(egui::vec2(screen.width(), screen.height() - TASKBAR_H));
        } else if let Some(grab) = self.drag_grab {
            // Окно тащат: позиция каждый кадр следует за указателем. Смещение
            // «хватки» заморожено на момент схватывания, поэтому окно не прыгает.
            if let Some(pointer) = ctx.input(|i| i.pointer.latest_pos()) {
                window = window.fixed_pos(pointer - grab);
            }
        }

        window.show(ctx, |ui| {
            self.draw_title_bar(ui, ctx, area_id);
            egui::Frame::none().inner_margin(10.0).show(ui, |ui| {
                add_contents(ui);
            });
        });
    }

    /// Заголовочная полоса: перетаскивание, двойной клик = развернуть и три
    /// win-кнопки. Кнопки рисуются штрихами (как в Windows), а не глифами —
    /// так гарантированно одинаково на любой машине.
    fn draw_title_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, area_id: egui::Id) {
        const BAR_H: f32 = 36.0;
        const BTN_W: f32 = 46.0;

        let bar_rect = ui.allocate_space(egui::vec2(ui.available_width(), BAR_H)).1;
        let painter = ui.painter().with_clip_rect(bar_rect);

        // Заголовок слева: глиф-иконка приложения + название.
        painter.text(
            bar_rect.min + egui::vec2(12.0, BAR_H / 2.0),
            egui::Align2::LEFT_CENTER,
            self.title.clone(),
            egui::FontId::proportional(12.0),
            egui::Color32::WHITE,
        );

        // Три кнопки справа: свернуть / развернуть / закрыть.
        let bx = bar_rect.max.x - BTN_W * 3.0;
        let defs = [("min", false), ("max", false), ("close", false)];
        for (i, (kind, _)) in defs.iter().enumerate() {
            let rect = egui::Rect::from_min_size(
                egui::pos2(bx + i as f32 * BTN_W, bar_rect.min.y),
                egui::vec2(BTN_W, BAR_H),
            );
            let resp = ui.interact(rect, egui::Id::new(("wbtn", kind, &self.title)), egui::Sense::click());
            let hovered = resp.hovered();
            let (bg, fg) = if *kind == "close" && hovered {
                (egui::Color32::from_rgb(196, 43, 28), egui::Color32::WHITE)
            } else if hovered {
                (egui::Color32::from_rgba_unmultiplied(255, 255, 255, 26), egui::Color32::WHITE)
            } else {
                (egui::Color32::TRANSPARENT, egui::Color32::from_rgb(200, 200, 200))
            };
            if bg != egui::Color32::TRANSPARENT {
                painter.rect_filled(rect, 0.0, bg);
            }
            let c = rect.center();
            let stroke = egui::Stroke::new(1.0_f32, fg);
            match *kind {
                "min" => {
                    painter.line_segment(
                        [egui::pos2(c.x - 5.0, c.y + 1.0), egui::pos2(c.x + 5.0, c.y + 1.0)],
                        stroke,
                    );
                }
                "max" => {
                    let r = egui::Rect::from_center_size(c, egui::vec2(10.0, 10.0));
                    painter.rect_stroke(r, 0.0, stroke);
                    if self.maximized {
                        // Глиф «восстановить»: второй квадратик со сдвигом.
                        let r2 = egui::Rect::from_min_size(
                            r.min + egui::vec2(3.0, -3.0),
                            egui::vec2(7.0, 7.0),
                        );
                        painter.rect_filled(r2, 0.0, egui::Color32::from_rgb(32, 32, 32));
                        painter.rect_stroke(r2, 0.0, stroke);
                    }
                }
                _ => {
                    painter.line_segment(
                        [egui::pos2(c.x - 5.0, c.y - 5.0), egui::pos2(c.x + 5.0, c.y + 5.0)],
                        stroke,
                    );
                    painter.line_segment(
                        [egui::pos2(c.x + 5.0, c.y - 5.0), egui::pos2(c.x - 5.0, c.y + 5.0)],
                        stroke,
                    );
                }
            }
            if resp.clicked() {
                match *kind {
                    "min" => self.minimized = true,
                    "max" => self.maximized = !self.maximized,
                    _ => self.is_open = false,
                }
            }
        }

        // Перетаскивание за заголовок (кроме зоны кнопок) + двойной клик = max.
        let drag_rect =
            egui::Rect::from_min_max(bar_rect.min, egui::pos2(bx - 2.0, bar_rect.max.y));
        let drag = ui.interact(drag_rect, egui::Id::new(("wbar", &self.title)), egui::Sense::click_and_drag());
        if !self.maximized {
            if drag.drag_started() {
                // Запоминаем, за какую точку схватили: указатель минус позиция
                // окна в памяти egui (читаем через публичный area_rect).
                let pointer = ui.input(|i| i.pointer.latest_pos()).unwrap_or(bar_rect.min);
                let win_pos = ctx
                    .memory(|m| m.area_rect(area_id))
                    .map(|r| r.min)
                    .unwrap_or(bar_rect.min);
                self.drag_grab = Some(pointer - win_pos);
            }
            if drag.drag_stopped() {
                self.drag_grab = None;
            }
            if drag.double_clicked() {
                self.maximized = !self.maximized;
                self.drag_grab = None;
            }
        }
    }
}

// =========================================================================
// 3. СПИСОК ОКОН (панель задач, меню «Пуск» и иконки стола перебирают их)
// =========================================================================

/// Идентификатор окна. Ошибка в порядковых индексах теперь становится ошибкой
/// компиляции, а список [`WindowId::ALL`] — единственным источником правды
/// для панели задач, меню «Пуск» и сетки иконок стола.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowId {
    Terminal,
    Monitor,
    Notes,
    Matrix,
    Cleaner,
    Apps,
    Scripts,
    Explorer,
    Trainer,
    Settings,
}

impl WindowId {
    /// Порядок здесь задаёт порядок кнопок на панели задач и в меню «Пуск».
    pub const ALL: [WindowId; 10] = [
        WindowId::Terminal,
        WindowId::Monitor,
        WindowId::Notes,
        WindowId::Matrix,
        WindowId::Cleaner,
        WindowId::Apps,
        WindowId::Scripts,
        WindowId::Explorer,
        WindowId::Trainer,
        WindowId::Settings,
    ];

    /// Глиф-иконка для панели задач / «Пуска» / стола (единый источник).
    /// Только геометрические символы: эмодзи вроде 🧠/🟢/🚀 в шрифтах egui
    /// превращаются в пустые квадраты, а знаки из Segoe UI отрисовываются
    /// везде (в app.rs подгружаются Segoe UI и Segoe UI Symbol).
    pub fn glyph(self) -> &'static str {
        match self {
            WindowId::Terminal => "❯",
            WindowId::Monitor => "▦",
            WindowId::Notes => "✎",
            WindowId::Matrix => "◉",
            WindowId::Cleaner => "⌫",
            WindowId::Apps => "▶",
            WindowId::Scripts => "≡",
            // «▦» занята Монитором; ▣ (U+25A3) — «заполненный файл», сосед
            // ▦ по таблице, гарантированно есть в Segoe UI Symbol.
            WindowId::Explorer => "▣",
            WindowId::Trainer => "⚙",
            // «⚙» уже занята Тренером; ◈ (U+25C8) — сосед ◉, гарантированно
            // есть в Segoe UI Symbol, который подгружается в app.rs.
            WindowId::Settings => "◈",
        }
    }

    /// Короткое имя для панели задач / «Пуска» / стола.
    pub fn label(self) -> &'static str {
        match self {
            WindowId::Terminal => "Терминал",
            WindowId::Monitor => "Монитор",
            WindowId::Notes => "Заметки",
            WindowId::Matrix => "Матрица",
            WindowId::Cleaner => "Очистка",
            WindowId::Apps => "Лаунчер",
            WindowId::Scripts => "Скрипты",
            WindowId::Explorer => "Файлы",
            WindowId::Trainer => "Тренер",
            WindowId::Settings => "Настройки",
        }
    }
}

/// Все окна приложения одной группой. Панель задач перебирает их в цикле,
/// а slice из восьми отдельных полей структуры собрать нельзя — поэтому они здесь.
pub struct VanguardWindows {
    pub terminal: VanguardWindow,
    pub monitor: VanguardWindow,
    pub notes: VanguardWindow,
    pub matrix: VanguardWindow,
    pub cleaner: VanguardWindow,
    pub apps: VanguardWindow,
    pub scripts: VanguardWindow,
    pub explorer: VanguardWindow,
    pub trainer: VanguardWindow,
    pub settings: VanguardWindow,
}

impl VanguardWindows {
    pub fn new() -> Self {
        Self {
            terminal: VanguardWindow::new("Vanguard Terminal", 600.0, 440.0),
            monitor: VanguardWindow::new("System Monitor", 480.0, 380.0),
            notes: VanguardWindow::new("Vanguard Notes", 400.0, 350.0),
            matrix: VanguardWindow::new("SysMatrix Stream", 300.0, 400.0),
            cleaner: VanguardWindow::new("Core Cleaner v1.1", 400.0, 250.0),
            apps: VanguardWindow::new("Vanguard App Registry", 450.0, 300.0),
            scripts: VanguardWindow::new("Script Explorer", 620.0, 420.0),
            // Проводник шире «Скриптов»: у него ещё и строка пути, и панель
            // действий, и редактор справа.
            explorer: VanguardWindow::new("Vanguard Explorer", 760.0, 470.0),
            trainer: VanguardWindow::new("Neural Trainer v0.1", 420.0, 340.0),
            settings: VanguardWindow::new("Vanguard Settings", 520.0, 430.0),
        }
    }

    /// Разворачивает и открывает окно — то, что делает клик по иконке
    /// или пункт контекстного меню.
    pub fn open(&mut self, id: WindowId) {
        let w = self.get_mut(id);
        w.is_open = true;
        w.minimized = false;
    }

    /// Доступ к окну по идентификатору.
    pub fn get_mut(&mut self, id: WindowId) -> &mut VanguardWindow {
        match id {
            WindowId::Terminal => &mut self.terminal,
            WindowId::Monitor => &mut self.monitor,
            WindowId::Notes => &mut self.notes,
            WindowId::Matrix => &mut self.matrix,
            WindowId::Cleaner => &mut self.cleaner,
            WindowId::Apps => &mut self.apps,
            WindowId::Scripts => &mut self.scripts,
            WindowId::Explorer => &mut self.explorer,
            WindowId::Trainer => &mut self.trainer,
            WindowId::Settings => &mut self.settings,
        }
    }

    /// Чтение состояния окна (заголовок, открыто ли, свёрнуто ли) без заимствования
    /// на запись — нужно панели задач, чтобы нарисовать кнопки.
    pub fn get(&self, id: WindowId) -> &VanguardWindow {
        match id {
            WindowId::Terminal => &self.terminal,
            WindowId::Monitor => &self.monitor,
            WindowId::Notes => &self.notes,
            WindowId::Matrix => &self.matrix,
            WindowId::Cleaner => &self.cleaner,
            WindowId::Apps => &self.apps,
            WindowId::Scripts => &self.scripts,
            WindowId::Explorer => &self.explorer,
            WindowId::Trainer => &self.trainer,
            WindowId::Settings => &self.settings,
        }
    }
}

impl Default for VanguardWindows {
    fn default() -> Self {
        Self::new()
    }
}
