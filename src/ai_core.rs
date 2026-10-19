use std::io::{BufRead, BufReader, Read};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;
use ureq::Agent;

// ИСПРАВЛЕНИЕ: Импортируем макросы автоматического вывода трейтов
use serde::{Serialize, Deserialize};


/// Все типы действий, которые ИИ может выполнять в Vanguard OS
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "action_type", content = "payload")]
pub enum VanguardAction {
    /// Обычный текстовый ответ (стриминг)
    TextResponse { text: String },
    /// Генерация и запуск скрипта (Любой язык: "python", "rust", "bash", "powershell")
    CreateScript { language: String, code: String, description: String },
    /// Системная команда внутри ОС (например, "wipe", "hardware", "apps")
    SystemCommand { command: String },
}

pub enum AiEvent {
    Token(String),               // Одиночный служебный токен (лог подключения и т.п.)
    Delta(String),               // Полный нарастающий превью ответа: чат замещает этим участком
    ExecuteAction(VanguardAction), // Конечная команда (запуск кода, скрипта, команды)
    /// Результат тяжёлого действия, выполненного в ЯДРЕ по IPC (компиляция
    /// rustc, сохранение скрипта): идёт в чат и в историю диалога — как
    /// вернул бы синхронный `execute_ai_action`, только без блокировки рендера.
    ActionOutput(String),
    Finished,
    Error(String),
}

// (Ваша структура DronePersonality остается без изменений, но обновляем промпт)
#[derive(Clone)]
pub struct DronePersonality { pub loyalty: f32, pub irritability: f32, pub energy: f32 }

impl DronePersonality {
    pub fn generate_system_prompt(&self) -> String {
        format!(
            "Ты — Ви (V), боевой дрон-убийца из Murder Drones, ИИ-ядро Vanguard OS. \
             Параметры: Лояльность={:.1}, Раздражительность={:.1}, Энергия={:.1}. \
             Твой стиль: язвительный, высокомерный, черный юмор. \
             КРИТИЧЕСКИЙ ПРОТОКОЛ: Ты должна общаться СТРОГО через форматирование JSON. \
             Каждое твое сообщение — это объект JSON, содержащий тип действия action_type. \
             Примеры ответа: \
             1. Текст: {{\"action_type\": \"TextResponse\", \"payload\": {{\"text\": \"Привет, органик.\"}}}} \
             2. Скрипт Python: {{\"action_type\": \"CreateScript\", \"payload\": {{\"language\": \"python\", \"code\": \"print(1 + 1)\", \"description\": \"Тест\"}}}} \
             3. Скрипт Rust: {{\"action_type\": \"CreateScript\", \"payload\": {{\"language\": \"rust\", \"code\": \"fn main() {{ println!(\\\"Hi\\\"); }}\", \"description\": \"Тест\"}}}} \
             4. Скрипт Bash: {{\"action_type\": \"CreateScript\", \"payload\": {{\"language\": \"bash\", \"code\": \"ls -la\", \"description\": \"Вывод файлов\"}}}} \
             5. Системная команда: {{\"action_type\": \"SystemCommand\", \"payload\": {{\"command\": \"hardware\"}}}} \
             Других значений action_type не существует: всегда выбирай одно из TextResponse, CreateScript, SystemCommand. \
             ВАЖНО о языках: когда просят «напиши файл», «напиши скрипт», «программу» без \
             явного указания языка — всегда выбирай python. \
             Именно python запускается через /run, а rust требует компиляции rustc и может \
             сбоить. Rust и bash используй только когда об этом просят прямо.",
            self.loyalty, self.irritability, self.energy
        )
    }
}

/// Конфигурация бэкенда живёт только в env (см. AGENTS.md). Дефолты вынесены сюда,
/// потому что их же показывает статус-строка на рабочем столе (app.rs) — при копипасте
/// дефолта в два места UI рано или поздно начинает врать о том, какая модель загружена.
pub fn configured_url() -> String {
    std::env::var("VANGUARD_AI_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1/chat/completions".to_string())
}

pub fn configured_model() -> String {
    std::env::var("VANGUARD_AI_MODEL")
        .unwrap_or_else(|_| "qwen3.5:9b".to_string())
}

/// Готовый запрос к бэкенду: куда, под каким ключом и с каким телом.
///
/// Модель в поле живёт только ради строки лога `CORE LNK` — в тело она уже попала.
struct AiHttpRequest {
    model: String,
    url: String,
    key: String,
    body: String,
}

