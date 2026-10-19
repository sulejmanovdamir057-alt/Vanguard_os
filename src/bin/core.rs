//! Ядро Vanguard OS — TCP-сервер движка, без графики.
//!
//! Отдельный процесс (`vanguard_core.exe`). Слушает `127.0.0.1:7331`, принимает
//! [`vanguard_os_lib::ipc::Request`] построчно и отвечает [`Response`] построчно.
//! Оболочка и ИИ-помощник подключаются как клиенты; эндпоинты ИИ при этом
//! держатся у клиентов — ядро отвечает только за тяжёлые и системные операции:
//!
//! - слеш-команды шелла (`/apps`, `/hardware`, `/wipe`, `/help`, `/reset`);
//! - исполнение скриптов (`/run`) фоном с потоком логов;
//! - **компиляция Rust-кода `rustc -O`** — главная причина выноса в отдельный
//!   процесс: раньше она шла в UI-потоке оболочки и замораживала рендер на
//!   секунды (единственная реальная заморозка, замеренная в этой сборке);
//! - телеметрия CPU/RAM/DISK — свой экземпляр daemon'а, чтобы оболочка не
//!   держала sysinfo-цикл в процессе рендера.
//!
//! Протокол — JSON-строки поверх TCP на локалхосте (см. [`ipc`]). Каждое соединение
//! обслуживается в своём потоке; фоновые задачи (`/run`, компиляция) живут в
//! отдельных потоках и шлют события, пока соединение открыто.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use vanguard_os_lib::core::executor::VanguardExecutor;
use vanguard_os_lib::core::shell::VanguardShell;
use vanguard_os_lib::core::telemetry::{spawn_telemetry_daemon, SystemMetrics};
use vanguard_os_lib::ipc::{self, Event, FsEntry, FsFile, FsListing, Request, Response};

fn main() {
    let addr = ipc::core_addr();
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vanguard_core: не удалось занять {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    // Телеметрия живёт в ядре: оболочка спрашивает её по IPC, а не держит
    // собственный цикл sysinfo внутри рендер-процесса.
    let metrics = Arc::new(std::sync::Mutex::new(SystemMetrics::new()));
    spawn_telemetry_daemon(Arc::clone(&metrics));

    // Счётчик незавершённых фоновых задач — для /run и компиляции.
    let bg_tasks = Arc::new(AtomicUsize::new(0));

    // Корень песочницы проводника. Резолвится один раз при старте — рабочая
    // директория самого ядра (см. AGENTS: запускать из корня проекта, иначе
    // оболочка и ядро увидят разные файлы). Именно против него проверяются
    // все Fs*-пути, поэтому оболочка не может выйти за пределы проекта.
    let sandbox_root: Arc<PathBuf> = Arc::new(
        std::env::current_dir()
            .ok()
            .and_then(|p| std::fs::canonicalize(&p).ok())
            .unwrap_or_else(|| PathBuf::from(".")),
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let metrics = Arc::clone(&metrics);
                let bg_tasks = Arc::clone(&bg_tasks);
                let sandbox_root = Arc::clone(&sandbox_root);
                thread::spawn(move || handle_client(stream, metrics, bg_tasks, sandbox_root));
            }
            Err(e) => eprintln!("vanguard_core: сбой приёма соединения: {}", e),
        }
    }
}

