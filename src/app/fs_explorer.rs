//! Окно «Файлы»: настоящий проводник по всем дискам машины.
//!
//! Дочерний модуль `app`, а не братский: [`VanguardOSApp::draw_fs_window`] висит
//! на `impl VanguardOSApp` и трогает приватные поля `self.fs` и `self.windows`, а
//! приватность в Rust видна модулю-предку и его потомкам. Из `main.rs` пришлось бы
//! расставлять `pub(crate)` на каждое поле. Та же причина, по которой все
//! помощники `fs_*` — свободные функции: окно вызывает их внутри `show()`, где
//! `&mut self` целиком занят отрисовкой самого окна.
//!
//! С диском оболочка не соприкасается вовсе — ни одного `std::fs`: перечисляет
//! каталоги и читает файлы ядро, здесь живут только состояние запроса и разметка.
//!
//! # Почему IPC идёт в фоне
//! Раньше один [`fs_spawn`] дёргал `ipc_client::call` прямо в кадре рендера, и
//! первое открытие окна при мёртвом ядре клало GUI примерно на 3 секунды —
//! `connect_with_autostart` опрашивает сокет 30×100 мс, — а при зависшем ядре
//! ждал бы таймаута в 30 с. Теперь запрос уходит в отдельный поток, а ответ
//! возвращается каналом и разбирается в [`VanguardOSApp::fs_drain`], который
//! `update()` дренирует **каждый кадр**. Это тот же приём, что у
//! `check_ai_stream`, и та же ловушка: дренаж обязан идти безусловно (иначе
//! результат зависнет до следующего запроса), а `bg_tasks` снимается только
//! после того, как ответ прочитан — иначе egui уснёт с непрочитанным
//! результатом в канале.

use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use super::*;

/// Высоты рядов задаются явно и не берутся из `ui.available_size_before_wrap()`.
///
/// `ui.horizontal()` — это `left_to_right(Align::Center)`, и при центрировании по
/// поперечной оси egui растягивает рамку ряда на всю высоту окна (layout.rs:611-615).
/// Из-за этого два служебных ряда сверху съедали всё место, ряду «список + редактор»
/// доставалось 18 px, и поле кода обрезалось до 46 px (замер: row size=740x18,
/// ScrollArea раздувалась до 64 из min_scrolled_size). `Align::Min` с фиксированной
/// высотой такого не делает.
const ROW_H: f32 = 30.0;

/// Высота ряда «список каталога + редактор». Сюда же относится предупреждение про
/// `allocate_space(vec2(0.0, _))`: в `left_to_right` он кладёт нулевой прямоугольник
/// на 8 px правее края, и min_rect рос на 8 px каждый кадр, пока окно не упиралось
/// в экран. Высота берётся из этой константы в `fs_panes`.
const PANE_H: f32 = 300.0;

/// Ширина колонки списка **вместе** с полосой прокрутки справа.
const LIST_W: f32 = 400.0;
/// Столько прокрутка забирает у строки: ширина строки списка = `LIST_W - 16`.
const LIST_GUTTER: f32 = 16.0;
/// Высота строки списка и отдельно — шапки колонок.
const LIST_ROW_H: f32 = 21.0;
const HEADER_H: f32 = 24.0;
/// Две правые колонки списка, как у столбцов «Размер»/«Дата изменения».
const COL_SIZE_W: f32 = 66.0;
const COL_DATE_W: f32 = 96.0;

