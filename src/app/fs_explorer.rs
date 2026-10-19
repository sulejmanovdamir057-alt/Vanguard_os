//! Окно «Файлы»: список песочницы ядра, действия над именами и редактор.
//!
//! Дочерний модуль `app`, а не братский: [`VanguardOSApp::draw_fs_window`] висит на
//! `impl VanguardOSApp` и трогает приватные поля `self.fs` и `self.windows`, а
//! приватность в Rust видна модулю-предку и его потомкам. Из `main.rs` пришлось бы
//! расставлять `pub(crate)` на каждое поле. Такая же причина, по которой все
//! помощники `fs_*` — свободные функции: окно вызывает их внутри `show()`, где
//! `&mut self` целиком занят отрисовкой самого окна.
//!
//! Оболочка к диску не тянется: один [`fs_call`] уходит в ядро, а пути там
//! проверяет `sandbox_path` (`bin/core.rs`).

use super::*;

/// Высоты рядов задаются явно и не берутся из `ui.available_size_before_wrap()`.
///
/// `ui.horizontal()` — это `left_to_right(Align::Center)`, и при центрировании по
/// поперечной оси egui растягивает рамку ряда на всю высоту окна (layout.rs:611-615).
/// Из-за этого два служебных ряда сверху съедали всё место, ряду «список + редактор»
/// доставалось 18 px, и поле кода обрезалось до 46 px (замер: row size=740x18,
/// ScrollArea раздувался до 64 из min_scrolled_size). `Align::Min` с фиксированной
/// высотой такого не делает.
const ROW_H: f32 = 30.0;

/// Высота ряда «список каталога + редактор». Сюда же относится предупреждение про
/// `allocate_space(vec2(0.0, _))`: в `left_to_right` он кладёт нулевой прямоугольник
/// на 8 px правее края, и min_rect рос на 8 px каждый кадр, пока окно не упиралось
/// в экран. Высота берётся из этой константы в `fs_panes`.
const PANE_H: f32 = 300.0;

/// Состояние окна «Файлы».
///
/// Здесь лежит только последний ответ `FsList` и открытый в редакторе файл —
/// поэтому рисование не делает ни одного `std::fs`.
#[derive(Default)]
pub(super) struct FsExplorer {
    /// Текущий каталог относительно корня песочницы, слэши `/` ("" = корень).
    path: String,
    /// Абсолютный корень песочницы — из первого ответа, для строки пути.
    root: String,
    /// Последний список от ядра: порядок уже приведён к «каталоги первыми».
    entries: Vec<FsEntry>,
    /// Выбранная запись — имя внутри текущего каталога.
    sel: Option<String>,
    /// Открытый файл: его относительный путь и содержимое в редакторе.
    file: String,
    code: String,
    /// В редакторе есть правки, которых ещё нет на диске.
    dirty: bool,
    /// Строка ввода для «создать» / «переименовать» и статусная строка.
    input: String,
    status: String,
    /// Последняя операция закончилась ошибкой — статус красим в красный.
    failed: bool,
    /// Список уже запрашивался хотя бы раз: иначе при мёртвом ядре окно
    /// стучалось бы заново на каждой рамке и клало рендер на 3 с ожидания.
    loaded: bool,
}

// ─── Пути и разговор с ядром ───────────────────────────────────────────────

/// Склеивает каталог и имя элемента в путь протокола: слэши `/`, без ведущего.
/// Ядро приведёт его к абсолютному и проверит, что тот внутри песочницы.
fn fs_join(dir: &str, name: &str) -> String {
    let dir = dir.trim_matches('/');
    let name = name.trim_matches('/');
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir, name)
    }
}

/// Родительский каталог: `"src/core"` → `"src"`, `"src"` → `""`.
fn fs_parent(dir: &str) -> String {
    let dir = dir.trim_matches('/');
    match dir.rfind('/') {
        Some(i) => dir[..i].to_string(),
        None => String::new(),
    }
}