/// Обслуживает одного клиента. Каждый клиент держит собственный `VanguardShell`
/// (своя история диалога), чтобы параллельные подключения не мешали друг другу.
fn handle_client(
    stream: TcpStream,
    metrics: Arc<std::sync::Mutex<SystemMetrics>>,
    bg_tasks: Arc<AtomicUsize>,
    root: Arc<PathBuf>,
) {
    let reader = BufReader::new(match stream.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut writer = stream;
    let mut shell = VanguardShell::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // соединение закрыто
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                send(&mut writer, &Response::err(format!("битый запрос: {}", e)));
                continue;
            }
        };

        // Тяжёлые и фоновые операции уходят в отдельный поток, чтобы не блокировать
        // остальные запросы этого же клиента; события шлются в его writer.
        match request {
            Request::Ping => send(&mut writer, &Response::Pong),
            Request::Shell { query } => {
                let out = dispatch_shell(&mut shell, &query);
                send(&mut writer, &out);
            }
            Request::RunScript { id } => {
                let mut w = match writer.try_clone() {
                    Ok(w) => w,
                    Err(_) => return,
                };
                let bg = Arc::clone(&bg_tasks);
                bg.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    let log = VanguardExecutor::execute_script(&id);
                    // Лог скрипта режем на строки и шлём событиями: так он
                    // появляется в GUI-чате построчно, как раньше через log_tx.
                    for l in log.lines() {
                        send(&mut w, &Response::Event { id: 0, event: Event::TaskLog { line: l.to_string() } });
                    }
                    send(&mut w, &Response::ok("<< TASK DONE >>"));
                    bg.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Request::CompileRust { code, description, gen_id } => {
                let mut w = match writer.try_clone() {
                    Ok(w) => w,
                    Err(_) => return,
                };
                let bg = Arc::clone(&bg_tasks);
                bg.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    let out = compile_rust(&code, &description, gen_id);
                    send(&mut w, &Response::ok(out));
                    bg.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Request::SaveScript { code, description, gen_id } => {
                let out = save_script(&code, &description, gen_id);
                send(&mut writer, &Response::ok(out));
            }
            Request::ReadScript { id } => {
                let out = read_script(&id);
                send(&mut writer, &out);
            }
            Request::ListScripts => {
                let out = list_scripts();
                send(&mut writer, &Response::ok(out));
            }
            Request::DialogueCount => {
                let out = vanguard_os_lib::core::shell::count_dialogue_lines();
                send(&mut writer, &Response::ok(out.to_string()));
            }
            // ── Проводник «Файлы»: всё через песочницу `root` ──
            Request::FsList { path } => send(&mut writer, &fs_list(&root, &path)),
            Request::FsRead { path } => send(&mut writer, &fs_read(&root, &path)),
            Request::FsWrite { path, content } => {
                send(&mut writer, &fs_write(&root, &path, &content))
            }
            Request::FsCreate { path } => send(&mut writer, &fs_create(&root, &path)),
            Request::FsMkdir { path } => send(&mut writer, &fs_mkdir(&root, &path)),
            Request::FsRename { from, to } => send(&mut writer, &fs_rename(&root, &from, &to)),
            Request::FsDelete { path } => send(&mut writer, &fs_delete(&root, &path)),
        }
    }

    let _ = metrics; // экземпляр держится для жизни потока — daemon крутится в main
}

/// Прогоняет слеш-команду через шелл и превращает результат в IPC-ответ.
/// `/run` тоже здесь, но для потока логов клиенту удобнее `Request::RunScript`.
fn dispatch_shell(shell: &mut VanguardShell, query: &str) -> Response {
    use vanguard_os_lib::core::shell::ShellAction;
    match shell.handle_command(query) {
        ShellAction::DisplayOutput(text) => Response::ok(text),
        ShellAction::SystemExit => Response::ok(">> ЯДРО: выключение запрошено клиентом.\n"),
        // Запрос к ИИ ядро не исполняет — эндпоинт Ollama живёт у клиента.
        // Сообщаем, что строка именно для ИИ-шины, а не слеш-команда.
        ShellAction::AiStreamAllowed => Response::ok("<< AI_STREAM_ALLOWED >>"),
    }
}

