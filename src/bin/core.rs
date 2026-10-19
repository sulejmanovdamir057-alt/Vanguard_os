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

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let metrics = Arc::clone(&metrics);
                let bg_tasks = Arc::clone(&bg_tasks);
                thread::spawn(move || handle_client(stream, metrics, bg_tasks));
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
            // ── Проводник «Файлы»: пути абсолютные, песочницы больше нет ──
            Request::FsList { path } => send(&mut writer, &fs_list(&path)),
            Request::FsRead { path } => send(&mut writer, &fs_read(&path)),
            Request::FsWrite { path, content } => {
                send(&mut writer, &fs_write(&path, &content))
            }
            Request::FsCreate { path } => send(&mut writer, &fs_create(&path)),
            Request::FsMkdir { path } => send(&mut writer, &fs_mkdir(&path)),
            Request::FsRename { from, to } => send(&mut writer, &fs_rename(&from, &to)),
            Request::FsDelete { path } => send(&mut writer, &fs_delete(&path)),
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

/// Резолвит абсолютный путь запроса в канонический.
///
/// Песочницы больше нет: проводник видит все диски машины, как настоящий
/// «Проводник Windows». Убрана она была не ради безопасности — папка проекта
/// в качестве корня просто не показывала компьютер. Остались две проверки, и
/// обе про корректность, а не про границы:
/// 1. компонент `..` отвергается **до** `canonicalize()`: канонизация идёт от
///    ближайшего существующего предка, и `..`, снятый по пути, остался бы в
///    результате текстом — путь получился бы несуществующий;
/// 2. путь обязан быть абсолютным. `C:файл` на Windows значит «относительно
///    текущего каталога диска C», то есть ядро, запущенное из другой папки,
///    ответило бы про другие файлы — та же ловушка CWD, что и у `/run`.
///
/// Несуществующий хвост — норма: и «новый файл», и `mkdir -p` с несколькими
/// новыми уровнями. Канонизируется ближайший существующий предок, а снятые
/// компоненты дописываются обратно; новая компонента ещё не может быть
/// симлинком, поэтому доверять ей можно.
fn resolve_path(input: &str) -> Result<PathBuf, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("путь не задан".to_string());
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err("путь должен быть абсолютным (например C:\\Windows)".to_string());
    }
    for part in path.components() {
        if matches!(part, std::path::Component::ParentDir) {
            return Err("путь не может содержать «..»".to_string());
        }
    }

    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&probe) {
            Ok(resolved) => {
                let mut resolved = resolved;
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
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
/// показа в строке пути и хлебных крошек проводника префикс срезаем — только
/// для показа: сравнивать и удалять надо по каноническому пути.
fn fs_pretty(path: &Path) -> String {
    let s = path.display().to_string();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// Дата изменения в секундах от Unix-эпохи; `0` — нет данных (или не смогли
/// прочитать). Время на диске локальное, поэтому конвертация — в `Local`.
fn fs_mtime(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Диски уровня «Этот компьютер»: `C:\`, `D:\` … `Z:\`, которые реально есть.
///
/// `A:` и `B:` пропускаются намеренно — на машине без дисковода обращение к
/// ним отвечает медленно и без всякой пользы. `Path::exists()` здесь спрашивает
/// `GetFileAttributes`, то есть это один системный вызов на букву.
fn fs_drives() -> Vec<FsEntry> {
    let mut out = Vec::new();
    for letter in b'C'..=b'Z' {
        let name = format!("{}:\\", letter as char);
        if Path::new(&name).exists() {
            out.push(FsEntry { name, is_dir: true, size: 0, mtime: 0 });
        }
    }
    out
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
///
/// Пустой путь — виртуальный уровень «Этот компьютер»: там нет каталога,
/// который можно прочитать, поэтому отдаются просто диски ([`fs_drives`]).
fn fs_list(path: &str) -> Response {
    if path.trim().trim_matches('\\').is_empty() {
        return fs_reply(Ok(FsListing { path: String::new(), entries: fs_drives() }));
    }
    let dir = match resolve_path(path) {
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
            .map(|e| {
                let m = e.metadata().ok();
                FsEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                    size: m.as_ref().map(|m| m.len()).unwrap_or(0),
                    mtime: m.as_ref().map(fs_mtime).unwrap_or(0),
                }
            })
            .collect(),
        Err(e) => return Response::err(format!("не удалось прочитать каталог: {}", e)),
    };
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    fs_reply(Ok(FsListing { path: fs_pretty(&dir), entries }))
}

/// Чтение файла. Бинарники отклоняются явно: `from_utf8` упал бы с ошибкой
/// кодировки, а hex-дамп в редакторе — это шум, а не проводник.
fn fs_read(path: &str) -> Response {
    let abs = match resolve_path(path) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    let meta = match std::fs::metadata(&abs) {
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
    let bytes = match std::fs::read(&abs) {
        Ok(b) => b,
        Err(e) => return Response::err(format!("не удалось прочитать: {}", e)),
    };
    match String::from_utf8(bytes) {
        Ok(content) => fs_reply(Ok(FsFile { path: fs_pretty(&abs), content })),
        Err(_) => Response::err("файл не в UTF-8 — бинарник, показывать нечего".to_string()),
    }
}

/// Запись файла: создаёт или перезаписывает целиком. Перезапись здесь ожидаема —
/// это путь SAVE; создание нового файла делает [`fs_create`].
fn fs_write(path: &str, content: &str) -> Response {
    let abs = match resolve_path(path) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    match std::fs::write(&abs, content) {
        Ok(()) => fs_reply(Ok(fs_pretty(&abs))),
        Err(e) => Response::err(format!("не удалось записать {}: {}", fs_pretty(&abs), e)),
    }
}

/// Создание нового пустого файла. Отдельный запрос, а не `fs_write` с пустым
/// содержимым: `fs::write` создаёт **или урезает существующий до нуля**, так что
/// кнопка «＋ Файл» молча обнуляла файл с занятым именем.
/// `create_new(true)` падает с `AlreadyExists` атомарно, поэтому устаревший
/// список каталога в оболочке не превращается в потерю данных.
fn fs_create(path: &str) -> Response {
    let abs = match resolve_path(path) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    let opts = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&abs);
    match opts {
        Ok(_) => fs_reply(Ok(fs_pretty(&abs))),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Response::err(format!("«{}» уже существует — имя занято.", fs_pretty(&abs)))
        }
        Err(e) => Response::err(format!("не удалось создать {}: {}", fs_pretty(&abs), e)),
    }
}

/// Создание каталога вместе со всеми родителями.
fn fs_mkdir(path: &str) -> Response {
    let abs = match resolve_path(path) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    match std::fs::create_dir_all(&abs) {
        Ok(()) => fs_reply(Ok(fs_pretty(&abs))),
        Err(e) => Response::err(format!("не удалось создать каталог: {}", e)),
    }
}

/// Переименование/перемещение.
///
/// На занятое имя не идёт: `std::fs::rename` заменяет цель молча («replacing
/// the original file if `to` already exists», на Win10 1607+ как на Unix), а
/// в проводнике это ошибка. `src == dst` — не «занято», а no-op.
fn fs_rename(from: &str, to: &str) -> Response {
    let src = match resolve_path(from) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    let dst = match resolve_path(to) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    // У корня диска `parent()` — None: переименовать `C:\` нельзя физически,
    // но лучше сказать об этом словами, чем отдать сырой io::Error.
    if src.parent().is_none() {
        return Response::err("корень диска переименовать нельзя".to_string());
    }
    if src == dst {
        // То же имя — переименовывать нечего; без этого ранний выход ниже
        // сработал бы как «имя занято» на собственное имя файла.
        return fs_reply(Ok(fs_pretty(&dst)));
    }
    if dst.exists() {
        return Response::err(format!("«{}» уже существует — имя занято.", fs_pretty(&dst)));
    }
    match std::fs::rename(&src, &dst) {
        Ok(()) => fs_reply(Ok(fs_pretty(&dst))),
        Err(e) => Response::err(format!("не удалось переименовать: {}", e)),
    }
}

/// Удаление: файл — как файл, директория — деревом целиком.
///
/// Корень диска под запретом: `remove_dir_all("C:\\")` здесь — не гипотетический
/// сценарий, а одна кнопка в списке, и остановился бы он только на паре
/// защищённых системных файлов.
fn fs_delete(path: &str) -> Response {
    let abs = match resolve_path(path) {
        Ok(p) => p,
        Err(e) => return Response::err(e),
    };
    if abs.parent().is_none() {
        return Response::err("корень диска удалить нельзя".to_string());
    }
    let result = if abs.is_dir() {
        std::fs::remove_dir_all(&abs)
    } else {
        std::fs::remove_file(&abs)
    };
    match result {
        Ok(()) => fs_reply(Ok(fs_pretty(&abs))),
        Err(e) => Response::err(format!("не удалось удалить {}: {}", fs_pretty(&abs), e)),
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