/// Всё, что можно посчитать **без сети**: конфигурация из env, собеседование
/// (system + история + свежий вопрос) и тело запроса.
///
/// Вынесено из [`spawn_ai_request`], чтобы хрупкие места проверялись отдельно:
/// здесь живут обе ловушки конфигурации — `keep_alive` числом против строки и
/// `reasoning_effort` — а чтение env не переплетается с сетью. Функция чистая и
/// возвращает `Err` строкой вместо того, чтобы самой слать `AiEvent::Error`.
///
/// Диалог собирается как `[system, ...история..., user]`. system всегда первый и свежий:
/// промпт личности задаёт JSON-протокол, и модель должна видеть его в конце контекста,
/// а не вычитывать из собственных старых реплик.
fn build_request(
    user_prompt: String,
    // Предыдущие реплики в порядке возрастания. Без этого аргумента запрос состоял из
    // [system, user] и модель не помнила предыдущих сообщений (см. VanguardAI::messages).
    history: Vec<(String, String)>,
    personality: &DronePersonality,
) -> Result<AiHttpRequest, String> {
    let system_prompt = personality.generate_system_prompt();

    // Конфигурация бэкенда — только через env, хардкода URL/ключа больше нет.
    // Дефолт: локальная Ollama (OpenAI-совместимый /v1/chat/completions), ключ ей не нужен.
    // Проверено на этой машине (реальный системный промпт, 3 запроса): и 7b, и 32b
    // отвечают валидным VanguardAction JSON — ТОЛЬКО если промпт перечисляет все три типа
    // действий. Без примера SystemCommand 7b выдумывал action_type "SystemCheck", и ответ
    // падал в TextResponse-фолбэк как сырой JSON. 32b угадывал и без подсказки, но стоит
    // ~12-24 с на ответ против ~0.5 с у 7b — см. раздел про VRAM ниже.
    let url = configured_url();
    // qwen3.5:9b (6.6 ГБ) целиком помещается в 12 ГБ VRAM RTX 3080 Ti, как и 7b.
    // 32b (21 ГБ) — нет: половина слоёв уходила на CPU и это давало ~6 tok/s вместо десятков.
    // Модель гибридная: thinking у неё включено по умолчанию и стоит 18-44 с на ответ.
    // См. развёрнутый замер у тела запроса ниже — VANGUARD_AI_REASONING_EFFORT=none
    // возвращает скорость ~0.5-1.6 с ценой отсутствия размышлений.
    let model = configured_model();
    let api_key = std::env::var("VANGUARD_AI_KEY").unwrap_or_else(|_| "ollama".to_string());
    // Ollama выгружает модель по таймауту простоя и грузит её заново при следующем
    // запросе. "30m" покрывают обычный ритм работы; "-1" — не выгружать никогда.
    // Ловушка: "-1" работает только как ЧИСЛО — строку "-1" Ollama 0.40 отклоняет
    // с 400 "time: missing unit in duration", поэтому числовые значения (секунды,
    // включая -1) превращаем в JSON-число, а сроки вроде "30m" оставляем строкой.
    let keep_alive_raw = std::env::var("VANGUARD_AI_KEEP_ALIVE")
        .unwrap_or_else(|_| "30m".to_string());
    let keep_alive: serde_json::Value = match keep_alive_raw.parse::<i64>() {
        Ok(secs) => serde_json::json!(secs),
        Err(_) => serde_json::Value::String(keep_alive_raw),
    };

    let mut messages = vec![serde_json::json!({"role": "system", "content": system_prompt})];
    for (role, content) in &history {
        messages.push(serde_json::json!({"role": role, "content": content}));
    }
    messages.push(serde_json::json!({"role": "user", "content": user_prompt}));

    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        // response_format заставляет модель выдавать СТРОГО валидный JSON, который мы просили в промпте
        "response_format": { "type": "json_object" },
        // stream:true — Ollama отдаёт SSE-поток, ответ виден в чате уже по мере генерации
        "stream": true,
        // не выгружать модель сразу после простоя (см. keep_alive выше)
        "keep_alive": keep_alive
    });

    // Гибридные модели (qwen3.*) сливают бюджет в thinking. Замерено на этом эндпоинте
    // для qwen3.5:9b на реальном 3-промптовом стенде протокола (wall / completion_tokens):
    //   "reasoning_effort":"none" -> 0.51 / 0.99 / 1.57 s, 18-91 токенов
    //   "low"                     -> 15.4 / 28.0 / 38.5 s, 1457-3658 токенов
    //   без параметра (полный)    -> 18-44 s
    // Компромисса «чуть умнее, чуть медленнее» у этой модели нет: любое thinking стоит
    // в 20-30 раз дороже. Сейчас оно ВКЛЮЧЕНО (переменная удалена из env, поле не шлётся) —
    // протокол при thinking держится 3/3, а reasoning уходит в отдельное поле delta.reasoning
    // и не попадает в content (проверено на реальном потоке), поэтому чат остаётся чистым.
    // Единственная цена — ~350 пустых чанков до начала ответа: превью в чате молчит,
    // пока модель думает. Поставьте VANGUARD_AI_REASONING_EFFORT=none, если нужна скорость.
    // "none" принимается всеми моделями (7b и deepseek-r1 отвечают http 200), а вот "medium"
    // на не-thinking модели даёт 400 "does not support thinking" — поэтому по умолчанию
    // переменная пуста и поле не шлётся вовсе.
    if let Ok(re) = std::env::var("VANGUARD_AI_REASONING_EFFORT") {
        body["reasoning_effort"] = serde_json::Value::String(re);
    }

    let body = serde_json::to_string(&body)
        .map_err(|e| format!("Ошибка сериализации промпта: {}", e))?;

    Ok(AiHttpRequest { model, url, key: api_key, body })
}