/// Состояние окна «Файлы».
///
/// Здесь лежит последний ответ `FsList` и открытый в редакторе файл — поэтому
/// рисование не делает ни одного `std::fs`.
pub(super) struct FsExplorer {
    /// Абсолютный перечисленный каталог; пусто — виртуальный уровень
    /// «Этот компьютер». Ставится **только по ответу ядра**, а не в момент
    /// клика: тогда строка пути всегда совпадает с реально показанным списком,
    /// даже если запрос не прошёл.
    path: String,
    /// Последний список от ядра: порядок уже приведён к «каталоги первыми».
    entries: Vec<FsEntry>,
    /// Выбранная запись — имя внутри текущего каталога.
    sel: Option<String>,
    /// Кто ждёт **второго** клика по «Удалить». Песочницы больше нет, поэтому
    /// один щелчок больше не должен ничего стирать — см. `fs_row_actions`.
    confirm: Option<String>,
    /// Открытый файл: его абсолютный путь и содержимое в редакторе.
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
    /// стучалось бы заново на каждой рамке.
    loaded: bool,
    /// Куда ведёт «←»: предыдущие каталоги, самый старый в конце.
    hist: Vec<String>,
    /// Запрос в полёте. Второй не стартуем — в канале лежит один `Receiver`.
    busy: bool,
    pending: Option<Receiver<FsReply>>,
    /// Та же счётная дуга, что у шелла: `update()` держит по ней кадр живым,
    /// а панель задач показывает «⚙». Общая именно потому, что счётчик один.
    bg: Arc<AtomicUsize>,
}

impl FsExplorer {
    /// `bg` берётся у шелла — это его `Arc`, а не копия: один счётчик на
    /// приложение, иначе индикатор фоновых задач врал бы.
    pub(super) fn new(bg: Arc<AtomicUsize>) -> Self {
        Self {
            path: String::new(),
            entries: Vec::new(),
            sel: None,
            confirm: None,
            file: String::new(),
            code: String::new(),
            dirty: false,
            input: String::new(),
            status: String::from("Загрузка…"),
            failed: false,
            loaded: false,
            hist: Vec::new(),
            busy: false,
            pending: None,
            bg,
        }
    }
}

// ─── Разговор с ядром: отправка и приём ────────────────────────────────────

/// Тип полезной нагрузки известен уже в момент отправки, поэтому фоновый поток
/// просто раскладывает ответ в нужную ячейку [`FsReply`].
enum FsKind {
    /// `FsList` — применить листинг.
    Listing,
    /// `FsRead` — положить содержимое в редактор.
    Reading,
    /// `FsWrite` — снять флаг правок и показать «Сохранено: …».
    Writing(String),
    /// Остальные мутации: при успехе перечитать каталог. `clear_sel` — после
    /// переименования/удаления, у созданий выделение трогать не нужно.
    Mutating { clear_sel: bool },
}

/// Итог одной задачи: уже разобранный JSON плюс метка, что с ним делать.
enum FsReply {
    List(Result<FsListing, String>),
    Read(Result<FsFile, String>),
    Written { path: String, result: Result<String, String> },
    Mutated { clear_sel: bool, result: Result<String, String> },
}

/// Превращает текст `Ok` в структуру. Ошибка формата сходится с ошибкой ядра
/// в одну строку — статусная строка окна показывает просто текст.
fn parse_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| format!("ядро ответило не тем: {}", e))
}

/// Ставит запрос в фон и сразу возвращает управление кадру.
///
/// Возвращает `false`, если запрос не ушёл (в полёте предыдущий) — вызывающему
/// это нужно, чтобы не считать задачу начатой (`loaded` в `draw_fs_window`).
/// `bg_tasks` увеличивается здесь, а уменьшается **в `fs_drain`** уже после
/// прочтения ответа: см. док к модулю.
fn fs_spawn(fs: &mut FsExplorer, req: Request, kind: FsKind) -> bool {
    if fs.busy {
        fs.status = String::from("Предыдущий запрос к ядру ещё идёт…");
        fs.failed = false;
        return false;
    }
    let (tx, rx) = channel();
    fs.busy = true;
    fs.pending = Some(rx);
    fs.status = String::from("Запрос к ядру…");
    fs.failed = false;
    fs.bg.fetch_add(1, Ordering::SeqCst);

    std::thread::spawn(move || {
        let payload: Result<String, String> = match serde_json::to_string(&req) {
            Ok(json) => {
                let reply = crate::ipc_client::call(&json);
                if reply.ok {
                    Ok(reply.text)
                } else {
                    Err(reply.text.trim().to_string())
                }
            }
            Err(e) => Err(format!("не удалось сериализовать запрос: {}", e)),
        };
        let _ = tx.send(match kind {
            FsKind::Listing => FsReply::List(payload.and_then(|t| parse_json(&t))),
            FsKind::Reading => FsReply::Read(payload.and_then(|t| parse_json(&t))),
            FsKind::Writing(path) => FsReply::Written { path, result: payload },
            FsKind::Mutating { clear_sel } => FsReply::Mutated { clear_sel, result: payload },
        });
        // fetch_sub намеренно не здесь — иначе счётчик упал бы раньше, чем
        // fs_drain прочитает ответ, и egui уснул бы с непрочитанным результатом.
    });
    true
}