/// Компиляция Rust-кода ИИ через `rustc -O`. Вынесена из UI-потока оболочки:
/// здесь она может сколько угодно — рендер клиента не страдает.
fn compile_rust(code: &str, description: &str, gen_id: u32) -> String {
    use std::fs;
    use std::process::Command;

    let source_file = format!("vanguard_tmp_{}.rs", gen_id);
    let binary_file = format!("vanguard_app_{}.exe", gen_id);

    if let Err(e) = fs::write(&source_file, code) {
        return format!(">> [ИИ] {}\n>> ОШИБКА ЗАПИСИ исходника {}: {}\n", description, source_file, e);
    }

    // .output(), а не .status(): ловим stderr, иначе сбой компиляции не диагностируется.
    match Command::new("rustc").args(["-O", &source_file, "-o", &binary_file]).output() {
        Ok(out) if out.status.success() => {
            let _ = fs::remove_file(&source_file);
            format!(
                ">> [ИИ] {}\n>> УСПЕШНАЯ КОМПИЛЯЦИЯ ЯДРА!\n>> Бинарник готов: {}.\n",
                description, binary_file
            )
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let head: Vec<&str> = stderr.lines().take(14).collect();
            format!(
                ">> СБОЙ КОМПИЛЯЦИИ RUST-МОДУЛЯ ИИ (rustc exit {}).\n>> Исходник сохранён для разбора: {}\n{}\n",
                out.status.code().unwrap_or(-1),
                source_file,
                head.join("\n")
            )
        }
        Err(e) => {
            let _ = fs::remove_file(&source_file);
            format!(">> СБОЙ КОМПИЛЯЦИИ: rustc не запустился: {}\n", e)
        }
    }
}

/// Сохраняет Python-код ИИ в `vanguard_gen_<id>.py`.
fn save_script(code: &str, description: &str, gen_id: u32) -> String {
    use std::fs;
    let filename = format!("vanguard_gen_{}.py", gen_id);
    match fs::write(&filename, code) {
        Ok(_) => format!(
            ">> [ИИ] {}\n>> Создан Python-скрипт ID: {}. Запуск через /run {}\n",
            description, gen_id, gen_id
        ),
        Err(e) => format!(">> [ИИ] {}\n>> ОШИБКА ЗАПИСИ {}: {}\n", description, filename, e),
    }
}

/// Читает исходник скрипта для проводника.
fn read_script(id: &str) -> Response {
    let filename = format!("vanguard_gen_{}.py", id);
    match std::fs::read_to_string(&filename) {
        Ok(text) => Response::ok(text),
        Err(e) => Response::err(format!("не удалось прочитать {}: {}", filename, e)),
    }
}

/// Список сгенерированных скриптов: (id, имя файла, размер в КБ).
fn list_scripts() -> String {
    use std::fs;
    let mut out = String::new();
    if let Ok(entries) = glob::glob("vanguard_gen_*.py") {
        for entry in entries.flatten() {
            let name = entry.to_string_lossy().into_owned();
            let id = vanguard_os_lib::core::shell::script_id_from_path(&name).unwrap_or_else(|| "???".to_string());
            let size = fs::metadata(&entry).map(|m| m.len() as f64 / 1024.0).unwrap_or(0.0);
            out.push_str(&format!("{}|{}|{:.2}\n", id, name, size));
        }
    }
    out
}

/// Лимит на открываемый файл. Оболочка держит текст в памяти и рисует его
/// каждый кадр — лог в сотню мегабайтов положил бы рендер.
const FS_VIEW_LIMIT: u64 = 1_048_576;

/// Приводит относительный путь протокола к общему виду: слэши `/`, без
/// ведущего и хвостового. Обе стороны хранят пути именно так, чтобы оболочке
/// не приходилось знать о различиях Windows и остальных систем.
fn fs_norm(rel: &str) -> String {
    rel.replace('\\', "/").trim_matches('/').to_string()
}

