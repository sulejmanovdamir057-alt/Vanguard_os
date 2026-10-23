use std::fs::{self, File};
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::thread;
use chrono::{DateTime, Local};
use rand::Rng;
use sysinfo::System;
use glob::glob;
use regex::Regex;

// Действия и события ИИ приходят из ядра (ai_core) — там же протокол VanguardAction.
use crate::ai_core::{AiEvent, VanguardAction};
use crate::core::executor::VanguardExecutor;

/// Сколько сообщений уходит в запрос. Это 12 реплик: замерено на этой машине, что 10 реплик
/// поднимают prompt_tokens с ~309 до ~1169 и стоят +0.4 с пребилда, дальше контекст дешёвым
/// уже не бывает, а для 9b-модели и этого окна хватает.
const MAX_HISTORY_MESSAGES: usize = 24;

/// Журнал диалогов для самообучения: каждая реплика дописывается сюда одной
/// JSON-строкой, а скрипт `trainer/vanguard_train.py` читает файл и дообучает
/// адаптер LoRA на собственных ответах модели. Best-effort запись: ошибка диска
/// не должна ронять диолог — обучение просто увидит меньше данных.
pub const DIALOGUE_LOG: &str = "vanguard_dialogues.jsonl";

/// Дописывает реплику в журнал диалогов (JSONL). Роль + текст + unix-время.
fn log_dialogue_line(role: &str, text: &str) {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // serde_json экранирует кавычки и переводы строк — руками ничего не надо.
    let line = serde_json::json!({ "role": role, "content": text, "ts": ts });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(DIALOGUE_LOG)
    {
        let _ = writeln!(f, "{}", line);
    }
}

/// Сколько реплик лежит в журнале — статистика для окна Neural Trainer.
/// `lines()` считает и пустые, и битые строки: для счётчика это не важно.
pub fn count_dialogue_lines() -> usize {
    use std::io::{BufRead, BufReader};
    match std::fs::File::open(DIALOGUE_LOG) {
        Ok(f) => BufReader::new(f).lines().count(),
        Err(_) => 0,
    }
}

/// Имя файла `vanguard_gen_553.py` -> Some("553"), всё остальное -> None.
/// Префикс `vanguard_gen_` — 13 байт, расширение `.py` — 3. Один разбор на все
/// места, где показывается ID: `/apps` в шелле и окна Лаунчера/Проводника в app.rs.
pub fn script_id_from_path(path: &str) -> Option<String> {
    let name = std::path::Path::new(path).file_name()?.to_string_lossy().into_owned();
    if name.len() > 16 { Some(name[13..name.len() - 3].to_string()) } else { None }
}

#[derive(Default)]
pub struct VanguardAI {
    /// Диалог, который уходит в каждый запрос: пары (role, content).
    /// Раньше этого поля не было вовсе — запрос состоял из [system, user], и модель
    /// не помнила предыдущих реплик.
    pub messages: Vec<(String, String)>,
}

impl VanguardAI {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn clear_memory(&mut self) {
        // Сбрасываем диалог — на это завязаны /reset и Ctrl+L в app.rs
        self.messages.clear();
    }

    /// Реплика пользователя. Запоминается до отправки, чтобы модель увидела её в запросе.
    pub fn push_user(&mut self, text: &str) {
        log_dialogue_line("user", text);
        self.messages.push(("user".to_string(), text.to_string()));
        self.trim_history();
    }

    /// Ответ модели. Без неё история состояла бы из одних вопросов, и модель не помнила бы,
    /// что сама ответила.
    pub fn push_assistant(&mut self, text: &str) {
        log_dialogue_line("assistant", text);
        self.messages.push(("assistant".to_string(), text.to_string()));
        self.trim_history();
    }

    fn trim_history(&mut self) {
        let excess = self.messages.len().saturating_sub(MAX_HISTORY_MESSAGES);
        if excess > 0 {
            self.messages.drain(..excess);
        }
    }

    pub fn save_code_if_exists(&mut self, full_text: &str) -> Option<String> {
        let re = Regex::new(r"(?s)```python\n(.*?)\n```").unwrap();

        if let Some(captures) = re.captures(full_text) {
            if let Some(code_block) = captures.get(1) {
                let gen_id = rand::thread_rng().gen_range(100..1000);
                let filename = format!("vanguard_gen_{}.py", gen_id);

                if let Ok(mut file) = File::create(&filename) {
                    if file.write_all(code_block.as_str().as_bytes()).is_ok() {
                        return Some(filename);
                    }
                }
            }
        }
        None
    }
}

