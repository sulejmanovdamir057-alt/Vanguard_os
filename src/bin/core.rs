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
use std::sync::{Arc, Mutex};
use std::thread;

use vanguard_os_lib::core::executor::VanguardExecutor;
use vanguard_os_lib::core::launcher;
use vanguard_os_lib::core::shell::VanguardShell;
use vanguard_os_lib::core::telemetry::{spawn_telemetry_daemon, SystemMetrics};
use vanguard_os_lib::ipc::{self, DeviceEntry, DeviceView, Event, FsEntry, FsFile, FsListing, NetAdapter, NetView, NetWifi, Request, Response, VirtualDriver};

/// `pub(crate)` — не для наружу: так эту же функцию зовёт `src/main.rs`,
/// подключивший файл модулем `#[path]` ради раздачи одним бинарником.
pub(crate) fn main() {
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
            // ── C#-приложения: компиляция (csc/dotnet) и запуск отдельным процессом ──
            Request::RunCsApp { dir, entry } => {
                send(&mut writer, &run_cs_app(&dir, &entry));
            }
            // ── Go-приложения: сборка (go build) и запуск отдельным процессом ──
            Request::RunGoApp { dir, entry } => {
                send(&mut writer, &run_go_app(&dir, &entry));
            }
            // ── Диспетчер устройств: только чтение, опрос через PowerShell ──
            Request::HwDevices => send(&mut writer, &hw_devices()),
            // ── Сеть: только чтение, опрос PowerShell + netsh ──
            Request::NetStatus => send(&mut writer, &net_status()),
            Request::SetDriver { id, enabled } => {
                send(&mut writer, &set_driver(&id, enabled))
            }
            // ── Запуски средствами Windows: C#-мост ShellExecute ──
            Request::ShellExec { mode, target } => {
                send(&mut writer, &shell_exec(&mode, &target));
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
        // Терминальные `cls`/`start` без слэша: у ядра нет ни чата, ни UI, так
        // что клиенту возвращаем маркеры, а разбор — на его стороне.
        ShellAction::ClearChat => Response::ok("<< CLEAR_CHAT >>"),
        ShellAction::OpenPath(target) => Response::ok(format!("<< OPEN_PATH: {} >>", target)),
        // `runas` дошёл до ядра — значит, запрос шла по IPC напрямую (`Request::Shell`).
        // Выполняем здесь же: мост запуска живёт в ядре, второй обход не нужен.
        ShellAction::Elevate(target) => shell_exec("runas", &target),
    }
}

/// Кэш сырого JSON `Get-PnPDevice`: PowerShell дорог (~1 с), а переключение
/// виртуального драйвера реальную аппаратуру не меняет — дерево перечитывать
/// незачем. Возраст не проверяется: свежесть гарантирует кнопка «Обновить»
/// (она идёт в `ps_fresh` в обход кэша), а тумблер так мигает мгновенно.
static PS_CACHE: Mutex<Option<(std::time::Instant, String)>> = Mutex::new(None);

/// Диспетчер устройств (кнопка «Обновить»): свежий `Get-PnPDevice` плюс
/// виртуальные драйверы. `$` в скрипте безопасен: аргумент уходит в
/// CreateProcess напрямую, без оболочки. `[string]`-каст в проекциях обязателен:
/// `$null` стал бы JSON-`null`, который строковое поле не примет, а
/// `[Console]::OutputEncoding` выправляет кириллицу в pipe (иначе mojibake).
fn hw_devices() -> Response {
    match ps_fresh() {
        Ok(ps) => Response::ok(view_json(&ps)),
        Err(e) => Response::err(e),
    }
}

/// Ответ [`Request::NetStatus`] до разбора: то, что PowerShell собрал про
/// адаптеры, плюс сырой текст netsh — Wi-Fi разбирает [`parse_wifi`] уже
/// на нашей стороне.
#[derive(serde::Deserialize)]
struct NetStatusRaw {
    #[serde(default)]
    adapters: Vec<NetAdapter>,
    #[serde(default)]
    wifi_hw: bool,
    #[serde(default)]
    wifi_text: String,
}