/// Резолвит относительный путь песочницы в абсолютный и проверяет, что тот
/// остался внутри корня.
///
/// Проверка тройная, потому что ловушек здесь три:
/// 1. компонент `..` (и `:` — префикс диска / альтернативный поток NTFS)
///    отвергается **до** `canonicalize()`: иначе путь мог уйти наружу и
///    вернуться обратно через junction, и проверка после была бы формальной;
/// 2. `canonicalize()` работает только для существующих путей, а проводник
///    создаёт новые — поэтому ищется ближайший существующий предок;
/// 3. сравнение `starts_with(root)` идёт по компонентам, а не по тексту:
///    `C:\vanguard_os_evil` не должен считаться наследником `C:\vanguard_os`.
fn sandbox_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = fs_norm(rel);
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    for part in rel.split('/') {
        if part == ".." || part.contains(':') {
            return Err("путь выходит за пределы песочницы".to_string());
        }
    }
    let joined = root.join(&rel);

    // Несуществующий путь — норма: и «новый файл», и `mkdir -p` с несколькими
    // новыми уровнями. Поднимаемся вверх до первого существующего каталога,
    // канонизируем его (это и есть момент проверки границы) и дописываем
    // снятые по пути компоненты обратно. Новая компонента не может быть
    // симлинком — её ещё нет на диске, — так что доверять ей безопасно.
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = joined;
    loop {
        match std::fs::canonicalize(&probe) {
            Ok(p) if p.starts_with(root) => {
                let mut resolved = p;
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Ok(_) => return Err("путь выходит за пределы песочницы".to_string()),
            Err(_) => {
                let name = probe
                    .file_name()
                    .map(|n| n.to_os_string())
                    .ok_or_else(|| "пустой путь".to_string())?;
                missing.push(name);
                probe = probe
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| "нет существующего предка пути".to_string())?;
            }
        }
    }
}

/// `canonicalize()` на Windows возвращает verbatim-путь `\\?\C:\...`. Для
/// показа в строке пути проводника префикс срезаем. Сравнивать при этом надо
/// по-прежнему каноническим путём — иначе `starts_with(root)` сломается.
fn fs_pretty(path: &Path) -> String {
    let s = path.display().to_string();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// Складывает ответ проводника в IPC-формат: успех — JSON, ошибка — `Err`.
/// Полезная нагрузка едет в тексте `Ok`, как и весь остальной протокол.
fn fs_reply<T: serde::Serialize>(result: Result<T, String>) -> Response {
    match result {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(json) => Response::ok(json),
            Err(e) => Response::err(format!("ядро не смогло сериализовать ответ: {}", e)),
        },
        Err(e) => Response::err(e),
    }
}

/// Список директории: сортировка «директории первыми, дальше по алфавиту» —
/// ровно так, как в проводнике Windows.
fn fs_list(root: &Path, rel: &str) -> Response {
    let dir = match sandbox_path(root, rel) {
        Ok(d) => d,
        Err(e) => return Response::err(e),
    };
    let meta = match std::fs::metadata(&dir) {
        Ok(m) => m,
        Err(e) => return Response::err(format!("не удалось открыть каталог: {}", e)),
    };
    if !meta.is_dir() {
        return Response::err("это файл, а не каталог".to_string());
    }
    let mut entries: Vec<FsEntry> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| FsEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir: e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                size: e.metadata().map(|m| m.len()).unwrap_or(0),
            })
            .collect(),
        Err(e) => return Response::err(format!("не удалось прочитать каталог: {}", e)),
    };
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    fs_reply(Ok(FsListing {
        path: fs_norm(rel),
        root: fs_pretty(root),
        entries,
    }))
}

/// Чтение файла. Бинарники отклоняются явно: `from_utf8` упал бы с ошибкой
/// кодировки, а hex-дамп в редакторе — это шум, а не проводник.
fn fs_read(root: &Path, rel: &str) -> Response {
    let path = match sandbox_path(root, rel) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return Response::err(format!("не удалось открыть файл: {}", e)),
    };
    if meta.is_dir() {
        return Response::err("это каталог, а не файл".to_string());
    }
    if meta.len() > FS_VIEW_LIMIT {
        return Response::err(format!(
            "файл слишком большой для просмотра ({} КБ, лимит 1024 КБ)",
            meta.len() / 1024
        ));
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return Response::err(format!("не удалось прочитать: {}", e)),
    };
    match String::from_utf8(bytes) {
        Ok(content) => fs_reply(Ok(FsFile { path: fs_norm(rel), content })),
        Err(_) => Response::err("файл не в UTF-8 — бинарник, показывать нечего".to_string()),
    }
}