/// Разрешение путей терминала: относительный путь — от каталога шелла, пустой
/// аргумент — сам каталог. Тюльпан (`~`) не поддерживаем: в Windows его нет.
fn term_resolve(cwd: &str, arg: &str) -> String {
    let arg = arg.trim();
    if arg.is_empty() {
        return if cwd.is_empty() { String::from(".") } else { cwd.to_string() };
    }
    let p = std::path::Path::new(arg);
    if p.is_absolute() || cwd.is_empty() {
        arg.to_string()
    } else {
        format!("{}\\{}", cwd, arg)
    }
}

/// `dir [путь]`: список каталога в духе Windows — каталоги первыми, потом файлы,
/// по дате и размеру. Больше 300 записей не печатаем: чат — не `less`.
fn term_dir(cwd: &str, arg: &str) -> String {
    let path = term_resolve(cwd, arg);
    let rd = match fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => return format!("vsh: не удалось открыть {}: {}\n", path, e),
    };
    let mut dirs: Vec<(String, String)> = Vec::new(); // (дата, имя)
    let mut files: Vec<(String, String, u64)> = Vec::new(); // (дата, имя, байты)
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let meta = e.metadata().ok();
        let date = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(|st| {
                let dt: DateTime<Local> = st.into();
                dt.format("%Y-%m-%d %H:%M").to_string()
            })
            .unwrap_or_default();
        match meta {
            Some(m) if m.is_dir() => dirs.push((date, name)),
            Some(m) => files.push((date, name, m.len())),
            None => files.push((date, name, 0)),
        }
    }
    dirs.sort_by_key(|d| d.1.to_lowercase());
    files.sort_by_key(|f| f.1.to_lowercase());
    let (n_dirs, n_files) = (dirs.len(), files.len());
    let mut lines: Vec<String> = dirs
        .iter()
        .map(|(d, n)| format!("{}  {:<8}  {}", d, "<DIR>", n))
        .chain(files.iter().map(|(d, n, s)| format!("{}  {:>12}  {}", d, s, n)))
        .collect();
    let total = lines.len();
    let mut out = format!("\n{}\n\n", path);
    for l in lines.drain(..).take(300) {
        out.push_str(&l);
        out.push('\n');
    }
    if total > 300 {
        out.push_str(&format!("... и ещё {} записей\n", total - 300));
    }
    out.push_str(&format!("\nДиректорий: {}, файлов: {}\n", n_dirs, n_files));
    out
}

/// `cd [путь]`: без аргумента — в домашний каталог пользователя. Каталог должен
/// существовать и быть каталогом; путь нормализуется (хвостовой слэш срезается,
/// кроме корня диска `C:\`).
fn term_cd(cwd: &str, arg: &str) -> Result<String, String> {
    let target = if arg.trim().is_empty() {
        std::env::var("USERPROFILE").unwrap_or_else(|_| term_resolve(cwd, ""))
    } else {
        term_resolve(cwd, arg)
    };
    match fs::metadata(&target) {
        Ok(m) if m.is_dir() => {
            let mut s = target.trim_end_matches('\\').to_string();
            if s.len() == 2 && s.as_bytes()[1] == b':' {
                s.push('\\');
            }
            Ok(s)
        }
        Ok(_) => Err(format!("vsh: {} — это файл, а не каталог\n", target)),
        Err(e) => Err(format!("vsh: не удалось открыть {}: {}\n", target, e)),
    }
}