/// Один IPC-вызов к ядру и разбор JSON-ответа. Полезная нагрузка едет в тексте
/// `Ok` — так устроен весь протокол, — поэтому и ошибка, и битый ответ
/// сводятся к одной строке, которую показывает статусная строка окна.
fn fs_call<T: serde::de::DeserializeOwned>(req: Request) -> Result<T, String> {
    let json = serde_json::to_string(&req)
        .map_err(|e| format!("не удалось сериализовать запрос: {}", e))?;
    let reply = crate::ipc_client::call(&json);
    if !reply.ok {
        return Err(reply.text.trim().to_string());
    }
    serde_json::from_str::<T>(&reply.text).map_err(|e| format!("ядро ответило не тем: {}", e))
}

/// Запрос, после которого каталог должен перечитаться: успех — перечитываем,
/// провал — пишем в статусную строку. Раньше эта пара строк повторялась в каждой
/// из четырёх кнопок, и любое изменение ловилось в четырёх местах.
///
/// Возвращает `true` на успехе — вызывающему нужно снять выделение после
/// переименования или удаления.
fn fs_apply(fs: &mut FsExplorer, req: Request) -> bool {
    match fs_call::<String>(req) {
        Ok(_) => {
            fs_reload(fs);
            true
        }
        Err(e) => {
            fs.status = e;
            fs.failed = true;
            false
        }
    }
}

/// Общая валидация строки ввода: пустое имя — ошибка в статус и `None`,
/// иначе — готовое имя. Три кнопки делили этот же текст с разной подсказкой.
fn fs_named_input(fs: &mut FsExplorer, prompt: &str) -> Option<String> {
    let name = fs.input.trim();
    if name.is_empty() {
        fs.status = prompt.to_string();
        fs.failed = true;
        return None;
    }
    Some(name.to_string())
}

// ─── Действия над каталогом ────────────────────────────────────────────────

/// Перечитывает текущий каталог у ядра.
fn fs_reload(fs: &mut FsExplorer) {
    match fs_call::<FsListing>(Request::FsList { path: fs.path.clone() }) {
        Ok(list) => {
            fs.root = list.root;
            fs.entries = list.entries;
            fs.status = format!("{} объектов", fs.entries.len());
            fs.failed = false;
        }
        Err(e) => {
            fs.entries.clear();
            fs.status = e;
            fs.failed = true;
        }
    }
}

/// Уход из каталога или на другой файл. Если в редакторе несохранённые правки,
/// переход отклоняется — иначе текст потерялся бы молча.
fn fs_leave(fs: &mut FsExplorer) -> bool {
    if fs.dirty {
        fs.status = String::from("Сначала SAVE — иначе правки в редакторе потеряются.");
        fs.failed = true;
        return false;
    }
    fs.file.clear();
    fs.code.clear();
    true
}

/// Вверх на каталог.
fn fs_up(fs: &mut FsExplorer) {
    if !fs_leave(fs) {
        return;
    }
    fs.path = fs_parent(&fs.path);
    fs.sel = None;
    fs_reload(fs);
}

/// Войти в подкаталог.
fn fs_enter(fs: &mut FsExplorer, dir: &str) {
    if !fs_leave(fs) {
        return;
    }
    fs.path = fs_join(&fs.path, dir);
    fs.sel = None;
    fs_reload(fs);
}

/// Открыть файл в редакторе.
fn fs_open_file(fs: &mut FsExplorer, name: &str) {
    if !fs_leave(fs) {
        return;
    }
    let path = fs_join(&fs.path, name);
    match fs_call::<FsFile>(Request::FsRead { path }) {
        Ok(f) => {
            fs.file = f.path;
            fs.code = f.content;
            fs.dirty = false;
            fs.status = format!("Открыт: {}", fs.file);
            fs.failed = false;
        }
        Err(e) => {
            fs.status = e;
            fs.failed = true;
        }
    }
}

/// Записать открытый файл обратно в песочницу.
///
/// `Request::FsWrite` перезаписывает целиком — это его назначение: это путь
/// SAVE. Создание нового файла делает `fs_apply` с [`Request::FsCreate`].
fn fs_save(fs: &mut FsExplorer) {
    if fs.file.is_empty() {
        fs.status = String::from("Файл не открыт.");
        fs.failed = true;
        return;
    }
    let path = fs.file.clone();
    let content = fs.code.clone();
    match fs_call::<String>(Request::FsWrite { path: path.clone(), content }) {
        Ok(_) => {
            fs.dirty = false;
            fs.status = format!("Сохранено: {}", path);
            fs.failed = false;
        }
        Err(e) => {
            fs.status = e;
            fs.failed = true;
        }
    }
}