/// Запись файла: создаёт или перезаписывает целиком. Назначен на сохранение
/// открытого в редакторе файла, поэтому перезапись здесь ожидаема. Для создания
/// нового файла есть [`fs_create`] — он один не даёт занялому имени исчезнуть.
fn fs_write(root: &Path, rel: &str, content: &str) -> Response {
    let path = match sandbox_path(root, rel) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    match std::fs::write(&path, content) {
        Ok(()) => fs_reply(Ok(fs_norm(rel))),
        Err(e) => Response::err(format!("не удалось записать {}: {}", fs_norm(rel), e)),
    }
}

/// Создание нового пустого файла.
///
/// Отдельный запрос, а не `fs_write` с пустым содержимым: `fs::write` создаёт
/// файл **или урезает существующий до нуля**, так что кнопка «＋ Файл» молча
/// обнуляла файл с уже занятым именем — потеря данных без единого намёка.
/// `create_new(true)` запрашивает создание атомарно и падает с `AlreadyExists`,
/// если файл появился между проверкой и созданием — устаревший список каталога
/// в оболочке поэтому ничего не ломает.
fn fs_create(root: &Path, rel: &str) -> Response {
    let path = match sandbox_path(root, rel) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    let opts = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path);
    match opts {
        Ok(_) => fs_reply(Ok(fs_norm(rel))),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Response::err(format!("«{}» уже существует — имя занято.", fs_norm(rel)))
        }
        Err(e) => Response::err(format!("не удалось создать {}: {}", fs_norm(rel), e)),
    }
}

/// Создание каталога вместе со всеми родителями.
fn fs_mkdir(root: &Path, rel: &str) -> Response {
    let path = match sandbox_path(root, rel) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    match std::fs::create_dir_all(&path) {
        Ok(()) => fs_reply(Ok(fs_norm(rel))),
        Err(e) => Response::err(format!("не удалось создать каталог: {}", e)),
    }
}

/// Переименование/перемещение. Корень песочницы защищён отдельно:
/// `sandbox_path("")` легально возвращает его самого.
///
/// На занятое имя переименование **не** идёт: `std::fs::rename` заменяет
/// существующий файл молча (документация std: «replacing the original file if
/// `to` already exists», и на Windows 10 1607+ поведение совпадает с Unix),
/// а в проводнике переименование на занятое имя — это ошибка, как в
/// Windows-Обозревателе, а не тихая потеря содержимого цели.
fn fs_rename(root: &Path, from: &str, to: &str) -> Response {
    let src = match sandbox_path(root, from) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    let dst = match sandbox_path(root, to) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    if src == root {
        return Response::err("корень песочницы переименовать нельзя".to_string());
    }
    if src == dst {
        // То же имя — переименовывать нечего; без этого ранний выход ниже
        // сработал бы как «имя занято» на собственное имя файла.
        return fs_reply(Ok(fs_norm(to)));
    }
    if dst.exists() {
        return Response::err(format!("«{}» уже существует — имя занято.", fs_norm(to)));
    }
    match std::fs::rename(&src, &dst) {
        Ok(()) => fs_reply(Ok(fs_norm(to))),
        Err(e) => Response::err(format!("не удалось переименовать: {}", e)),
    }
}

/// Удаление: файл — как файл, директория — деревом целиком. `remove_dir_all`
/// на корне уничтожил бы весь проект, поэтому он отдельно под запретом.
fn fs_delete(root: &Path, rel: &str) -> Response {
    let path = match sandbox_path(root, rel) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    if path == root {
        return Response::err("корень песочницы удалить нельзя".to_string());
    }
    let result = if path.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    };
    match result {
        Ok(()) => fs_reply(Ok(fs_norm(rel))),
        Err(e) => Response::err(format!("не удалось удалить {}: {}", fs_norm(rel), e)),
    }
}

/// Пишет одну JSON-строку ответа в соединение.
fn send(writer: &mut TcpStream, response: &Response) {
    let mut line = match serde_json::to_string(response) {
        Ok(l) => l,
        Err(_) => return,
    };
    line.push('\n');
    let _ = writer.write_all(line.as_bytes());
    let _ = writer.flush();
}
