//! Настройки оболочки: файл на диске, применение через env и запуск демона Ollama.
//!
//! Окно «Настройки» (`app.rs::draw_settings_window`) редактирует [`AppSettings`];
//! файл лежит рядом с остальными артефактами `vanguard_*` и закрыт в `.gitignore`.
//!
//! Применение = `std::env::set_var`, потому что `ai_core::spawn_ai_request` читает
//! окружение **на каждый запрос** (а не один раз при старте). Поэтому правка в окне
//! действует немедленно, без перезапуска — это и есть весь трюк.
//!
//! Вторая половина модуля — поднятие `ollama serve`: на этой машине Ollama не в
//! автозагрузке, и без него любой запрос ИИ падает с `os error 10061`
//! («конечный компьютер отверг запрос»). Раньше это ловилось только руками.

use std::path::PathBuf;
use std::time::Duration;

/// Файл настроек. Пишется в ту же рабочую папку, что и заметки/скрипты,
/// то есть относительно CWD — как и всё остальное в проекте.
pub const SETTINGS_FILE: &str = "vanguard_settings.json";

/// Порт, на котором `ollama serve` слушает по умолчанию.
const OLLAMA_PORT: u16 = 11434;

/// Настройки, которые правятся в одноимённом окне.
///
/// Пустая строка у `model`/`reasoning_effort`/`keep_alive` на диске означает
/// «файла ещё не было» — [`AppSettings::load`] подставит туда значение из
/// окружения, чтобы окно показывало реальную конфигурацию, а не пустоту.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AppSettings {
    /// Поднимать `ollama serve` при старте оболочки.
    #[serde(default)]
    pub ollama_autostart: bool,
    /// Тег модели Ollama, например `qwen3.5:9b`.
    #[serde(default)]
    pub model: String,
    /// `none` | `low` | `medium` | `high`. `none` — мышление выключено;
    /// пустое значение недопустимо, иначе гибридная модель уйдёт в раздумья
    /// на десятки секунд (см. замер в AGENTS.md).
    #[serde(default)]
    pub reasoning_effort: String,
    /// Сколько держать модель в VRAM: `-1` — не выгружать никогда.
    #[serde(default)]
    pub keep_alive: String,
}

impl AppSettings {
    /// Читает файл; отсутствие или битый JSON — не ошибка, а «настройки по умолчанию».
    pub fn load() -> Self {
        let mut s = std::fs::read_to_string(SETTINGS_FILE)
            .ok()
            .and_then(|text| serde_json::from_str::<AppSettings>(&text).ok())
            .unwrap_or_default();

        // Пустые поля добиваем из окружения: так окно показывает то, что реально
        // отправится в Ollama, а env остаётся источником правды до первого сохранения.
        if s.model.is_empty() {
            s.model = std::env::var("VANGUARD_AI_MODEL").unwrap_or_else(|_| String::from("qwen3.5:9b"));
        }
        if s.reasoning_effort.is_empty() {
            s.reasoning_effort =
                std::env::var("VANGUARD_AI_REASONING_EFFORT").unwrap_or_else(|_| String::from("none"));
        }
        if s.keep_alive.is_empty() {
            s.keep_alive = std::env::var("VANGUARD_AI_KEEP_ALIVE").unwrap_or_else(|_| String::from("30m"));
        }
        s
    }

    /// Пишет файл. Ошибка возвращается наверх — в GUI-чат, не в консоль
    /// (в оболочке `println!` запрещён, прогресс виден только в чате).
    pub fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(SETTINGS_FILE, text).map_err(|e| format!("{} ({})", e, SETTINGS_FILE))
    }

    /// Кладёт настройки в окружение процесса — это и есть их применение.
    /// Вызывается и при старте, и при каждой правке в окне.
    ///
    /// Гонка с фоновым запросом теоретически возможна (окружение — глобальное),
    /// но `spawn_ai_request` читает env один раз, в первые микросекунды потока,
    /// до какой-либо сети — так что правка в окне либо успевает, либо нет, но
    /// полузаписанного значения модель не увидит.
    pub fn apply_to_env(&self) {
        std::env::set_var("VANGUARD_AI_MODEL", &self.model);
        std::env::set_var("VANGUARD_AI_REASONING_EFFORT", &self.reasoning_effort);
        std::env::set_var("VANGUARD_AI_KEEP_ALIVE", &self.keep_alive);
    }
}

/// Ищет `ollama.exe`: сначала стандартный `%LOCALAPPDATA%\Programs\Ollama`,
/// затем PATH (там он бывает, если ставили иначе).
pub fn ollama_exe() -> Option<PathBuf> {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(local)
            .join("Programs")
            .join("Ollama")
            .join("ollama.exe");
        if p.exists() {
            return Some(p);
        }
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let p = dir.join("ollama.exe");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Слушает ли кто-нибудь локальный порт Ollama.
///
/// **Разовый** вызов (старт приложения, кнопка в окне настроек) — не в кадре:
/// egui-цикл не место для сети, даже для loopback (так уже горел пробник бэкенда,
/// его поэтому вынесли в отдельный поток).
pub fn ollama_listening() -> bool {
    let addr: std::net::SocketAddr = match format!("127.0.0.1:{}", OLLAMA_PORT).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

/// Запускает `ollama serve` фоном: без окна консоли, stdin/stdout/stderr в никуда.
///
/// Путь к `ollama.exe` на этой машине содержит кириллицу, но `Command` на Windows
/// идёт через `CreateProcessW`, поэтому 8.3-трюк (он нужен PowerShell'ному
/// `Start-Process`, который ломается на не-ASCII путях) здесь не требуется.
///
/// Если демон уже жив, этот вызов просто вернёт `Ok` и дочерний процесс упадёт
/// на захвате порта — сверять стоит [`ollama_listening`], а не результат спауна.
pub fn spawn_ollama() -> Result<(), String> {
    let exe = ollama_exe().ok_or_else(|| {
        String::from("ollama.exe не найден: ни в %LOCALAPPDATA%\\Programs\\Ollama, ни в PATH")
    })?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // create_no_window: служебный демон, отдельное консольное окно не нужно —
    // оно бы мигнуло поверх того, что делает пользователь.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{}: {}", exe.display(), e)),
    }
}