/// Окно «Сеть» (первое открытие и кнопка «Обновить»): адаптеры с адресами из
/// PowerShell плюс Wi-Fi-эфир из netsh. Кэша нет — в отличие от драйверов,
/// сетевые состояния меняются часто, а опрос и так идёт только по запросу.
/// `$` в скрипте безопасен: аргумент уходит в CreateProcess напрямую, без
/// оболочки. `[string]`-касты обязательны ($null стал бы JSON-`null`,
/// который строковое поле не примет), `[Console]::OutputEncoding` выправляет
/// кириллицу в pipe. Слияние `Get-NetAdapter` и `Get-NetIPConfiguration` —
/// по имени интерфейса; у адаптера без адресов (или не попавшего в конфиг)
/// `$c` — `$null`, и все его поля просто придут пустыми.
fn net_status() -> Response {
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
        $cfg=@{}; Get-NetIPConfiguration | ForEach-Object { $cfg[$_.InterfaceAlias]=$_ }; \
        $a = @(Get-NetAdapter | ForEach-Object { $c = $cfg[$_.Name]; \
        [pscustomobject]@{ name=[string]$_.Name; status=[string]$_.Status; speed=[string]$_.LinkSpeed; \
        desc=[string]$_.InterfaceDescription; ipv4=[string](@($c.IPv4Address | ForEach-Object { $_.IPAddress }) -join ', '); \
        gw=[string](@($c.IPv4DefaultGateway | ForEach-Object { $_.NextHop }) -join ', '); \
        dns=[string](@($c.DNSServer | Where-Object { $_.AddressFamily -eq 2 } | ForEach-Object { $_.ServerAddresses }) -join ', ') }}); \
        $hw = @(Get-NetAdapter -IncludeHidden | Where-Object { $_.Name -match 'Wireless|Wi-Fi|802.11|WLAN' \
        -or $_.InterfaceDescription -match 'Wireless|Wi-Fi|802.11|WLAN' }).Count -gt 0; \
        $wt = (netsh wlan show networks | Out-String); \
        ConvertTo-Json -Compress -Depth 5 -InputObject ([pscustomobject]@{ adapters=$a; wifi_hw=[bool]$hw; wifi_text=$wt })";
    use std::process::Command;
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|e| format!("powershell не запустился: {}", e));
    let out = match out {
        Ok(o) => o,
        Err(e) => return Response::err(e),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Response::err(format!(
            "PowerShell: {}",
            err.trim().chars().take(300).collect::<String>()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return Response::err(String::from("Get-NetAdapter ничего не вернул"));
    }
    let raw: NetStatusRaw = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => return Response::err(format!("кривой JSON PowerShell: {}", e)),
    };
    let view = NetView {
        adapters: raw.adapters,
        wifi: parse_wifi(&raw.wifi_text),
        wifi_hw: raw.wifi_hw,
    };
    match serde_json::to_string(&view) {
        Ok(s) => Response::ok(s),
        Err(e) => Response::err(e.to_string()),
    }
}

/// Разбор `netsh wlan show networks` без привязки к локали: ключ «SSID» не
/// переводится ни на одной (вывод этой машины — EN, текст ошибок PowerShell
/// при этом — RU, поэтому на «английские» слова полагаться нельзя), а сигнал —
/// строка, чьё значение кончается на «%» («Signal : 87%» и «Сигнал : 87%» —
/// одно и то же). Шапка netsh и строки до первого SSID отбрасываются тем, что
/// `last_mut()` ещё нет. Пустой SSID — скрытая сеть, имя подставит GUI.
fn parse_wifi(text: &str) -> Vec<NetWifi> {
    let mut out: Vec<NetWifi> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // «SSID 1 : Имя» — начало новой сети. «BSSID …» сюда не попадает:
        // префикс у него другой, а netsh без mode=bssid его и не печатает.
        if t.starts_with("SSID ") {
            if let Some(v) = t.splitn(2, ':').nth(1) {
                out.push(NetWifi { ssid: v.trim().to_string(), signal: 0 });
            }
            continue;
        }
        let Some(v) = t.splitn(2, ':').nth(1) else { continue; };
        let v = v.trim();
        if v.ends_with('%') {
            if let Ok(n) = v.trim_end_matches('%').trim().parse::<u8>() {
                if let Some(last) = out.last_mut() {
                    last.signal = n;
                }
            }
        }
    }
    out
}