// ─── Четыре ряда окна ──────────────────────────────────────────────────────

/// Кнопка «создать»: имя берётся из строки ввода, запрос собирает `make`.
/// Делает одинаковыми «＋ Папка» и «＋ Файл» — они различались только подписью
/// и одним полем структуры.
fn fs_btn_create(
    ui: &mut egui::Ui,
    fs: &mut FsExplorer,
    label: &str,
    prompt: &str,
    make: fn(String) -> Request,
) {
    if ui.button(label).clicked() {
        if let Some(name) = fs_named_input(fs, prompt) {
            let path = fs_join(&fs.path, &name);
            fs_apply(fs, make(path));
        }
    }
}

/// Строка пути: корень песочницы + текущий относительный путь.
fn fs_row_path(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), ROW_H),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| {
            if ui.button("↑").on_hover_text("Вверх").clicked() {
                fs_up(fs);
            }
            let shown = if fs.path.is_empty() {
                fs.root.clone()
            } else {
                format!("{}\\{}", fs.root, fs.path.replace('/', "\\"))
            };
            ui.monospace(shown);
            if ui.button("⟳").on_hover_text("Обновить список").clicked() {
                fs_reload(fs);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(egui::Color32::from_rgb(130, 130, 135), "двойной клик — открыть");
            });
        },
    );
}

/// Строка действий: имя берётся из строки ввода, «кто» — из выделения.
fn fs_row_actions(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), ROW_H),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut fs.input)
                    .hint_text("имя файла или папки")
                    .desired_width(190.0),
            );
            fs_btn_create(ui, fs, "＋ Папка", "Введите имя папки.", |path| {
                Request::FsMkdir { path }
            });
            // FsCreate, а не FsWrite с пустым содержимым: тот обнулял бы файл с уже
            // занятым именем. Проверка занятости — на стороне ядра (`create_new`),
            // а не по нашему списку: тот может отставать от диска.
            fs_btn_create(ui, fs, "＋ Файл", "Введите имя файла.", |path| {
                Request::FsCreate { path }
            });
            ui.separator();
            // Клонируем имя сразу: ниже идёт `&mut fs`, а цикл по entries
            // держал бы на него заимствование.
            let sel_name = fs.sel.clone();
            let has_sel = sel_name.is_some();
            if ui
                .add_enabled(has_sel, egui::Button::new("→ Переименовать"))
                .clicked()
            {
                // По ссылке: имя нужно ещё ниже, в ветке «Удалить».
                if let Some(name) = &sel_name {
                    if let Some(new_name) = fs_named_input(fs, "Введите новое имя.") {
                        let from = fs_join(&fs.path, name);
                        let to = fs_join(&fs.path, &new_name);
                        if fs_apply(fs, Request::FsRename { from, to }) {
                            fs.sel = None;
                        }
                    }
                }
            }
            if ui
                .add_enabled(has_sel, egui::Button::new("✕ Удалить"))
                .clicked()
            {
                if let Some(name) = sel_name {
                    let path = fs_join(&fs.path, &name);
                    if fs_apply(fs, Request::FsDelete { path }) {
                        fs.sel = None;
                    }
                }
            }
        },
    );
}

/// Слева список каталога, справа редактор — обе панели на одной явной высоте.
///
/// Обе обёрнуты в `with_layout(top_down)`: дочерний ui наследует раскладку
/// родителя, а родитель — горизонтальный ряд, и имена леглись бы в одну строку,
/// тянув окно на 1430 px. `max_width` не спасает: он применяется до измерения
/// контента, а при `auto_shrink[0] = false` итоговая ширина = max(доступная, контент).
fn fs_panes(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), PANE_H),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| {
            fs_pane_list(ui, fs);
            ui.separator();
            fs_pane_editor(ui, fs);
        },
    );
}