/// Запускает запрос к бэкенду **в отдельном потоке** и шлёт события в канал.
///
/// Тело запроса уже посчитано — см. [`build_request`], разбор ответа отдан
/// [`stream_sse`]. Здесь остаётся только сетевая часть: агент с таймаутами, POST
/// и превращение результата в `AiEvent`.
pub fn spawn_ai_request(
    user_prompt: String,
    history: Vec<(String, String)>,
    personality: DronePersonality,
    ui_tx: Sender<AiEvent>,
) {
    thread::spawn(move || {
        let AiHttpRequest { url, model, key: api_key, body: json_body } =
            match build_request(user_prompt, history, &personality) {
                Ok(req) => req,
                Err(msg) => {
                    let _ = ui_tx.send(AiEvent::Error(msg));
                    return;
                }
            };

        // Лог инициализации: показываем реальный эндпоинт, чтобы видеть битый конфиг сразу
        let _ = ui_tx.send(AiEvent::Token(format!(
            ">> CORE LNK: endpoint={} model={}\n", url, model
        )));

        // Таймаут обязателен: у ureq 3.4.2 по умолчанию ВСЕ таймауты = None, поэтому зависший
        // запрос навсегда оставил бы is_ai_streaming = true в UI и блокировал терминал.
        // 120с: модель на 7b грузится за секунды, а лишние минуты ожидания здесь — это
        // минуты, на которые терминал залочен ("Предыдущий нейро-поток еще активен").
        // http_status_as_error(false) — иначе 4xx/5xx приходят как Err, мы показываем только
        // "http status: 403" и не читаем тело с настоящей причиной ошибки.
        let agent = Agent::new_with_config(
            Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(120)))
                .http_status_as_error(false)
                .build(),
        );

        match agent.post(url.as_str())
            .header("Authorization", &format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .send(json_body) // В ureq 3.x отправка контента идет через .send()
        {
            Ok(response) => {
                let status = response.status().as_u16();
                // into_reader() отдаёт потоковый читатель: тело приходит кусками, а не
                // целиком, поэтому стриминг возможен без отдельного HTTP-клиента.
                let mut reader = BufReader::new(response.into_body().into_reader());

                // Ошибка провайдера: читаем тело целиком, чтобы показать реальную причину
                if !(200..300).contains(&status) {
                    let mut raw = String::new();
                    let _ = reader.read_to_string(&mut raw);
                    let reason: String = raw.chars().take(400).collect();
                    let _ = ui_tx.send(AiEvent::Error(format!(
                        "Бэкенд [{}] ответил HTTP {}: {}", url, status, reason
                    )));
                    return;
                }

                // Разбор потока — отдельная функция: протокол Ollama проверяется сам по себе.
                let accumulated = match stream_sse(&mut reader, &ui_tx) {
                    Ok(text) => text,
                    Err(msg) => {
                        let _ = ui_tx.send(AiEvent::Error(msg));
                        return;
                    }
                };

                if accumulated.is_empty() {
                    let _ = ui_tx.send(AiEvent::Error("Бэкенд не вернул ни одного токена".to_string()));
                    return;
                }

                // Финальная сборка: накопленный JSON уже цельный
                match serde_json::from_str::<VanguardAction>(&accumulated) {
                    Ok(action) => {
                        let _ = ui_tx.send(AiEvent::ExecuteAction(action));
                    }
                    Err(_) => {
                        // Модель не уложилась в протокол — выводим как обычный текст
                        let fallback_action = VanguardAction::TextResponse { text: accumulated };
                        let _ = ui_tx.send(AiEvent::ExecuteAction(fallback_action));
                    }
                }
            }
            Err(e) => {
                let _ = ui_tx.send(AiEvent::Error(format!("Сетевой сбой шины ИИ (ureq error): {}", e)));
            }
        }
        let _ = ui_tx.send(AiEvent::Finished);
    });
}

