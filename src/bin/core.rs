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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use vanguard_os_lib::core::executor::VanguardExecutor;
use vanguard_os_lib::core::shell::VanguardShell;
use vanguard_os_lib::core::telemetry::{spawn_telemetry_daemon, SystemMetrics};
use vanguard_os_lib::ipc::{self, Event, Request, Response};

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
fn handle_client(stream: TcpStream, metrics: Arc<std::sync::Mutex<SystemMetrics>>, bg_tasks: Arc<AtomicUsize>) {
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
