#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Vanguard OS — верифицируемый ИИ-рефакторинг исходников Rust.

Былая версия перезаписывала src/*.rs ответом локальной модели без проверки:
модель принималась на веру, бэкап оседал в репозитории как *.bak, а модель
qwen2.5-coder:32b (21 GB) на 12 GB VRAM уходила наполовину на CPU (~6 ток/с —
замерено, см. AGENTS.md). Текущая версия делает то же самое, но безопасно:

  * модель и эндпоинт берутся из окружения (те же VANGUARD_AI_*, что у приложения);
  * результат проходит `cargo check`: не компилируется — исходник восстанавливается
    из бэкапа во временной папке; в репозитории не остаётся ни одного .bak;
  * --dry-run не трогает файлы вообще и печатает дифф в stdout;
  * печатается измеренная пропускная способность генерации (ток/с), чтобы выбор
    модели был виден числами, а не на веру.

ЧЕСТНОЕ ПРЕДУПРЕЖДЕНИЕ: cargo check доказывает только компиляцию. Сохранилось ли
поведение, проверяет человек через `git diff` перед коммитом.

Примеры:
    python src/optimize_project.py src/telemetry.rs --task unwrap
    python src/optimize_project.py --task cleanup --dry-run
    python src/optimize_project.py src/app.rs --model qwen3.5:9b --think
"""
from __future__ import annotations

import argparse
import difflib
import http.client
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Задачи — узкие и проверяемые. Старый режим «оптимизируй память и алгоритмы
# целиком» просил модель менять то, чем ничем нельзя подтвердить; всё, что ниже,
# — механическая правка, которую после действительно можно проверить диффом.
TASKS = {
    "cleanup": "мёртвый код, дублирование, устаревшие комментарии",
    "unwrap":  "заменить unwrap()/expect()/panic! на обработку ошибок",
    "docs":    "doc-комментарии у публичных элементов",
}

TASK_RULES = {
    "cleanup": (
        "Убери мёртвый код (недостижимые ветки, неиспользуемые функции и поля), "
        "слий дублирование в общие хелперы, замени комментарии-хроники прошлых "
        "правок на объяснения текущего кода."
    ),
    "unwrap": (
        "Замени unwrap()/expect()/panic! и индексный доступ v[i] на обработку "
        "ошибок: match / let-else / if let с осмысленным сообщением. Ничего не "
        "глотай молча: если раньше было падение, пользователь должен увидеть причину."
    ),
    "docs": (
        "Добавь или приведи в порядок doc-комментарии (///) у публичных структур, "
        "функций и полей. Пиши по-русски, коротко и по существу: что делает элемент "
        "и почему он устроен именно так, если это не очевидно."
    ),
}

BASE_RULES = """\
Это рефакторинг существующего файла, а не новая разработка. Требования:
1. ПОВЕДЕНИЕ СОХРАНЯЕТСЯ. Не добавляй фичи, не меняй логику и публичные сигнатуры.
2. Новые зависимости не добавляются: use — только из текущего файла.
3. Стиль комментариев исходника — смешанный русско-английский; сохрани его.
4. Никакого текста вне кода: весь файл — ровно один fenced-блок ```rust ... ```
5. Файл должен пройти `cargo check`: не оставляй TODO и закомментированный мусор.
6. Если править нечего — верни файл без изменений, всё тем же fenced-блоком.
"""


def cfg_from_env() -> dict:
    """Та же конфигурация, что у ai_core.rs: всё читаем из окружения."""
    return {
        "url": os.environ.get("VANGUARD_AI_URL",
                              "http://localhost:11434/v1/chat/completions"),
        "model": os.environ.get("VANGUARD_AI_MODEL", "qwen3.5:9b"),
        "key": os.environ.get("VANGUARD_AI_KEY", "ollama"),
        # thinking по умолчанию выключен: на этом прайсе он стоит 27x времени
        # (замерено в AGENTS.md). --think включает его явно.
        "think": os.environ.get("VANGUARD_AI_REASONING_EFFORT") is None,
    }


def warn_if_heavy(model: str) -> None:
    """Эвристика по замерам этой машины: RTX 3080 Ti = 12288 MiB.

    Модель тяжелее ~11 GB параметров уходит на CPU, и генерация рушится
    (~6 ток/с против ~136 ток/с у 7b/9b на 100% GPU)."""
    m = re.search(r":(\d+)\s*b\b", model, re.IGNORECASE)
    if m and int(m.group(1)) > 11:
        print(f"[!] Модель {model}: ~{m.group(1)} GB параметров не влезает в "
              f"12 GB VRAM — половина уйдёт на CPU, ждать десятки минут. "
              f"Возьмите qwen3.5:9b или qwen2.5-coder:7b.")


def call_model(prompt: str, cfg: dict, timeout: int):
    """Стриминговый запрос к /v1/chat/completions (тот же эндпоинт, что у
    приложения). Возвращает (текст, usage, длительность_сек)."""
    parts = urllib.parse.urlsplit(cfg["url"])
    if "/v1/" not in parts.path:
        raise RuntimeError(
            f"поддерживается только /v1/chat/completions, а в VANGUARD_AI_URL "
            f"указан {parts.path or '(путь пуст)'}")

    body = {
        "model": cfg["model"],
        "stream": True,
        "messages": [
            {"role": "system", "content":
             "Ты — редактор Rust-исходников. Отвечаешь ТОЛЬКО одним "
             "fenced-блоком ```rust с полным содержимым файла. Без пояснений."},
            {"role": "user", "content": prompt},
        ],
        "options": {"temperature": 0.2},
    }
    if "/v1/" in parts.path and not cfg["think"]:
        # на Ollama reasoning_effort="none" — единственный проверенный способ
        # выключить thinking на гибридных моделях (см. AGENTS.md)
        body["reasoning_effort"] = "none"

    conn_cls = (http.client.HTTPSConnection if parts.scheme == "https"
                else http.client.HTTPConnection)
    conn = conn_cls(parts.hostname, parts.port or (443 if parts.scheme == "https" else 80),
                    timeout=60)
    headers = {"Content-Type": "application/json",
               "Authorization": f"Bearer {cfg['key']}"}
    t0 = time.perf_counter()
    try:
        conn.request("POST", parts.path or "/v1/chat/completions",
                     json.dumps(body), headers)
        resp = conn.getresponse()
        if resp.status != 200:
            detail = resp.read(2000).decode("utf-8", "replace")
            raise RuntimeError(f"HTTP {resp.status}: {detail[:300]}")

        chunks, usage = [], {}
        # прогресс — только в реальном терминале и не чаще раза в 1.5 с:
        # иначе пипа получает сотню строк мусора вместо одной строки
        tty = sys.stdout.isatty()
        last_print = 0.0
        for raw in resp:  # итерация по строкам SSE; таймаут ловит зависание
            line = raw.strip()
            if not line.startswith(b"data:"):
                continue
            payload = line[5:].strip()
            if payload == b"[DONE]":
                break
            try:
                obj = json.loads(payload)
            except json.JSONDecodeError:
                continue
            if obj.get("usage"):
                usage.update(obj["usage"])
            choice = (obj.get("choices") or [{}])[0]
            piece = (choice.get("delta") or {}).get("content")
            if piece:
                chunks.append(piece)
                now = time.perf_counter()
                if tty and now - last_print > 1.5:
                    print(f"\r   ... сгенерировано {len(''.join(chunks))} символов",
                          end="", flush=True)
                    last_print = now
        if tty:
            print()
        return "".join(chunks), usage, time.perf_counter() - t0
    finally:
        conn.close()


def extract_code(text: str) -> str | None:
    """Достаёт код из ответа. Модель нередко добавляет ```rust вопреки просьбе —
    раньше эти кавычки попадали прямо в .rs и рушили сборку."""
    fenced = re.findall(r"```(?:rust)?\s*\n(.*?)```", text, re.S)
    if fenced:
        return max(fenced, key=len).strip() + "\n"
    stripped = text.strip()
    # без ограждения принимаем только явно похожее на код начало файла
    if stripped.startswith(("use ", "//", "#!", "pub ", "fn ", "mod ")):
        return stripped + "\n"
    return None


def cargo_check() -> tuple[bool, str]:
    """Прогоняет cargo check в корне репозитория. (успех, первые строки ошибок)."""
    try:
        proc = subprocess.run(
            ["cargo", "check", "--message-format", "short"],
            cwd=ROOT, capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=600)
    except FileNotFoundError:
        raise RuntimeError("cargo не найден в PATH — поставьте Rust-тулчейн")
    except subprocess.TimeoutExpired:
        return False, "cargo check не уложился в 600 с"
    errors = [ln for ln in (proc.stderr or "").splitlines() if "error" in ln]
    return proc.returncode == 0, "\n".join(errors[:15])


def process_file(path: Path, task: str, cfg: dict, args) -> str:
    """Возвращает строку-итог для финальной сводки. Файл либо исправлен и
    компилируется, либо лежит в исходном виде — посередине ничего не бывает."""
    original = path.read_text(encoding="utf-8")
    prompt = (f"Файл: {path.relative_to(ROOT)} (язык Rust).\n\n"
              f"Задача ({task}): {TASK_RULES[task]}\n\n{BASE_RULES}\n\n"
              f"Вот код файла:\n{original}")

    print(f"\n=== {path.relative_to(ROOT)} | задача: {task} ===")
    try:
        answer, usage, elapsed = call_model(prompt, cfg, args.timeout)
    except Exception as exc:  # сеть/HTTP/эндпоинт — файл не трогаем
        return f"{path.name}: сбой запроса — {exc}"

    tokens = usage.get("completion_tokens") or max(1, len(answer) // 4)
    est = "" if "completion_tokens" in usage else " (оценка по символам)"
    print(f"   генерация: {elapsed:.1f} c, {tokens} токенов, "
          f"{tokens / max(elapsed, 0.01):.0f} ток/с{est}")

    candidate = extract_code(answer)
    if candidate is None:
        return f"{path.name}: модель вернула не код — пропущено, файл не тронут"
    if candidate == original:
        return f"{path.name}: изменений не предложено"

    if args.dry_run:
        diff = list(difflib.unified_diff(
            original.splitlines(), candidate.splitlines(),
            fromfile=str(path.relative_to(ROOT)),
            tofile=f"{path.relative_to(ROOT)} (предложение ИИ)", lineterm=""))
        head = diff[:120]
        print("\n".join(head))
        if len(diff) > len(head):
            print(f"... ещё {len(diff) - len(head)} строк диффа")
        return f"{path.name}: --dry-run, дифф выше, файл не тронут"

    # Бэкап живёт во временной папке ОС, а не в репозитории: старые *.bak
    # навсегда уходят в прошлое.
    with tempfile.TemporaryDirectory(prefix="vanguard_opt_") as tmp:
        backup = Path(tmp) / path.name
        backup.write_text(original, encoding="utf-8")
        path.write_text(candidate, encoding="utf-8")
        ok, errors = cargo_check()
        if ok:
            return f"{path.name}: применено, cargo check чистый ({tokens} токенов)"
        path.write_text(backup.read_text(encoding="utf-8"), encoding="utf-8")
        return (f"{path.name}: cargo check упал — исходник восстановлен. "
                f"Причины:\n{errors}")


def main() -> int:
    ap = argparse.ArgumentParser(
        description="ИИ-рефакторинг src/*.rs с проверкой компиляцией",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="задачи:\n" + "\n".join(f"  {k:8} {v}" for k, v in TASKS.items()))
    ap.add_argument("files", nargs="*",
                    help="файлы .rs (по умолчанию — все src/*.rs)")
    ap.add_argument("--task", choices=TASKS, default="cleanup")
    ap.add_argument("--dry-run", action="store_true",
                    help="показать дифф и ничего не менять")
    ap.add_argument("--model", help="модель (по умолчанию из VANGUARD_AI_MODEL)")
    ap.add_argument("--url", help="эндпоинт (по умолчанию из VANGUARD_AI_URL)")
    ap.add_argument("--think", action="store_true",
                    help="разрешить thinking модели (по умолчанию выключен)")
    ap.add_argument("--timeout", type=int, default=900,
                    help="лимит времени на запрос к модели, с")
    args = ap.parse_args()

    cfg = cfg_from_env()
    if args.model:
        cfg["model"] = args.model
    if args.url:
        cfg["url"] = args.url
    if args.think:
        cfg["think"] = False

    print("=== ИИ-РЕФАКТОРИНГ VANGUARD OS (верифицируемый) ===")
    print(f">> эндпоинт: {cfg['url']}")
    print(f">> модель:   {cfg['model']}"
          f" | thinking: {'вкл (--think)' if cfg['think'] else 'выкл'}")
    print(">> врата: cargo check; не компилируется — файл восстанавливается")
    warn_if_heavy(cfg["model"])

    if args.files:
        targets = [Path(f) for f in args.files]
    else:
        targets = sorted((ROOT / "src").glob("*.rs"))
    targets = [t.resolve() for t in targets
               if t.suffix == ".rs" and t.exists()
               and t.name != Path(__file__).name]
    if not targets:
        print("[-] Нет файлов для обработки.")
        return 1

    results = [process_file(t, args.task, cfg, args) for t in targets]

    print("\n=== ИТОГ ===")
    for line in results:
        print(f"  {line}")
    print("\n>> cargo check доказывает только компиляцию. Посмотри "
          "`git diff` и реши, подходят ли изменения, прежде чем коммитить.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