/// Закрывает задачу: полёта больше нет, счётчик фоновых задач возвращён в 0.
fn fs_finish(fs: &mut FsExplorer) {
    fs.busy = false;
    fs.pending = None;
    fs.bg.fetch_sub(1, Ordering::SeqCst);
}

/// Ошибка до разбора ответа (поток умер, не успев отправить).
fn fs_fail(fs: &mut FsExplorer, text: String) {
    fs.status = text;
    fs.failed = true;
}

/// Кладёт ответ в состояние окна. Может **запустить** следующий запрос
/// (перечитывание каталога после успешной мутации) — поэтому вызывается
/// уже после [`fs_finish`], когда `busy` снят.
fn fs_apply_reply(fs: &mut FsExplorer, reply: FsReply) {
    match reply {
        FsReply::List(res) => match res {
            Ok(list) => {
                // Ядро прислало канонический путь — он и есть адрес каталога
                // (регистр, junction, двойные слэши выравниваются там).
                fs.path = list.path;
                fs.entries = list.entries;
                fs.status = format!("{} объектов", fs.entries.len());
                fs.failed = false;
            }
            // Список НЕ чистим: показываем прошлый (он как раз и есть текущий
            // адрес) и красную строку причины — так же ведёт себя и Windows,
            // когда обновить каталог не удалось.
            Err(e) => fs_fail(fs, e),
        },
        FsReply::Read(res) => match res {
            Ok(f) => {
                fs.file = f.path;
                fs.code = f.content;
                fs.dirty = false;
                fs.status = format!("Открыт: {}", fs.file);
                fs.failed = false;
            }
            Err(e) => fs_fail(fs, e),
        },
        FsReply::Written { path, result } => match result {
            Ok(_) => {
                fs.dirty = false;
                fs.status = format!("Сохранено: {}", path);
                fs.failed = false;
            }
            Err(e) => fs_fail(fs, e),
        },
        FsReply::Mutated { clear_sel, result } => match result {
            Ok(_) => {
                if clear_sel {
                    fs.sel = None;
                    fs.confirm = None;
                }
                let path = fs.path.clone();
                fs_spawn(fs, Request::FsList { path }, FsKind::Listing);
            }
            Err(e) => fs_fail(fs, e),
        },
    }
}

// ─── Пути ───────────────────────────────────────────────────────────────────

/// Абсолютный родитель каталога. `None` — уже на «Этот компьютере».
///
/// У корня диска `Path::parent()` возвращает `None`, и это ровно то, что нужно:
/// `C:\` → виртуальный уровень, а не `C:` (без слэша это относительный путь).
fn fs_parent(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    Some(Path::new(path).parent().map(fs_pretty).unwrap_or_default())
}

/// `canonicalize()` даёт verbatim-путь `\\?\C:\...`; для строки пути и крошек
/// префикс срезаем. Сравнения идут по сырому пути из ответа ядра.
fn fs_pretty(path: &Path) -> String {
    let s = path.display().to_string();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// Путь элемента внутри каталога. На «Этот компьютере» каталога нет — там
/// имя элемента уже и есть готовый путь (`C:\`).
fn fs_join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else if dir.ends_with('\\') {
        format!("{}{}", dir, name)
    } else {
        format!("{}\\{}", dir, name)
    }
}