/// Переключатель виртуального драйвера: пишет `enabled` в его `driver.json`
/// и возвращает свежий [`DeviceView`] — клиенту не нужен второй запрос.
/// Путь от id собирается только после проверки: без неё `..` открыл бы
/// запись куда угодно.
fn set_driver(id: &str, enabled: bool) -> Response {
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\']) {
        return Response::err(format!("недопустимый id: «{}»", id));
    }
    let path = Path::new("drivers").join(id).join("driver.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Response::err(format!("драйвера «{}» нет", id)),
    };
    let mut driver: VirtualDriver = match serde_json::from_str(&text) {
        Ok(d) => d,
        Err(e) => return Response::err(format!("кривой driver.json: {}", e)),
    };
    driver.enabled = enabled;
    let out = match serde_json::to_string_pretty(&driver) {
        Ok(s) => s,
        Err(e) => return Response::err(e.to_string()),
    };
    if let Err(e) = std::fs::write(&path, out) {
        return Response::err(format!("не удалось записать {}: {}", path.display(), e));
    }
    match ps_cached() {
        Ok(ps) => Response::ok(view_json(&ps)),
        Err(e) => Response::err(e),
    }
}

/// Свежий опрос PowerShell — путь кнопки «Обновить»; результат кладёт в кэш.
fn ps_fresh() -> Result<String, String> {
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
        $d = @(Get-PnPDevice | Select-Object \
        @{n='name';e={[string]$_.FriendlyName}}, @{n='class';e={[string]$_.Class}}, \
        @{n='status';e={[string]$_.Status}}, @{n='instance';e={[string]$_.InstanceId}}); \
        ConvertTo-Json -Compress -Depth 3 -InputObject $d";
    use std::process::Command;
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|e| format!("powershell не запустился: {}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "PowerShell: {}",
            err.trim().chars().take(300).collect::<String>()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return Err(String::from("Get-PnPDevice ничего не вернул"));
    }
    if let Ok(mut c) = PS_CACHE.lock() {
        *c = Some((std::time::Instant::now(), text.clone()));
    }
    Ok(text)
}

/// Снимок PnP для путей, где реальная аппаратура не менялась (тумблер
/// драйвера). Кэша нет или протух (> 60 с) — опрашиваем на месте.
fn ps_cached() -> Result<String, String> {
    if let Ok(c) = PS_CACHE.lock() {
        if let Some((at, text)) = c.as_ref() {
            if at.elapsed().as_secs() < 60 {
                return Ok(text.clone());
            }
        }
    }
    ps_fresh()
}

/// Все виртуальные драйверы из `drivers/<id>/driver.json`. Кривой манифест или
/// пустые id/name пропускаются — одно битое приложение не должно ронять дерево.
fn read_drivers() -> Vec<VirtualDriver> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("drivers") else { return out; };
    for e in rd.flatten() {
        if !e.path().is_dir() { continue; }
        let Ok(t) = std::fs::read_to_string(e.path().join("driver.json")) else { continue; };
        let Ok(mut d) = serde_json::from_str::<VirtualDriver>(&t) else { continue; };
        if d.id.is_empty() || d.name.is_empty() { continue; }
        if d.class.is_empty() { d.class = String::from("System"); }
        out.push(d);
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// Собирает ответ: PnP-дерево + включённые виртуальные драйверы отдельными
/// строками + полный перечень драйверов для секции управления.
fn view_json(ps_json: &str) -> String {
    // Кривой кэш лучше пустого дерева: настоящую ошибку Пользователь увидит
    // по «Обновить» (он идёт через ps_fresh и кэш не читает).
    let mut devices: Vec<DeviceEntry> = serde_json::from_str(ps_json).unwrap_or_default();
    let drivers = read_drivers();
    for d in drivers.iter().filter(|d| d.enabled) {
        devices.push(DeviceEntry {
            name: d.name.clone(),
            class: d.class.clone(),
            status: String::from("OK"),
            instance: format!("VIRTUAL\\{}\\0", d.id),
            virtual_dev: true,
        });
    }
    let view = DeviceView { devices, drivers };
    serde_json::to_string(&view).unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e))
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

// ─── C#-приложения и C#-мост запуска ────────────────────────────────────────

/// ShellExecute через C#-мост (`core::launcher`): белый список режимов — сам
/// хелпер доверяет `args[0]` не больше, чем мы. Ответ — строка хелпера
/// (`pid=…`) либо хвост ошибки; сборка хелпера (csc, ~0.6 с) тоже здесь.
fn shell_exec(mode: &str, target: &str) -> Response {
    const MODES: [&str; 4] = ["runas", "aumid", "clsid", "shell"];
    if !MODES.contains(&mode) {
        return Response::err(format!("vlaunch: неизвестный режим «{}»", mode));
    }
    match launcher::run(&[mode, target]) {
        Ok(t) => Response::ok(t),
        Err(e) => Response::err(e),
    }
}

