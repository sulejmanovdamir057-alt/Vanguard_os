use eframe::egui;
use rand::Rng;

// =========================================================================
// 1. СТРУКТУРА ОПИСАНИЯ ОКНА (Аналог Toplevel-контейнера из Python/Tkinter)
// =========================================================================
pub struct VanguardWindow {
    pub title: String,
    pub is_open: bool,
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
        // Если флаг видимости окна равен false — мгновенно прекращаем выполнение,
        // экономя ресурсы процессора и видеокарты (Zero-cost abstraction)
        if !self.is_open {
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