/// Хлебные крошки: абсолютный путь → пары `(подпись, куда вести)`.
///
/// Первый сегмент — диск, и его **путь** пишется с хвостовым слэшем
/// (`C:\`), а подпись — без (`C:`): `Path::new("C:")` на Windows означает
/// «текущий каталог диска C», а не его корень.
fn fs_crumbs(path: &str) -> Vec<(String, String)> {
    if path.is_empty() {
        return vec![(String::from("Этот компьютер"), String::new())];
    }
    // Общие ресурсы (`\\server\share`) отдаются одним куском: ломать их на
    // сегменты негде, а первый сегмент такого пути вообще пустой.
    if path.starts_with("\\\\") {
        let p = path.trim_end_matches('\\').to_string();
        return vec![(p.clone(), p)];
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut acc = String::new();
    for part in path.split('\\').filter(|p| !p.is_empty()) {
        if acc.is_empty() {
            acc = format!("{}:\\", part);
            out.push((format!("{}:", part), acc.clone()));
        } else {
            if !acc.ends_with('\\') {
                acc.push('\\');
            }
            acc.push_str(part);
            out.push((part.to_string(), acc.clone()));
        }
    }
    out
}

// ─── Действия над каталогом ────────────────────────────────────────────────

/// Переход в каталог: пишет историю и уходит в ядро.
///
/// `fs.path` при этом НЕ трогаем — его обновит ответ [`FsReply::List`], и
/// строка пути не соврёт, если запрос не дойдёт.
fn fs_navigate(fs: &mut FsExplorer, path: &str) {
    if !fs_leave(fs) || fs.path == path {
        return;
    }
    fs.hist.push(fs.path.clone());
    if fs.hist.len() > 64 {
        fs.hist.remove(0);
    }
    fs.sel = None;
    fs.confirm = None;
    fs_spawn(fs, Request::FsList { path: path.to_string() }, FsKind::Listing);
}

/// Перечитывает текущий каталог у ядра (кнопка ⟳ и авто-перечитывание
/// после успешной мутации).
fn fs_reload(fs: &mut FsExplorer) {
    let path = fs.path.clone();
    fs_spawn(fs, Request::FsList { path }, FsKind::Listing);
}

/// Назад по истории.
fn fs_back(fs: &mut FsExplorer) {
    if !fs_leave(fs) {
        return;
    }
    let prev = fs.hist.pop();
    if let Some(p) = prev {
        fs.sel = None;
        fs.confirm = None;
        fs_spawn(fs, Request::FsList { path: p }, FsKind::Listing);
    }
}

/// Вверх на каталог.
fn fs_up(fs: &mut FsExplorer) {
    let parent = fs_parent(&fs.path);
    if let Some(p) = parent {
        fs_navigate(fs, &p);
    }
}

/// Войти в подкаталог.
fn fs_enter(fs: &mut FsExplorer, dir: &str) {
    let target = fs_join(&fs.path, dir);
    fs_navigate(fs, &target);
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

/// Открыть файл в редакторе.
fn fs_open_file(fs: &mut FsExplorer, name: &str) {
    if !fs_leave(fs) {
        return;
    }
    let path = fs_join(&fs.path, name);
    fs_spawn(fs, Request::FsRead { path }, FsKind::Reading);
}

/// Записать открытый файл обратно.
///
/// `Request::FsWrite` перезаписывает целиком — это его назначение: это путь
/// SAVE. Создание нового файла делает `fs_btn_create` с [`Request::FsCreate`].
fn fs_save(fs: &mut FsExplorer) {
    if fs.file.is_empty() {
        fs.status = String::from("Файл не открыт.");
        fs.failed = true;
        return;
    }
    let path = fs.file.clone();
    let content = fs.code.clone();
    fs_spawn(fs, Request::FsWrite { path: path.clone(), content }, FsKind::Writing(path));
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

// ─── Оформление списка: колонки и формат ───────────────────────────────────

/// Ширина строки списка: у `LIST_W` уходит полоса прокрутки.
fn fs_row_width() -> f32 {
    LIST_W - LIST_GUTTER
}

/// Правые края колонок «Размер» и «Изменён» внутри строки шириной `w`.
/// Шапка и строки пользуются одной функцией, иначе заголовки разъедутся.
fn fs_cols(w: f32) -> (f32, f32) {
    (w - COL_DATE_W - 4.0, w - 4.0)
}

/// Сколько влезает в колонку имени: от левого края до начала «Размер».
fn fs_name_end(w: f32) -> f32 {
    fs_cols(w).0 - COL_SIZE_W - 6.0
}

fn fs_fmt_size(e: &FsEntry) -> String {
    if e.is_dir {
        return String::from("—");
    }
    const K: f64 = 1024.0;
    let b = e.size as f64;
    if b < K {
        format!("{} Б", e.size)
    } else if b < K * K {
        format!("{:.1} КБ", b / K)
    } else if b < K * K * K {
        format!("{:.1} МБ", b / (K * K))
    } else {
        format!("{:.1} ГБ", b / (K * K * K))
    }
}

/// Дата изменения как в «Проводнике»: `19.10.2026 14:32`, локальное время.
fn fs_fmt_date(unix: u64) -> String {
    if unix == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(unix as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%d.%m.%Y %H:%M").to_string())
        .unwrap_or_default()
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
    if ui.add_enabled(!fs.busy, egui::Button::new(label)).clicked() {
        if let Some(name) = fs_named_input(fs, prompt) {
            let path = fs_join(&fs.path, &name);
            fs_spawn(fs, make(path), FsKind::Mutating { clear_sel: false });
        }
    }
}

/// Строка пути: «←», хлебные крошки, ⟳ и подсказка справа.
fn fs_row_path(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    let mut back = false;
    let mut up = false;
    let mut reload = false;
    let mut go: Option<String> = None;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), ROW_H),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| {
            if ui
                .add_enabled(!fs.busy && !fs.hist.is_empty(), egui::Button::new("←"))
                .on_hover_text("Назад")
                .clicked()
            {
                back = true;
            }
            // ↑ — как в «Проводнике Windows»: отдельная кнопка, а не только
            // клик по родительской крошке (та ведёт на конкретный сегмент пути).
            if ui
                .add_enabled(!fs.busy && !fs.path.is_empty(), egui::Button::new("↑"))
                .on_hover_text("Вверх")
                .clicked()
            {
                up = true;
            }
            // Крошки собираются до цикла: дальше `fs` берётся целиком, а
            // список подписок на него висеть не должен.
            let crumbs = fs_crumbs(&fs.path);
            for (i, (label, full)) in crumbs.iter().enumerate() {
                if i > 0 {
                    ui.label(
                        egui::RichText::new("›").size(11.0).color(egui::Color32::from_rgb(140, 140, 148)),
                    );
                }
                if ui.small_button(label).clicked() {
                    go = Some(full.clone());
                }
            }
            ui.add_space(8.0);
            if ui
                .add_enabled(!fs.busy, egui::Button::new("⟳"))
                .on_hover_text("Обновить список")
                .clicked()
            {
                reload = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(130, 130, 135),
                    "двойной клик — открыть",
                );
            });
        },
    );
    if back {
        fs_back(fs);
    } else if up {
        fs_up(fs);
    } else if reload {
        fs_reload(fs);
    } else if let Some(target) = go {
        fs_navigate(fs, &target);
    }
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
                    .desired_width(160.0),
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
                .add_enabled(has_sel && !fs.busy, egui::Button::new("→ Переименовать"))
                .clicked()
            {
                // По ссылке: имя нужно ещё ниже, в ветке «Удалить».
                if let Some(name) = &sel_name {
                    if let Some(new_name) = fs_named_input(fs, "Введите новое имя.") {
                        let from = fs_join(&fs.path, name);
                        let to = fs_join(&fs.path, &new_name);
                        fs_spawn(
                            fs,
                            Request::FsRename { from, to },
                            FsKind::Mutating { clear_sel: true },
                        );
                    }
                }
            }
            // Удаление в два клика. Песочницы больше нет (корень — все диски),
            // поэтому один щелчок по выделенной строке мог бы унести дерево
            // целиком; первый клик только метит цель и красит кнопку.
            let armed = fs.confirm.clone();
            let del_label = match &armed {
                Some(n) if sel_name.as_deref() == Some(n.as_str()) => {
                    format!("✓ Точно «{}»?", n)
                }
                _ => String::from("✕ Удалить"),
            };
            if ui
                .add_enabled(has_sel && !fs.busy, egui::Button::new(del_label))
                .clicked()
            {
                if let Some(name) = sel_name.clone() {
                    if armed.as_deref() != Some(name.as_str()) {
                        fs.confirm = Some(name);
                    } else {
                        fs.confirm = None;
                        let path = fs_join(&fs.path, &name);
                        fs_spawn(
                            fs,
                            Request::FsDelete { path },
                            FsKind::Mutating { clear_sel: true },
                        );
                    }
                }
            }
        },
    );
}

