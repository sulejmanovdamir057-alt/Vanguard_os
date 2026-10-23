//! C#-мост запуска: `tools/vlaunch.cs` → `vanguard_launch.exe`.
//!
//! Хелпер собирается через csc.exe .NET Framework — он входит в каждую Windows
//! и не требует ни SDK, ни NuGet — и делает то, чего здесь не хочется делать
//! winapi-зависимостями: ShellExecute с повышением (`runas`, UAC-диалог
//! системы), запуск системных приложений Windows по AUMID
//! (`shell:AppsFolder\...`) и по CLSID (`shell::{GUID}`); режим `shell` — тот же
//! ShellExecute без особых слов.
//!
//! Модуль лежит в lib по общей схеме «движок — в lib», но зовёт его ядро для
//! [`crate::ipc::Request::ShellExec`]. Правила:
//! - `tools/vlaunch.cs` держим ASCII-only — та же дисциплина, что у `tools/*.ps1`
//!   (и `/codepage:65001` в сборке закрывает кодировку на любых исходниках);
//! - exe пересобирается, когда исходник новее по mtime — правка .cs видна сразу;
//! - вызывающий держит `bg_tasks`: на `runas` ответ ждёт пользователя в UAC
//!   сколько угодно, кадр держать нельзя.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Исходник хелпера — относительно CWD процесса (общее правило артефактов).
/// Только через `join`, а не слэшем в строке: **csc режет аргумент по `/`** —
/// аргумент `tools/vlaunch.cs` он читает как неизвестную опцию `/vlaunch.cs` и
/// ищет `vlaunch.cs` в корне (проверено руками 2026-10-23).
fn src_path() -> PathBuf {
    Path::new("tools").join("vlaunch.cs")
}
/// Собранный хелпер — в CWD; в `.gitignore`, локальный артефакт.
const EXE: &str = "vanguard_launch.exe";

/// csc.exe .NET Framework: сначала 64-битный, затем 32-битный — на любой
/// машине есть хотя бы один.
fn csc_path() -> Result<PathBuf, String> {
    const CANDIDATES: [&str; 2] = [
        r"C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe",
        r"C:\Windows\Microsoft.NET\Framework\v4.0.30319\csc.exe",
    ];
    CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            String::from("csc.exe не найден — нужен .NET Framework (входит в Windows)")
        })
}

/// Путь к свежему хелперу: собирает (или пересобирает, если `tools/vlaunch.cs`
/// новее) при первом вызове. Ошибка — либо нет исходника, либо csc не прошёл.
pub fn ensure_helper() -> Result<PathBuf, String> {
    let src = src_path();
    let exe = Path::new(EXE);
    if !src.is_file() {
        return Err(format!("нет исходника хелпера: {}", src.display()));
    }
    // Абсолютный путь на выход: `Command::new` с голым именем ищет exe по
    // правилам CreateProcess, но CWD в этот поиск не входит — Rust std снял
    // текущий каталог из резолва exe ради безопасности (проверено руками
    // 2026-10-23: относительное имя из корня репозитория даёт "program not found").
    let abs = std::env::current_dir()
        .map_err(|e| format!("не удалось прочитать CWD: {}", e))?
        .join(EXE);
    let src_time = std::fs::metadata(&src).and_then(|m| m.modified()).ok();
    let exe_time = std::fs::metadata(exe).and_then(|m| m.modified()).ok();
    if let (Some(s), Some(e)) = (src_time, exe_time) {
        if e >= s {
            return Ok(abs);
        }
    }
    let csc = csc_path()?;
    let out_arg = format!("/out:{}", exe.display());
    let out = Command::new(csc)
        .args(["/nologo", "/codepage:65001", "/target:exe"])
        .arg(out_arg)
        .arg(src)
        .output()
        .map_err(|e| format!("csc не запустился: {}", e))?;
    if !out.status.success() {
        return Err(output_tail(&out.stdout, &out.stderr));
    }
    Ok(abs)
}

/// Один запуск хелпера: `args[0]` — режим (`runas`/`aumid`/`clsid`/`shell`),
/// дальше цель и хвост аргументов. Возвращает строку хелпера (`pid=1234`);
/// ненулевой код выхода — ошибка с хвостом вывода.
pub fn run(args: &[&str]) -> Result<String, String> {
    let exe = ensure_helper()?;
    let out = Command::new(exe)
        .args(args)
        .output()
        .map_err(|e| format!("vlaunch не запустился: {}", e))?;
    if !out.status.success() {
        return Err(output_tail(&out.stdout, &out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        Ok(String::from("started"))
    } else {
        Ok(text)
    }
}

/// Хвост вывода: полный текст (включая пустые строки компилятора) в чат не
/// тащим — хватает последних строк, на них и жалуются csc/dotnet/хелпер.
/// Кодировка: csc на pipe пишет OEM-кодовой страницей (cp866 здесь), поэтому
/// русские фразы компилятора покажутся мусором — ASCII-часть (CS-код, путь,
/// номер строки) остаётся читаемой. Декодера cp866 в дереве зависимостей нет,
/// а деп ради косметики не тянем.
pub fn output_tail(out: &[u8], err: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(out).into_owned();
    text.push_str(&String::from_utf8_lossy(err));
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::from("пустой вывод при ненулевом коде выхода");
    }
    let keep = lines.len().saturating_sub(12);
    lines[keep..].join("\n")
}