/// `type <файл>`: печатает файл, как в cmd. Обрезаем на 64 КиБ (читаем целиком
/// в буфер) и отказываемся от не-UTF-8 — бинарник в ленте чата не нужен.
fn term_type(cwd: &str, arg: &str) -> String {
    let name = arg.trim();
    if name.is_empty() {
        return String::from("vsh: укажите файл: type <имя>\n");
    }
    let path = term_resolve(cwd, name);
    let mut f = match File::open(&path) {
        Ok(f) => f,
        Err(e) => return format!("vsh: не удалось открыть {}: {}\n", path, e),
    };
    use std::io::Read;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut over = false;
    loop {
        match f.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() + n > 65536 {
                    over = true;
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
    match String::from_utf8(buf) {
        Ok(text) => {
            let mut out = format!("\n{}\n\n", path);
            out.push_str(&text);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            if over {
                out.push_str("\n... (файл обрезан на 64 КиБ)\n");
            }
            out
        }
        Err(_) => format!("vsh: {} — не текст (не UTF-8)\n", path),
    }
}

#[derive(Debug, PartialEq)]
pub enum ShellAction {
    SystemExit,
    DisplayOutput(String),
    AiStreamAllowed,
    /// Терминальный `cls`: чат живёт в GUI, поэтому действие описано здесь,
    /// а исполнение — в app.rs (тот же путь, что Ctrl+L и меню стола).
    ClearChat,
    /// `start <цель>`: открыть URL/файл/папку средствами системы. Разбор тут,
    /// запуск — в app.rs: там живёт `sys_open` и будет реестр приложений.
    OpenPath(String),
}

pub struct VanguardShell {
    pub ai_core: VanguardAI,
    // Логи фоновых задач (/run) уходят через этот канал прямо в GUI-чат,
    // минуя консоль. None — печатаем в stdout как раньше.
    pub log_tx: Option<Sender<AiEvent>>,
    // Счётчик незавершённых фоновых задач: UI держит кадр живым, пока он > 0,
    // иначе egui уснёт и лог задачи не дорисуется.
    pub bg_tasks: Arc<AtomicUsize>,
    /// Текущий каталог терминала (`dir`/`cd`/`type` читают отсюда). Пусто — CWD
    /// процесса. Своя у каждого шелла: у ядра он на каждое соединение свой.
    pub term_cwd: String,
}

impl Default for VanguardShell {
    fn default() -> Self {
        Self {
            ai_core: VanguardAI::new(),
            log_tx: None,
            // В точности прежний счётчик: Arc<AtomicUsize>::default() — это
            // Arc::new(AtomicUsize::new(0)).
            bg_tasks: Arc::new(AtomicUsize::new(0)),
            term_cwd: String::new(),
        }
    }
}

impl VanguardShell {
    pub fn new() -> Self {
        Self::default()
    }

    /// Строка приглашения для GUI-терминала: тот же вид, что раньше был
    /// захардкожен в app.rs, только с текущим каталогом вместо «~».
    pub fn term_prompt(&self) -> String {
        let here = if self.term_cwd.is_empty() {
            String::from("~")
        } else {
            self.term_cwd.clone()
        };
        format!("vanguard_root@kernel_sh:{}#", here)
    }

    /// Команды без слэша — «настоящий» терминал рядом со слеш-командами ядра.
    /// Возвращает `None`, если текст принадлежит модели: вызывающий в этом
    /// случае пускает его в ИИ-стрим, как и раньше.
    fn term_bare_command(&mut self, parts: &[&str], trimmed: &str) -> Option<ShellAction> {
        let cmd = parts[0].to_ascii_lowercase();
        let cwd = self.term_cwd.clone();
        let arg = if parts.len() > 1 {
            parts[1..].join(" ")
        } else {
            String::new()
        };
        let action = match cmd.as_str() {
            "help" | "?" => ShellAction::DisplayOutput(String::from(
                ">> ТЕРМИНАЛ: help, dir [путь], cd [путь], type <файл>, cls, echo <текст>, start <цель>\n\
                 >> СИСТЕМА: ipconfig, tasklist, systeminfo, ver, whoami, hostname\n\
                 >> ЯДРО: /help, /apps, /run <id>, /hardware, /wipe, /reset, /exit\n",
            )),
            "dir" | "ls" => ShellAction::DisplayOutput(term_dir(&cwd, &arg)),
            "cd" => match term_cd(&cwd, &arg) {
                Ok(dir) => {
                    self.term_cwd = dir.clone();
                    ShellAction::DisplayOutput(format!("{}\n", dir))
                }
                Err(e) => ShellAction::DisplayOutput(e),
            },
            "type" | "cat" => ShellAction::DisplayOutput(term_type(&cwd, &arg)),
            "cls" | "clear" => ShellAction::ClearChat,
            // echo печатает всё после ключевого слова — как в cmd; длину берём
            // у lowercased-копии, она побайтово равна исходному токену.
            "echo" => {
                let text = trimmed[cmd.len()..].trim_start();
                ShellAction::DisplayOutput(format!("{}\n", text))
            }
            "start" => {
                if arg.is_empty() {
                    ShellAction::DisplayOutput(String::from(
                        "vsh: укажите цель: start <файл|папка|URL|приложение>\n",
                    ))
                } else {
                    ShellAction::OpenPath(arg)
                }
            }
            "ipconfig" | "tasklist" | "systeminfo" | "ver" | "whoami" | "hostname" => {
                self.term_run_tool(parts);
                ShellAction::DisplayOutput(format!(
                    ">> {} — выполняется в фоне, вывод придёт в чат…\n",
                    cmd
                ))
            }
            _ => return None,
        };
        Some(action)
    }

    /// Фоновый запуск системной утилиты: вывод придёт токеном в чат, как у
    /// `/run`. Отдельный процесс, а не реализация на Rust — нужен именно тот
    /// вывод, что видит пользователь в настоящей Windows. `ver` — встроенная
    /// команда cmd (отдельного exe у неё нет), остальные — настоящие утилиты.
    fn term_run_tool(&mut self, parts: &[&str]) {
        let tool = parts[0].to_ascii_lowercase();
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        let log_tx = self.log_tx.clone();
        let bg_tasks = Arc::clone(&self.bg_tasks);
        bg_tasks.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            let result = if tool == "ver" {
                Command::new("cmd").args(["/c", "ver"]).output()
            } else {
                Command::new(&tool).args(&args).output()
            };
            let payload = match result {
                Ok(o) => {
                    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                    let e = String::from_utf8_lossy(&o.stderr);
                    if !e.trim().is_empty() {
                        s.push_str(&e);
                    }
                    s
                }
                Err(e) => format!("vsh: не удалось запустить {}: {}\n", tool, e),
            };
            match log_tx {
                Some(tx) => {
                    let _ = tx.send(AiEvent::Token(payload));
                }
                None => println!("{}", payload),
            }
            bg_tasks.fetch_sub(1, Ordering::SeqCst);
        });
    }

    pub fn handle_command(&mut self, query: &str) -> ShellAction {
        let trimmed = query.trim();
        let cmd_lower = trimmed.to_lowercase();
        let parts: Vec<&str> = trimmed.split_whitespace().collect();

        if parts.is_empty() {
            return ShellAction::DisplayOutput(String::new());
        }

        match parts[0].to_lowercase().as_str() {
            "/exit" | "/shutdown" | "выход" => ShellAction::SystemExit,
            "/wipe" => {
                let mut log = String::from(">> INITIATING STORAGE PURGE PROTOCOL...\n");
                let mut deleted_count = 0;
                if let Ok(entries) = glob("vanguard_gen_*.py") {
                    for entry in entries.flatten() {
                        if fs::remove_file(&entry).is_ok() {
                            log.push_str(&format!(">> PURGED: {}\n", entry.display()));
                            deleted_count += 1;
                        }
                    }
                }
                log.push_str(&format!("\n>> PURGE COMPLETE. TOTAL FILES DESTROYED: {}\n", deleted_count));
                ShellAction::DisplayOutput(log)
            }
            "/reset" => {
                self.ai_core.clear_memory();
                ShellAction::DisplayOutput(String::from(">> WIPING DISPLAY BUFFER...\n>> PREVIOUS NEURAL CONTEXT DROPPED.\n"))
            }
            "/run" => {
                if parts.len() < 2 {
                    return ShellAction::DisplayOutput(">> ERROR: Specify file ID. Example: /run 553\n".to_string());
                }
                let file_id = parts[1..].join(" ");
                let file_id_clone = file_id.clone();
                let log_tx = self.log_tx.clone();
                let bg_tasks = Arc::clone(&self.bg_tasks);
                bg_tasks.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    let execution_log = VanguardExecutor::execute_script(&file_id_clone);
                    let payload = format!("\n{}\n", execution_log);
                    match log_tx {
                        Some(tx) => { let _ = tx.send(AiEvent::Token(payload)); }
                        None => println!("{}", payload),
                    }
                    bg_tasks.fetch_sub(1, Ordering::SeqCst);
                });
                ShellAction::DisplayOutput(format!(">> INITIATING BACKGROUND TASK FOR APP ID: {}...\n", file_id))
            }
            "/apps" => {
                let mut log = "=== VANGUARD APPLICATION REGISTRY ===\n".to_string();
                let mut found = false;
                if let Ok(entries) = glob("vanguard_gen_*.py") {
                    for entry in entries.flatten() {
                        found = true;
                        let file_size = fs::metadata(&entry).map(|m| m.len() as f64 / 1024.0).unwrap_or(0.0);
                        let file_name = entry.to_string_lossy();
                        let id = script_id_from_path(&file_name).unwrap_or_else(|| "???".to_string());
                        log.push_str(&format!("-> [ID: {}] | FILE: {} | SIZE: {:.2} KB\n", id, file_name, file_size));
                    }
                }
                if !found { return ShellAction::DisplayOutput(">> REGISTRY EMPTY.\n".to_string()); }
                ShellAction::DisplayOutput(log)
            }
            "/hardware" => {
                let mut sys = System::new_all();
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                ShellAction::DisplayOutput(format!(">> CPU THREADS: {}\n>> RAM CAPACITY: {:.2} GB Total\n", sys.cpus().len(), sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0))
            }
            "/help" => ShellAction::DisplayOutput(String::from(">> COMMANDS: /help, /apps, /run <id>, /hardware, /wipe, /reset, /exit\n")),
            _ => {
                if cmd_lower.starts_with('/') {
                    ShellAction::DisplayOutput(format!(">> vsh: command not found: {}\n", cmd_lower))
                } else if let Some(action) = self.term_bare_command(&parts, trimmed) {
                    action
                } else {
                    ShellAction::AiStreamAllowed
                }
            }
        }
    }

    pub fn execute_ai_action(&mut self, action: VanguardAction) -> String {
        match action {
            VanguardAction::TextResponse { text } => {
                format!("V: {}\n", text)
            }
            VanguardAction::SystemCommand { command } => {
                format!(">> ИИ инициировал системную команду: /{}.\n", command)
            }
            VanguardAction::CreateScript { language, code, description } => {
                let gen_id = rand::thread_rng().gen_range(100..1000);
                match language.to_lowercase().as_str() {
                    "python" => {
                        let filename = format!("vanguard_gen_{}.py", gen_id);
                        // Без unwrap: execute_ai_action зовётся из check_ai_stream прямо в
                        // UI-потоке, и паника здесь убила бы всё приложение целиком.
                        if let Err(e) = fs::write(&filename, &code) {
                            return format!(">> [ИИ] {}\n>> ОШИБКА ЗАПИСИ {}: {}\n", description, filename, e);
                        }
                        format!(">> [ИИ] {}\n>> Создан Python-скрипт ID: {}. Запуск через /run {}\n", description, gen_id, gen_id)
                    }
                    "rust" => {
                        let source_file = format!("vanguard_tmp_{}.rs", gen_id);
                        let binary_file = format!("vanguard_app_{}.exe", gen_id);
                        if let Err(e) = fs::write(&source_file, &code) {
                            return format!(">> [ИИ] {}\n>> ОШИБКА ЗАПИСИ исходника {}: {}\n", description, source_file, e);
                        }

                        // .output(), а не .status(): первый наследует stderr в консоль, а консоли
                        // у GUI-приложения нет — rustc писал бы точную причину туда, где её
                        // никто не увидит. Ловим stderr и кладём в чат, иначе сбой
                        // компиляции полностью не диагностируется.
                        let output = Command::new("rustc")
                            .args(["-O", &source_file, "-o", &binary_file])
                            .output();

                        match output {
                            Ok(out) if out.status.success() => {
                                let _ = fs::remove_file(&source_file);
                                format!(">> [ИИ] {}\n>> УСПЕШНАЯ КОМПИЛЯЦИЯ ЯДРА!\n>> Бинарник готов: {}.\n", description, binary_file)
                            }
                            Ok(out) => {
                                // Исходник при сбое НЕ удаляем: он и есть артефакт для разбора.
                                // Плюс хвост stderr rustc — первые строки обычно содержат саму
                                // ошибку, дальше идут подсказки компилятора.
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
                                format!(">> СБОЙ КОМПИЛЯЦИИ: rustc не запустился: {}.\n", e)
                            }
                        }
                    }
                    "bash" | "sh" | "cmd" => {
                        let ext = if cfg!(target_os = "windows") { "bat" } else { "sh" };
                        let filename = format!("vanguard_script_{}.{}", gen_id, ext);
                        if let Err(e) = fs::write(&filename, &code) {
                            return format!(">> [ИИ] {}\n>> ОШИБКА ЗАПИСИ {}: {}\n", description, filename, e);
                        }
                        #[cfg(unix)] {
                            let _ = Command::new("chmod").args(&["+x", &filename]).status();
                        }
                        format!(">> [ИИ] {}\n>> Создан сценарий автоматизации: {}.\n", description, filename)
                    }
                    _ => format!(">> Ошибка: Язык [ {} ] не поддерживается ядром.\n", language)
                }
            }
        }
    }
}