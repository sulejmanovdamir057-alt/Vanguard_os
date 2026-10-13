//! Протокол IPC ядра Vanguard: JSON-строки поверх TCP на локалхосте.
//!
//! # Почему JSON-строки поверх TCP
//! Ядро — отдельный процесс. Оболочка и ИИ-помощник шлют ему запросы и слушают
//! события. TCP на `127.0.0.1` выбран вместо named pipe потому, что его
//! отлаживается любым `telnet 127.0.0.1 7331`, а события (стриминг ИИ, логи
//! `/run`) едут вдоль того же открытого соединения — по одной JSON-строке на
//! событие, пока соединение живо. Ограничения на длину потока нет.
//!
//! # Формат
//! Каждая строка — один JSON-объект. Клиент шлёт [`Request`], ядро отвечает
//! потоком [`Response`]. События потока помечены
//! `kind = "event"` и имеют тот же `id`, что породивший их запрос — так клиент
//! не спутает параллельные потоки.

use serde::{Deserialize, Serialize};

/// Порт IPC-сервера ядра. Подобран так, чтобы не резервировался Windows'ом
/// (низкие (<1024) и типовые 8080/3000 стараемся не трогать).
pub const CORE_PORT: u16 = 7331;

/// Адрес ядра по умолчанию. Переопределяется переменной `VANGUARD_CORE_ADDR`
/// (например, для отладки ядра на другой машине — но IPC принципиально локальный).
pub const CORE_ADDR: &str = "127.0.0.1";

/// Запрос к ядру. Одна строка JSON на соединение.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Слеш-команда шелла (`/apps`, `/hardware`, `/wipe`, `/help`, `/reset`).
    /// `/run` тоже принимается, но для потока логов удобнее [`Request::RunScript`].
    Shell { query: String },
    /// Запустить скрипт `<id>` фоном; логи придут потоком [`Event::TaskLog`].
    RunScript { id: String },
    /// Скомпилировать Rust-код, который прислал ИИ (тяжёлая операция — едет
    /// в ядро, чтобы не замораживать рендер оболочки). Ответ — одним
    /// [`Response::Output`] со строкой результата, как в `execute_ai_action`.
    CompileRust {
        code: String,
        description: String,
        gen_id: u32,
    },
    /// Сохранить Python-код, присланный ИИ, в `vanguard_gen_<id>.py`.
    SaveScript {
        code: String,
        description: String,
        gen_id: u32,
    },
    /// Прочитать исходник скрипта (для проводника скриптов).
    ReadScript { id: String },
    /// Список сгенерированных скриптов (id, имя, размер) — для окон Лаунчера/Проводника.
    ListScripts,
    /// Счётчик реплик в журнале диалогов (окно Neural Trainer).
    DialogueCount,
    /// Пустой heartbeat: ядро живо, если отвечает `{"kind":"ok"}`.
    Ping,
}

/// Ответ ядра. Первые три строки — результат запроса, остальные — события потока.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Конечный результат запроса: текст, вывод команды, результат компиляции.
    Ok {
        /// Тот же `id`, если клиент его передал (для сопоставления запросов).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        text: String,
    },
    /// Ядру не удалось выполнить запрос (файл не найден, `rustc` не запустился…).
    Err {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        text: String,
    },
    /// Событие живого потока: строка лога фоновой задачи.
    Event {
        id: u64,
        #[serde(flatten)]
        event: Event,
    },
    /// Heartbeat на [`Request::Ping`].
    Pong,
}

/// События потока. Живут внутри [`Response::Event`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Очередная строка вывода фоновой задачи (`/run`).
    TaskLog { line: String },
}

impl Response {
    /// Удобный конструктор успешного ответа без id.
    pub fn ok(text: impl Into<String>) -> Self {
        Response::Ok { id: None, text: text.into() }
    }

    /// Ответ с явным id (для сопоставления с запросом клиента).
    pub fn ok_id(id: u64, text: impl Into<String>) -> Self {
        Response::Ok { id: Some(id), text: text.into() }
    }

    /// Удобный конструктор ошибки без id.
    pub fn err(text: impl Into<String>) -> Self {
        Response::Err { id: None, text: text.into() }
    }
}

/// Код порта по умолчанию нужен и ядру (слушать), и клиентам (подключаться).
/// Симметрия гарантируется общим [`CORE_PORT`] — здесь просто экспорт для
/// краткости в `core::main`.
pub fn core_addr() -> String {
    format!("{}:{}", CORE_ADDR, CORE_PORT)
}