/// Шапка колонок списка. Рисуется отдельно от строк и не прокручивается —
/// иначе после десятой записи было бы непонятно, где размер, а где дата.
fn fs_list_header(ui: &mut egui::Ui, w: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, HEADER_H), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(38, 38, 44));
    let (size_x, date_x) = fs_cols(w);
    let cy = rect.center().y;
    let font = egui::FontId::proportional(10.0);
    let color = egui::Color32::from_rgb(160, 160, 168);
    painter.text(
        rect.left_center() + egui::vec2(6.0, 0.0),
        egui::Align2::LEFT_CENTER,
        "Имя",
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(size_x, cy),
        egui::Align2::RIGHT_CENTER,
        "Размер",
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(date_x, cy),
        egui::Align2::RIGHT_CENTER,
        "Изменён",
        font,
        color,
    );
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(60, 60, 66)),
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

/// Список записей текущего каталога: колонки «Имя / Размер / Изменён».
///
/// Строка — один прямоугольник, а не `selectable_label`: так колонки справа
/// выравниваются по правому краю, а не едут за длинным именем. Двойной клик
/// откладывается до конца цикла: `fs_enter`/`fs_open_file` берут
/// `&mut FsExplorer` целиком, а цикл держит на `entries` заимствование.
fn fs_pane_list(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    let w = fs_row_width();
    ui.allocate_ui_with_layout(
        egui::vec2(LIST_W, PANE_H),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            fs_list_header(ui, w);
            egui::ScrollArea::vertical()
                .id_source("fs_list")
                .max_width(LIST_W)
                .max_height(PANE_H - HEADER_H)
                .auto_shrink([true, false])
                .show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        let mut enter: Option<String> = None;
                        let mut open: Option<String> = None;
                        for e in &fs.entries {
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(w, LIST_ROW_H),
                                egui::Sense::click(),
                            );
                            let selected = fs.sel.as_deref() == Some(e.name.as_str());
                            if selected {
                                ui.painter().rect_filled(
                                    rect,
                                    3.0,
                                    egui::Color32::from_rgb(52, 74, 110),
                                );
                            } else if resp.hovered() {
                                ui.painter().rect_filled(
                                    rect,
                                    3.0,
                                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 20),
                                );
                            }

                            // Имя — с обрезкой по колонке: длинное имя не должен
                            // заезжать под «Размер», как и в «Проводнике».
                            let name_rect = egui::Rect::from_min_max(
                                egui::pos2(rect.left() + 4.0, rect.top()),
                                egui::pos2(fs_name_end(w), rect.bottom()),
                            );
                            let glyph = if e.is_dir { "▸" } else { "·" };
                            ui.painter().with_clip_rect(name_rect).text(
                                name_rect.left_center(),
                                egui::Align2::LEFT_CENTER,
                                format!("{} {}", glyph, e.name),
                                egui::FontId::proportional(12.0),
                                egui::Color32::from_rgb(224, 224, 230),
                            );

                            let cy = rect.center().y;
                            let (size_x, date_x) = fs_cols(w);
                            let dim = egui::Color32::from_rgb(168, 168, 176);
                            let font = egui::FontId::proportional(10.0);
                            ui.painter().text(
                                egui::pos2(size_x, cy),
                                egui::Align2::RIGHT_CENTER,
                                fs_fmt_size(e),
                                font.clone(),
                                dim,
                            );
                            ui.painter().text(
                                egui::pos2(date_x, cy),
                                egui::Align2::RIGHT_CENTER,
                                fs_fmt_date(e.mtime),
                                font,
                                dim,
                            );

                            if resp.clicked() {
                                // Смена выделения снимает метку «Удалить» —
                                // иначе второй клик стёр бы уже другую цель.
                                if fs.sel.as_deref() != Some(e.name.as_str()) {
                                    fs.confirm = None;
                                }
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
        },
    );
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
                        .add_enabled(!fs.busy && !fs.file.is_empty(), egui::Button::new("SAVE"))
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