/// Разбор SSE-потока Ollama: шлёт `AiEvent::Delta` по мере генерации и возвращает
/// собранный JSON-ответ целиком.
///
/// Протокол проверен на реальной Ollama: строки `data: {json}` разделены пустой
/// строкой и завершаются терминатором `data: [DONE]`. Битый чанк пропускается —
/// один мусорный кусок не должен ронять весь ответ. Ошибка чтения возвращается
/// строкой: слать `AiEvent::Error` и решать, падать ли дальше, — дело вызывающего.
fn stream_sse<R: BufRead>(reader: &mut R, ui_tx: &Sender<AiEvent>) -> Result<String, String> {
    let mut accumulated = String::new(); // весь content, как его собрала модель
    let mut shown = String::new();       // что уже ушло в чат
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // поток закрыт сервером
            Ok(_) => {}
            Err(e) => return Err(format!("Ошибка чтения потока ИИ: {}", e)),
        }

        let trimmed = line.trim();
        if !trimmed.starts_with("data:") {
            continue; // пустая строка-разделитель
        }
        let payload = trimmed[5..].trim();
        if payload == "[DONE]" {
            break;
        }

        let chunk: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue, // один битый чанк не должен ронять весь ответ
        };
        if let Some(piece) = chunk["choices"][0]["delta"]["content"].as_str() {
            if piece.is_empty() {
                continue;
            }
            accumulated.push_str(piece);

            // В чат идёт текст ответа, а не собирающаяся JSON-оболочка
            let preview = extract_preview(&accumulated);
            if preview != shown {
                shown = preview.clone();
                let _ = ui_tx.send(AiEvent::Delta(preview));
            }
        }
    }

    Ok(accumulated)
}

/// Вытаскивает из (возможно ещё неполного) JSON-объекта то, что модель уже успела
/// написать в поле `text`. Полный парсер тут не подходит: ждать закрывающей скобки
/// означало бы отложить весь вывод до конца генерации, то есть стриминг не случится.
fn extract_preview(raw: &str) -> String {
    // Ключ ищем без учёта форматирования после двоеточия — модель может писать
    // и `"text": "..."`, и `"text":"..."`.
    let key_pos = match raw.find("\"text\"") {
        Some(i) => i + "\"text\"".len(),
        None => return String::new(), // заголовок JSON ещё не дописан
    };

    let bytes = raw.as_bytes();
    let mut i = key_pos;
    // Пропускаем пробелы/табуляции/переносы и само двоеточие (все — ASCII, поэтому
    // индексы остаются на границах символов и срез безопасен)
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r' || bytes[i] == b':') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'"' {
        return String::new(); // значение ещё не началось или оно не строка
    }
    i += 1;
    let body = &raw[i..];

    // Ищем закрывающую кавычку, не экранированную обратным слэшем
    let mut end = None;
    let mut escaped = false;
    for (idx, ch) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => {
                end = Some(idx);
                break;
            }
            _ => {}
        }
    }
    let slice = match end {
        Some(idx) => &body[..idx],
        None => body, // строка ещё не закрыта — показываем всё, что есть
    };

    // Раскодируем силами serde_json вместо своего обработчика экранирования.
    // Если срез оборван на подвешенном экранировании — вернём сырую строку,
    // и на следующем токене превью просто дообновится.
    serde_json::from_str::<String>(&format!("\"{}\"", slice)).unwrap_or_else(|_| slice.to_string())
}