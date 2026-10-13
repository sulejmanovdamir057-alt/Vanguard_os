//! IPC-клиент оболочки: связь с ядром `vanguard_core.exe` по TCP.
//!
//! Ядро — отдельный процесс. Оболочка подключается к `127.0.0.1:7331`; если его
//! ещё нет, запускает `vanguard_core.exe` сама (из той же папки, где лежит эта
//! оболочка) и ждёт, пока поднимется listener. Каждый вызов держит соединение
//! на время одного запроса — проще, чем пул, а задержка локалхоста несущественна.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use vanguard_os_lib::ipc::{self, Response};

/// Короткий таймаут на соединение/чтение: ядро локальное, долго ждать нечего.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Ответ на один IPC-запрос. Поток событий (`/run`-логи) уже сведён в текст.
pub struct CoreReply {
    pub ok: bool,
    pub text: String,
}

/// Подключается к ядру, при необходимости поднимая его, и выполняет один запрос.
/// `request` — уже сериализованная JSON-строка [`vanguard_os_lib::ipc::Request`].
///
/// Возвращает ответ ядра; события потока (`Event::TaskLog`) склеиваются в текст
/// в порядке прибытия, так что клиент получает готовую стенограмму задачи.
pub fn call(request_json: &str) -> CoreReply {
    let stream = match connect_with_autostart() {
        Ok(s) => s,
        Err(e) => return CoreReply { ok: false, text: format!(">> ЯДРО НЕДОСТУПНО: {}\n", e) },
    };

    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => return CoreReply { ok: false, text: format!(">> ЯДРО: сбой клонирования сокета: {}\n", e) },
    };

    let mut line = request_json.to_string();
    line.push('\n');
    if let Err(e) = writer.write_all(line.as_bytes()).and_then(|_| writer.flush()) {
        return CoreReply { ok: false, text: format!(">> ЯДРО: сбой отправки: {}\n", e) };
    }

    // Читаем до финального ответа. Ядро соединение НЕ закрывает (его цикл
    // живёт до EOF, чтобы на одном сокете можно было слать несколько запросов),
    // поэтому без break клиент ждал бы следующую строку до read-timeout.
    // Финал всегда последний: события /run шлёт один поток до `Ok`.
    let reader = BufReader::new(stream);
    let mut text = String::new();
    let mut ok = true;
    let mut saw_final = false;

    for resp_line in reader.lines() {
        let resp_line = match resp_line {
            Ok(l) => l,
            Err(_) => break,
        };
        let resp_line = resp_line.trim();
        if resp_line.is_empty() {
            continue;
        }
        let response: Response = match serde_json::from_str(resp_line) {
            Ok(r) => r,
            Err(_) => continue, // битую строку пропускаем, не роняем весь ответ
        };
        match response {
            Response::Ok { text: t, .. } => {
                text.push_str(&t);
                saw_final = true;
                break;
            }
            Response::Err { text: t, .. } => {
                text.push_str(&t);
                ok = false;
                saw_final = true;
                break;
            }
            Response::Event { event: vanguard_os_lib::ipc::Event::TaskLog { line }, .. } => {
                text.push_str(&line);
                text.push('\n');
            }
            Response::Pong => {
                text.push_str(">> ЯДРО: pong\n");
                saw_final = true;
                break;
            }
        }
    }

    if !saw_final && text.is_empty() {
        ok = false;
        text.push_str(">> ЯДРО: пустой ответ (соединение оборвано?)\n");
    }

    CoreReply { ok, text }
}

/// Находит сокет ядра: пробуем подключиться, при отказе поднимаем процесс и ждём.
fn connect_with_autostart() -> std::io::Result<TcpStream> {
    let addr = ipc::core_addr();

    // Первая попытка: ядро уже запущено.
    if let Ok(stream) = TcpStream::connect(&addr) {
        let _ = stream.set_read_timeout(Some(TIMEOUT));
        let _ = stream.set_write_timeout(Some(TIMEOUT));
        return Ok(stream);
    }

    // Ядра нет — запускаем из той же папки, где лежит эта оболочка.
    spawn_core_process();

    // Ждём, пока listener поднимется (до ~3 с).
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(stream) = TcpStream::connect(&addr) {
            let _ = stream.set_read_timeout(Some(TIMEOUT));
            let _ = stream.set_write_timeout(Some(TIMEOUT));
            return Ok(stream);
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        format!("ядро не ответило на {} за 3 с", addr),
    ))
}

/// Ищем `vanguard_core.exe` рядом с текущим бинарником и запускаем его фоном.
fn spawn_core_process() {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let core_path = exe_dir.join("vanguard_core.exe");

    let mut cmd = Command::new(&core_path);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // create_no_window: ядро — служебный процесс, отдельного окна/консоли не нужно.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd.spawn(); // если не вышло — вернётся ошибка при connect
}