/// C#-приложение: компиляция и запуск отдельным процессом. Секунды (dotnet
/// build) — поэтому запросом в ядро, а не вызовом GUI; клиент держит `bg_tasks`.
/// Ответ — PID строкой, ошибка — хвост вывода компилятора.
fn run_cs_app(dir: &str, entry: &str) -> Response {
    match cs_build_and_run(dir, entry) {
        Ok(pid) => Response::ok(pid),
        Err(e) => Response::err(e),
    }
}

/// Go-приложение: компиляция и запуск отдельным процессом. Секунды (первый
/// `go build` греет кэш) — поэтому запросом в ядро, а не вызовом GUI; клиент
/// держит `bg_tasks`. Ответ — PID строкой, ошибка — хвост вывода сборщика.
fn run_go_app(dir: &str, entry: &str) -> Response {
    match go_build_and_run(dir, entry) {
        Ok(pid) => Response::ok(pid),
        Err(e) => Response::err(e),
    }
}

/// Общая проверка путей компилируемых входов (C#/Go). `dir` приходит
/// относительным (`apps/<id>`): ядро и оболочка делят CWD (правило раздела о
/// runtime side effects), поэтому резолвим от текущего каталога ядра. `..` и
/// байты NUL отсекаем текстом до любых проверок на диске; `entry` — только
/// голый файл, как в манифесте. csc к тому же режет аргументы по `/`
/// (проверено: `tools/vlaunch.cs` он читает как опцию) — слэши приводим к
/// windows-виду здесь же, до того, как путь уйдёт в компилятор.
fn app_src(dir: &str, entry: &str) -> Result<(PathBuf, PathBuf), String> {
    if dir.contains("..") || dir.contains('\0') {
        return Err(String::from("кривая директория приложения"));
    }
    if entry.contains("..") || entry.contains('/') || entry.contains('\\') || entry.contains('\0') {
        return Err(String::from("кривой файл входа: нужен голый файл вроде main.cs"));
    }
    let dir = dir.replace('/', "\\");
    let root = if Path::new(&dir).is_absolute() {
        PathBuf::from(dir)
    } else {
        std::env::current_dir()
            .map_err(|e| format!("не удалось прочитать CWD: {}", e))?
            .join(dir)
    };
    if !root.is_dir() {
        return Err(format!("нет папки приложения {}", root.display()));
    }
    let src = root.join(entry);
    if !src.is_file() {
        return Err(format!("нет файла входа {}", src.display()));
    }
    Ok((root, src))
}

/// C#-вход: выбор компилятора по расширению, старт — общий [`spawn_built`].
fn cs_build_and_run(dir: &str, entry: &str) -> Result<String, String> {
    let (root, src) = app_src(dir, entry)?;
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let exe = match ext.as_str() {
        "cs" => csc_build(&src)?,
        "csproj" => dotnet_build(&root, &src)?,
        other => return Err(format!("не C#-вход: .{}", other)),
    };
    spawn_built(&exe, &root)
}

/// Go-вход: та же схема «проверить пути → собрать → запустить», что у C#.
fn go_build_and_run(dir: &str, entry: &str) -> Result<String, String> {
    let (root, src) = app_src(dir, entry)?;
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "go" {
        return Err(format!("не Go-вход: .{}", ext));
    }
    let exe = go_build(&root, &src)?;
    spawn_built(&exe, &root)
}

/// Старт скомпилированного приложения: отдельный процесс, потоки не
/// перехватываем — та же политика, что у `apps::launch_app`; консольному
/// приложению оболочка без консоли выделит свою. Отдаёт PID строкой.
fn spawn_built(exe: &Path, root: &Path) -> Result<String, String> {
    let child = std::process::Command::new(exe)
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("не удалось запустить {}: {}", exe.display(), e))?;
    Ok(child.id().to_string())
}

/// Путь к компилятору Go. Голое имя `go` ищется по PATH процесса ядра — но
/// оболочка, запущенная из сессии, унаследованной до установки SDK, приносит
/// PATH без Go (замерено: ядро из harness-шелла отвечает «program not found»).
/// Поэтому — кандидаты, как у `csc_build`: портативная раскладка
/// `%LOCALAPPDATA%\Programs\Go` и стандартный путь установщика `C:\Go`.
fn go_tool() -> Result<PathBuf, String> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join("go.exe");
            if p.is_file() {
                cands.push(p);
                break;
            }
        }
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        cands.push(Path::new(&la).join("Programs").join("Go").join("bin").join("go.exe"));
    }
    cands.push(PathBuf::from(r"C:\Go\bin\go.exe"));
    cands.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        String::from("go.exe не найден: ни в PATH, ни в %LOCALAPPDATA%\\Programs\\Go, ни в C:\\Go — нужен Go SDK")
    })
}

