//! Формат приложений Vanguard OS: папка `apps/<id>/` с манифестом `app.json`.
//!
//! Каталог `store/` — локальный «магазин»: установка это копирование папки
//! `store/<id>` → `apps/<id>`, удаление — её стирание. Копия в store остаётся,
//! поэтому удалённое приложение ставится заново. Вход — скрипт или бинарник,
//! запуск отдельным процессом, как в настоящей Windows.

use std::path::{Path, PathBuf};
use serde::Deserialize;

/// Манифест — `app.json` в корне папки приложения.
#[derive(Debug, Clone, Deserialize)]
pub struct AppManifest {
    pub id: String,
    pub name: String,
    /// Файл входа относительно папки приложения: main.py / app.exe / run.bat
    /// / main.cs / app.csproj — по расширению выбирается способ запуска.
    pub entry: String,
    /// Глиф для списков: только символы Segoe UI Symbol (эмодзи = квадраты).
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub desc: String,
}

/// Найденное приложение: манифест плюс его папка.
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub manifest: AppManifest,
    pub dir: PathBuf,
}

/// Читает `dir/<id>/app.json` для каждой вложенной папки. Кривой манифест или
/// пустые обязательные поля пропускаются: одно битое приложение не должно
/// ронять весь список. Сортировка — по имени, регистронезависимо, как в
/// проводнике.
pub fn scan_apps(dir: &str) -> Vec<AppInfo> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out; };
    for e in rd.flatten() {
        if !e.path().is_dir() { continue; }
        let Ok(text) = std::fs::read_to_string(e.path().join("app.json")) else { continue; };
        let Ok(mut m) = serde_json::from_str::<AppManifest>(&text) else { continue; };
        if m.id.is_empty() || m.name.is_empty() || m.entry.is_empty() { continue; }
        if m.icon.is_empty() { m.icon = String::from("▶"); }
        out.push(AppInfo { manifest: m, dir: e.path() });
    }
    out.sort_by_key(|a| a.manifest.name.to_lowercase());
    out
}

/// C#-вход (`.cs`/`.csproj`): компиляция — тяжёлый отдельный путь через ядро
/// (`Request::RunCsApp`), поэтому [`launch_app`] такое приложение запустить не
/// может — оболочка проверяет это первым делом.
pub fn is_cs(entry: &str) -> bool {
    matches!(
        entry.to_ascii_lowercase().rsplit_once('.').map(|(_, e)| e),
        Some("cs" | "csproj")
    )
}

/// Go-вход (`.go`): сборка `go build` — тяжёлый путь через ядро
/// (`Request::RunGoApp`), ровно как у C# — см. [`is_cs`]. `go` должен быть
/// в PATH ядра, иначе запрос вернёт понятную ошибку вместо запуска.
pub fn is_go(entry: &str) -> bool {
    matches!(
        entry.to_ascii_lowercase().rsplit_once('.').map(|(_, e)| e),
        Some("go")
    )
}

/// Идентификатор должен быть именем папки, а не путём: без этого install/uninstall
/// открыли бы переход на уровень выше прямо из строки запроса.
fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\']) {
        return Err(format!("недопустимый id: «{}»", id));
    }
    Ok(())
}

/// Ставит приложение из `store/<id>` в `apps/<id>` обычным копированием папки.
/// Занятый каталог — ошибка, а не перезапись: то же правило, что у проводника.
pub fn install_app(id: &str) -> Result<String, String> {
    check_id(id)?;
    let from = Path::new("store").join(id);
    let to = Path::new("apps").join(id);
    if !from.is_dir() { return Err(format!("в магазине нет «{}»", id)); }
    if to.exists() { return Err(format!("«{}» уже установлено", id)); }
    copy_dir(&from, &to)?;
    Ok(format!("Установлено в {}", to.display()))
}

/// Удаляет установленное приложение целиком и рекурсивно. Копию в store не
/// трогает — установить заново можно.
pub fn uninstall_app(id: &str) -> Result<String, String> {
    check_id(id)?;
    let to = Path::new("apps").join(id);
    if !to.is_dir() { return Err(format!("нет приложения «{}»", id)); }
    std::fs::remove_dir_all(&to).map_err(|e| format!("не удалось удалить: {}", e))?;
    Ok(format!("Удалено: {}", id))
}

/// Рекурсивное копирование папки. Обычный `std::fs`: приложения маленькие,
/// а отдельный поток здесь был бы сложностью без выигрыша.
fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to)
        .map_err(|e| format!("не удалось создать {}: {}", to.display(), e))?;
    let rd = std::fs::read_dir(from).map_err(|e| e.to_string())?;
    for e in rd.flatten() {
        let dst = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), &dst).map_err(|err| {
                format!("не удалось скопировать {}: {}", e.path().display(), err)
            })?;
        }
    }
    Ok(())
}

/// Стартует приложение отдельным процессом; возвращает PID. Тип входа
/// выбирается по расширению, как в Windows. Потоки не перехватываем: это не
/// `/run` (ему логи обязаны), а приложение со своим окном — консольное
/// получит свою консоль, GUI-процесс отрисует окно сам.
pub fn launch_app(app: &AppInfo) -> Result<u32, String> {
    let entry = app.dir.join(&app.manifest.entry);
    if !entry.is_file() {
        return Err(format!("нет файла входа {}", entry.display()));
    }
    let ext = entry
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    use std::process::{Command, Stdio};
    let mut cmd = match ext.as_str() {
        "py" => {
            let mut c = Command::new("python");
            c.arg(&entry);
            c
        }
        "bat" | "cmd" => {
            let mut c = Command::new("cmd");
            c.arg("/c").arg(&entry);
            c
        }
        "ps1" => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&entry);
            c
        }
        "exe" => Command::new(&entry),
        // Defensive: вызывающие проверяют is_cs() первым делом, но если дошло
        // сюда — это не «неизвестный формат», а путь, который ядро умеет сам.
        other if is_cs(other) => {
            return Err(String::from(
                "C#-вход: компиляция идёт в ядре (Request::RunCsApp), не через launch_app",
            ))
        }
        other if is_go(other) => {
            return Err(String::from(
                "Go-вход: сборка идёт в ядре (Request::RunGoApp), не через launch_app",
            ))
        }
        other => return Err(format!("неизвестный тип входа: .{}", other)),
    };
    let child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("не удалось запустить {}: {}", entry.display(), e))?;
    Ok(child.id())
}
