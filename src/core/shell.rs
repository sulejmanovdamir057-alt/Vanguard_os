use std::fs::{self, File};
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::thread;
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

pub struct VanguardAI {
    /// Диалог, который уходит в каждый запрос: пары (role, content).
    /// Раньше этого поля не было вовсе — запрос состоял из [system, user], и модель
    /// не помнила предыдущих реплик.
    pub messages: Vec<(String, String)>,
}

impl VanguardAI {
    pub fn new() -> Self {
        Self { messages: Vec::new() }
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

#[derive(Debug, PartialEq)]
pub enum ShellAction {
    SystemExit,
    DisplayOutput(String),
    AiStreamAllowed,
}

pub struct VanguardShell {
    pub ai_core: VanguardAI,
    // Логи фоновых задач (/run) уходят через этот канал прямо в GUI-чат,
    // минуя консоль. None — печатаем в stdout как раньше.
    pub log_tx: Option<Sender<AiEvent>>,
    // Счётчик незавершённых фоновых задач: UI держит кадр живым, пока он > 0,
    // иначе egui уснёт и лог задачи не дорисуется.
    pub bg_tasks: Arc<AtomicUsize>,
}

impl VanguardShell {
    pub fn new() -> Self {
        Self {
            ai_core: VanguardAI::new(),
            log_tx: None,
            bg_tasks: Arc::new(AtomicUsize::new(0)),
        }
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