/// Одиночный `.go` → exe через `go build -o <temp> file.go`: однофайловый
/// вход собирается и без go.mod. Результат — в %TEMP%, как у csc (папка
/// `store/` под git, мусор там остался бы виден в git status).
fn go_build(root: &Path, src: &Path) -> Result<PathBuf, String> {
    let go = go_tool()?;
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("app");
    let parent = src
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("app");
    let out = std::env::temp_dir().join(format!("vanguard_go_{}_{}.exe", parent, stem));
    let res = std::process::Command::new(&go)
        .args(["build", "-o"])
        .arg(&out)
        .arg(src)
        .current_dir(root)
        .output()
        .map_err(|e| format!("go не запустился (нужен Go SDK в PATH): {}", e))?;
    if !res.status.success() {
        return Err(launcher::output_tail(&res.stdout, &res.stderr));
    }
    if !out.is_file() {
        return Err(String::from("go build не создал exe"));
    }
    Ok(out)
}

/// Одиночный `.cs` → exe через csc.exe .NET Framework — он входит в каждую
/// Windows, SDK и NuGet не нужны; сам компилятор — C# 5, исходники под него.
/// `/codepage:65001` обязателен: без BOM UTF-8 читается как cp1251 (та же
/// ловушка, что у `.ps1` в PS 5.1). Флаг цели подбираем по исходнику: WinForms
/// под `winexe`, иначе рядом с окном открылась бы пустая консоль.
fn csc_build(src: &Path) -> Result<PathBuf, String> {
    const CANDIDATES: [&str; 2] = [
        r"C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe",
        r"C:\Windows\Microsoft.NET\Framework\v4.0.30319\csc.exe",
    ];
    let csc = CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            String::from("csc.exe не найден — нужен .NET Framework (входит в Windows)")
        })?;
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("app");
    let parent = src
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("app");
    // Результат — в %TEMP%, а не рядом с исходником: папка store/ под git,
    // и мусор там остался бы виден в git status.
    let out = std::env::temp_dir().join(format!("vanguard_cs_{}_{}.exe", parent, stem));
    let text = std::fs::read_to_string(src).unwrap_or_default();
    let target = if text.contains("System.Windows.Forms") {
        "winexe"
    } else {
        "exe"
    };
    let out_arg = format!("/out:{}", out.display());
    let res = std::process::Command::new(csc)
        .args(["/nologo", "/codepage:65001", "/optimize+"])
        .arg(format!("/target:{}", target))
        .arg(out_arg)
        .args(["/r:System.Windows.Forms.dll", "/r:System.Drawing.dll"])
        .arg(src)
        .output()
        .map_err(|e| format!("csc не запустился: {}", e))?;
    if !res.status.success() {
        return Err(launcher::output_tail(&res.stdout, &res.stderr));
    }
    Ok(out)
}

/// `.csproj` → `dotnet build -c Release` (нужен установленный .NET SDK; без
/// пакетов проект собирается и без сети) → exe под `bin/Release/<TFM>/`.
fn dotnet_build(root: &Path, proj: &Path) -> Result<PathBuf, String> {
    let res = std::process::Command::new("dotnet")
        .arg("build")
        .arg(proj)
        .args(["-c", "Release", "--nologo", "-v", "q"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("dotnet не запустился (нужен .NET SDK): {}", e))?;
    if !res.status.success() {
        return Err(launcher::output_tail(&res.stdout, &res.stderr));
    }
    let stem = proj.file_stem().and_then(|s| s.to_str()).unwrap_or("app");
    // TFM-папка одна уровень (net10.0-windows/…); имя exe = имя проекта.
    let pattern = root
        .join("bin")
        .join("Release")
        .join("*")
        .join(format!("{}.exe", stem));
    let pattern = pattern.to_string_lossy().replace('\\', "/");
    let mut found: Vec<PathBuf> = glob::glob(&pattern)
        .map_err(|e| format!("кривой glob: {}", e))?
        .filter_map(|p| p.ok())
        .collect();
    found.sort();
    found
        .into_iter()
        .next()
        .ok_or_else(|| format!("сборка прошла, но exe не найден: {}", pattern))
}

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
