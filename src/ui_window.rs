use eframe::egui;
use rand::Rng;

// =========================================================================
// 1. СТРУКТУРА ОПИСАНИЯ ОКНА (Аналог Toplevel-контейнера из Python/Tkinter)
// =========================================================================
pub struct VanguardWindow {
    pub title: String,
    pub is_open: bool,
    /// Свёрнутое окно не рисуется, но остаётся в панели задач и снова открывается кликом
    /// по его кнопке. egui не умеет сворачивание сам, поэтому флаг живёт здесь.
    pub minimized: bool,
    default_pos: egui::Pos2,
    default_size: egui::Vec2,
}

// =========================================================================
// 2. РЕАЛИЗАЦИЯ ПОВЕДЕНИЯ ОКНА (Конструктор и метод вывода на экран)
// =========================================================================
impl VanguardWindow {
    /// Конструктор: создает новое окно со случайными хаотичными координатами появления
    pub fn new(title: &str, width: f32, height: f32) -> Self {
        let mut rng = rand::thread_rng();
        Self {
            title: title.to_string(),
            is_open: false,
            minimized: false,
            // Генерируем хакерский разброс окон по экрану при их первом открытии
            default_pos: egui::pos2(rng.gen_range(100.0..300.0), rng.gen_range(150.0..300.0)),
            default_size: egui::vec2(width, height),
        }
    }

    /// Главный метод отрисовки: принимает контекст egui и замыкание (callback) с контентом
    pub fn show<F>(&mut self, ctx: &egui::Context, add_contents: F)
    where
        F: FnOnce(&mut egui::Ui),
    {
        // Если флаг видимости окна равен false или окно свёрнуто — мгновенно прекращаем
        // выполнение, экономя ресурсы процессора и видеокарты (Zero-cost abstraction)
        if !self.is_open || self.minimized {
            return;
        }

        // Настраиваем неоновую ядовито-красную рамку для окна Vanguard OS
        let window_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(255, 0, 0));

        // Конструируем нативный контейнер окна egui
        egui::Window::new(&self.title)
            .open(&mut self.is_open) // Привязываем крестик [X] к флагу в нашей структуре
            .default_pos(self.default_pos)
            .default_size(self.default_size)
            .resizable(true)
            .collapsible(false)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(egui::Color32::from_rgb(5, 5, 5)) // Глубокий черный фон
                    .stroke(window_stroke)                  // Применяем рамку
                    .inner_margin(10.0),                    // Внутренние отступы (padding)
            )
            .show(ctx, |ui| {
                // Выполняем переданный код утилиты (Блокнот, Матрица или Очиститель)
                add_contents(ui);
            });
    }
}

// =========================================================================
// 3. СПИСОК ОКОН (панель задач и иконки перебирают их в цикле)
// =========================================================================

/// Идентификатор окна. Заменяет собой прежний `usize`-индекс из
/// `VanguardOSApp::window_ref`: индекс легко перепутать с порядком кнопок,
/// а здесь ошибка становится ошибкой компиляции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowId {
    Notes,
    Matrix,
    Cleaner,
    Apps,
    Scripts,
}

impl WindowId {
    /// Порядок здесь задаёт порядок кнопок на панели задач.
    /// Добавляете окно — дописываете вариант сюда, и оно сразу появится в панели.
    pub const ALL: [WindowId; 5] = [
        WindowId::Notes,
        WindowId::Matrix,
        WindowId::Cleaner,
        WindowId::Apps,
        WindowId::Scripts,
    ];
}

/// Все окна приложения одной группой. Панель задач должна перебирать их в цикле,
/// а slice из пяти отдельных полей структуры собрать нельзя — поэтому они здесь.
pub struct VanguardWindows {
    pub notes: VanguardWindow,
    pub matrix: VanguardWindow,
    pub cleaner: VanguardWindow,
    pub apps: VanguardWindow,
    pub scripts: VanguardWindow,
}

impl VanguardWindows {
    pub fn new() -> Self {
        Self {
            notes: VanguardWindow::new("Vanguard Notes", 400.0, 350.0),
            matrix: VanguardWindow::new("SysMatrix Stream", 300.0, 400.0),
            cleaner: VanguardWindow::new("Core Cleaner v1.1", 400.0, 250.0),
            apps: VanguardWindow::new("Vanguard App Registry", 450.0, 300.0),
            scripts: VanguardWindow::new("Script Explorer", 620.0, 420.0),
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
            WindowId::Notes => &mut self.notes,
            WindowId::Matrix => &mut self.matrix,
            WindowId::Cleaner => &mut self.cleaner,
            WindowId::Apps => &mut self.apps,
            WindowId::Scripts => &mut self.scripts,
        }
    }

    /// Чтение состояния окна (заголовок, открыто ли, свёрнуто ли) без заимствования
    /// на запись — нужно панели задач, чтобы нарисовать кнопки.
    pub fn get(&self, id: WindowId) -> &VanguardWindow {
        match id {
            WindowId::Notes => &self.notes,
            WindowId::Matrix => &self.matrix,
            WindowId::Cleaner => &self.cleaner,
            WindowId::Apps => &self.apps,
            WindowId::Scripts => &self.scripts,
        }
    }
}

impl Default for VanguardWindows {
    fn default() -> Self {
        Self::new()
    }
}