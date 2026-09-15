use std::fs::{self, File};
use std::io::Write;
use std::thread;
use glob::glob;
use regex::Regex;
use rand::Rng;
use sysinfo::System;

use crate::executor::VanguardExecutor;

pub struct VanguardAI {
    pub history: String,
}

impl VanguardAI {
    pub fn new() -> Self {
        Self { history: String::from("System: Инициализирован встроенный ИИ-терминал.\n") }
    }
    pub fn clear_memory(&mut self) {
        self.history = String::from("System: Инициализирован встроенный ИИ-терминал.\n");
    }
    pub fn save_code_if_exists(&mut self, full_text: &str) -> Option<String> {
        self.history.push_str(&format!("{}\n", full_text));
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
}

impl VanguardShell {
    pub fn new() -> Self {
        Self { ai_core: VanguardAI::new() }
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
                thread::spawn(move || {
                    let execution_log = VanguardExecutor::execute_script(&file_id_clone);
                    println!("\n{}\n", execution_log);
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
                        let id = if file_name.len() > 16 { &file_name[13..file_name.len() - 3] } else { "???" };
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
}