/// Строка статуса: зелёный на успехе, красный на ошибку ядра, ⏳ пока в полёте.
fn fs_row_status(ui: &mut egui::Ui, fs: &mut FsExplorer) {
    ui.separator();
    let color = if fs.failed {
        egui::Color32::from_rgb(255, 96, 96)
    } else {
        egui::Color32::from_rgb(0, 220, 90)
    };
    let text = if fs.busy {
        format!("⏳ {}", fs.status)
    } else {
        fs.status.clone()
    };
    ui.colored_label(color, text);
}

impl VanguardOSApp {
    /// Разбирает ответы фоновых запросов «Файлов». Вызывается из `update()`
    /// **каждый кадр и без условий** — та же дисциплина, что у `check_ai_stream`:
    /// иначе результат зависнет в канале до следующего открытия окна, а
    /// `bg_tasks` не упадёт и egui не перестанет просить перерисовку.
    pub(super) fn fs_drain(&mut self) {
        let rx = match self.fs.pending.take() {
            Some(rx) => rx,
            None => return,
        };
        let reply = match rx.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => {
                // Ещё в полёте — возвращаем приёмник на место.
                self.fs.pending = Some(rx);
                return;
            }
            Err(TryRecvError::Disconnected) => {
                fs_finish(&mut self.fs);
                fs_fail(&mut self.fs, String::from("Фоновый запрос оборвался."));
                return;
            }
        };
        // Снимаем счётчик ДО применения ответа: применение может запустить
        // следующий запрос, который снова его поднимет.
        fs_finish(&mut self.fs);
        fs_apply_reply(&mut self.fs, reply);
    }

    /// Окно «Файлы»: список перечитывается у ядро по клику, а не каждый кадр —
    /// иначе на каждую рамку уходил бы IPC-вызов.
    pub(super) fn draw_fs_window(&mut self, ctx: &egui::Context) {
        if !self.windows.explorer.is_open {
            // Окно закрыто — сбрасываем флаг: при следующем открытии список
            // должен перечитаться у ядра, файлы могли измениться извне.
            self.fs.loaded = false;
            return;
        }
        if !self.fs.loaded {
            // Первое открытие: список ещё пуст — просим ядро. `loaded` ставим
            // только если запрос реально ушёл, иначе при занятом полёте окно
            // навсегда решило бы, что список уже есть.
            self.fs.loaded = fs_reload_result(&mut self.fs);
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

/// Первое открытие окна: запрос списка текущего каталога. Отдельная обёртка
/// нужна, чтобы вернуть `bool` от [`fs_reload`] — см. `loaded` выше.
fn fs_reload_result(fs: &mut FsExplorer) -> bool {
    let path = fs.path.clone();
    fs_spawn(fs, Request::FsList { path }, FsKind::Listing)
}