/// Список записей текущего каталога. Двойной клик откладывается до конца цикла:
/// `fs_enter`/`fs_open_file` берут `&mut FsExplorer` целиком, а цикл держит
/// на `entries` заимствование.
fn fs_pane_list(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    egui::ScrollArea::vertical()
        .id_source("fs_list")
        .max_width(230.0)
        .auto_shrink([true, true])
        .show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                let mut enter: Option<String> = None;
                let mut open: Option<String> = None;
                for e in &fs.entries {
                    let glyph = if e.is_dir { "▸" } else { "·" };
                    let selected = fs.sel.as_deref() == Some(e.name.as_str());
                    let resp = ui.selectable_label(selected, format!("{} {}", glyph, e.name));
                    if resp.clicked() {
                        fs.sel = Some(e.name.clone());
                    }
                    if resp.double_clicked() {
                        if e.is_dir {
                            enter = Some(e.name.clone());
                        } else {
                            open = Some(e.name.clone());
                        }
                    }
                }
                if let Some(d) = enter {
                    fs_enter(fs, &d);
                }
                if let Some(f) = open {
                    fs_open_file(fs, &f);
                }
            });
        });
}

/// Редактор: заголовок файла с меткой правок, кнопка SAVE и многострочное поле.
fn fs_pane_editor(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    // vertical(): длинные строки переносятся, а не скроллятся вбок — та же схема,
    // что у терминала, которая доказанно держит своё окно в 600 px. auto_shrink
    // по X выключен, чтобы редактор занимал весь остаток ряда, а не сжимался.
    egui::ScrollArea::vertical()
        .id_source("fs_code")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.horizontal(|ui| {
                    let (title, color) = if fs.file.is_empty() {
                        (
                            String::from("— файл не открыт —"),
                            egui::Color32::from_rgb(130, 130, 135),
                        )
                    } else if fs.dirty {
                        (format!("● {} *", fs.file), egui::Color32::from_rgb(255, 170, 60))
                    } else {
                        (
                            fs.file.clone(),
                            egui::Color32::from_rgb(130, 130, 135),
                        )
                    };
                    ui.colored_label(color, title);
                    if ui
                        .add_enabled(!fs.file.is_empty(), egui::Button::new("SAVE"))
                        .clicked()
                    {
                        fs_save(fs);
                    }
                });
                // desired_rows — минимум высоты: без него окно схлопывалось бы до
                // пары строк, когда в каталоге два файла, и редактором было бы
                // нечем пользоваться.
                let editor = ui.add(
                    egui::TextEdit::multiline(&mut fs.code)
                        .code_editor()
                        .desired_rows(14)
                        .desired_width(f32::INFINITY),
                );
                if editor.changed() {
                    fs.dirty = true;
                }
            });
        });
}

/// Строка статуса: зелёный на успехе, красный на ошибке ядра.
fn fs_row_status(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    ui.separator();
    let color = if fs.failed {
        egui::Color32::from_rgb(255, 96, 96)
    } else {
        egui::Color32::from_rgb(0, 220, 90)
    };
    ui.colored_label(color, fs.status.as_str());
}

impl VanguardOSApp {
    /// Окно «Файлы»: список перечитывается у ядра по клику, а не каждый кадр —
    /// иначе на каждую рамку уходил бы IPC-вызов.
    pub(super) fn draw_fs_window(&mut self, ctx: &egui::Context) {
        if !self.windows.explorer.is_open {
            // Окно закрыто — сбрасываем флаг: при следующем открытии список
            // должен перечитаться у ядра, файлы могли измениться извне.
            self.fs.loaded = false;
            return;
        }
        if !self.fs.loaded {
            // Первое открытие: список ещё пуст — просим ядро.
            self.fs.loaded = true;
            fs_reload(&mut self.fs);
        }

        self.windows.explorer.show(ctx, |ui| {
            // Каждый ряд ниже — свободная функция, заимствующая только `self.fs`:
            // замыкание не дотягивается до `self` целиком, иначе оно пересеклось бы
            // с уже взятым `self.windows.explorer` (см. шапку модуля).
            fs_row_path(ui, &mut self.fs);
            fs_row_actions(ui, &mut self.fs);
            ui.separator();
            fs_panes(ui, &mut self.fs);
            fs_row_status(ui, &mut self.fs);
        });
    }
}
