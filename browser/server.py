# -*- coding: utf-8 -*-
"""Vanguard Browser — локальный сервер браузера (стадия 1).

FastAPI на 127.0.0.1:8787: раздаёт фронтенд (TS -> browser/static) и мостит
запросы к ядру Vanguard OS (TCP 127.0.0.1:7331) и к внешнему вебу (прокси).
Оболочка поднимает этот процесс по кнопке «Браузер» и открывает системный
браузер на этом адресе; стадия 2 встроит тот же адрес в WebView2.

Внешние страницы грузятся СЕРВЕРОМ (не фронтендом): CORS и заголовки
сайтов обходят нас стороной, а наружу стучимся только с 127.0.0.1.
"""
import json
import os
import re
import socket
import subprocess
import time
import html as htmllib
import urllib.parse
import urllib.request
from pathlib import Path

from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

BASE = Path(__file__).resolve().parent
STATIC = BASE / "static"
CORE = ("127.0.0.1", 7331)

app = FastAPI(title="Vanguard Browser", version="0.1")


# ── Мост к ядру (Rust): те же JSON-строки, что у оболочки ─────────────────
def _spawn_core() -> bool:
    """Поднимает ядро, если его никто не обслуживает — тот же контур, что у
    ipc_client оболочки, но по физическим кандидатам (dev- или dist-раскладка).
    Первый существующий бинарь, окно консоли не рисуем."""
    root = BASE.parent
    for rel in (
        "target/debug/vanguard_core.exe",
        "target/release/vanguard_core.exe",
        "vanguard_core.exe",
    ):
        exe = root / rel
        if not exe.is_file():
            continue
        kwargs: dict = {
            "cwd": str(root),
            "stdin": subprocess.DEVNULL,
            "stdout": subprocess.DEVNULL,
            "stderr": subprocess.DEVNULL,
        }
        if os.name == "nt":
            kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW
        try:
            subprocess.Popen([str(exe)], **kwargs)
            return True
        except OSError:
            return False
    return False


def _wait_core(seconds: float = 3.0) -> None:
    """Порт после спавна: ядро биндится почти сразу, но не мгновенно."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        time.sleep(0.1)
        try:
            with socket.create_connection(CORE, timeout=0.2):
                return
        except OSError:
            continue


def core_call(payload: dict, timeout: float = 30.0) -> dict:
    """Один запрос к vanguard_core: строка JSON в TCP, ответ — до финального
    kind (события Event идут перед ним, как в ipc_client). Свежий запуск —
    ядро ещё не поднято: браузер поднимает его сам. Любая ошибка уходит
    HTTPException, то есть всегда JSON {detail} — плоский 500 фронтенд не
    распарсит («not valid JSON»)."""
    try:
        s = socket.create_connection(CORE, timeout=5.0)
    except OSError:
        _spawn_core()
        _wait_core()
        try:
            s = socket.create_connection(CORE, timeout=5.0)
        except OSError as e:
            raise HTTPException(503, f"ядро недоступно на 127.0.0.1:7331: {e}")
    with s:
        s.settimeout(timeout)
        try:
            s.sendall((json.dumps(payload) + "\n").encode("utf-8"))
            buf = b""
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    raise HTTPException(502, "ядро закрыло соединение")
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    if not line.strip():
                        continue
                    resp = json.loads(line.decode("utf-8"))
                    if resp.get("kind") in ("ok", "err"):
                        if resp["kind"] == "err":
                            raise HTTPException(502, resp.get("text", "ошибка ядра"))
                        return resp
        except OSError as e:
            raise HTTPException(502, f"сбой обмена с ядром: {e}")


class ShellIn(BaseModel):
    query: str


class UrlIn(BaseModel):
    url: str


class DriverIn(BaseModel):
    id: str
    enabled: bool


@app.get("/api/os/hardware")
def os_hardware():
    """Аппаратура через /hardware ядра (sysinfo того же компьютера)."""
    return {"text": core_call({"cmd": "shell", "query": "/hardware"})["text"]}


@app.get("/api/os/devices")
def os_devices():
    """Дерево диспетчера устройств: ответ ядра — готовый DeviceView."""
    r = core_call({"cmd": "hw_devices"}, timeout=40.0)
    return json.loads(r["text"])


@app.get("/api/os/apps")
def os_apps():
    """Реестр сгенерированных скриптов (/apps ядра)."""
    return {"text": core_call({"cmd": "shell", "query": "/apps"})["text"]}


@app.post("/api/os/shell")
def os_shell(body: ShellIn):
    """Любая слеш-команда шелла — терминальный мост для вкладки «Система»."""
    q = body.query.strip()
    if not q.startswith("/"):
        raise HTTPException(400, "только слеш-команды (например, /help)")
    return {"text": core_call({"cmd": "shell", "query": q})["text"]}


@app.post("/api/os/driver")
def os_driver(body: DriverIn):
    """Тумблер виртуального драйвера — SetDriver ядра, ответ тот же DeviceView."""
    r = core_call({"cmd": "set_driver", "id": body.id, "enabled": body.enabled})
    return json.loads(r["text"])


# ── Прокси внешнего веба + reader-экстракция ──────────────────────────────
def extract(url: str, page: str) -> dict:
    """Грубый reader: заголовок, текст без скриптов/тегов и список ссылок.
    Полноценный парсер здесь не нужен — рендер текстовый, «как в lynx»."""
    m = re.search(r"<title[^>]*>(.*?)</title>", page, re.I | re.S)
    title = htmllib.unescape(m.group(1)).strip() if m else url
    body = re.sub(r"(?is)<(script|style|noscript|svg|canvas|head)\b.*?</\1>", " ", page)
    links, seen = [], set()
    for m in re.finditer(
        r"<a\b[^>]*href\s*=\s*[\"']([^\"']+)[\"'][^>]*>(.*?)</a>", body, re.I | re.S
    ):
        href = urllib.parse.urljoin(url, m.group(1))
        text = " ".join(htmllib.unescape(re.sub(r"<[^>]+>", " ", m.group(2))).split())
        if href.startswith(("http://", "https://")) and text and href not in seen:
            seen.add(href)
            links.append({"text": text[:120], "href": href})
            if len(links) >= 200:
                break
    text = htmllib.unescape(re.sub(r"(?s)<[^>]+>", "\n", body))
    lines = [" ".join(l.split()) for l in text.splitlines()]
    return {
        "kind": "page",
        "url": url,
        "title": title,
        "text": "\n".join(l for l in lines if l)[:400_000],
        "links": links,
    }


def normalize_url(url: str) -> str:
    """HTTP-клиенты Python кодируют URL латиницей: кириллический домен роняет
    запрос ещё до DNS ('latin-1' codec can't encode…). Домен → punycode (idna),
    путь/запрос/якорь → percent-encoding; уже закодированные %XX не трогаем
    (`%` в safe-списках)."""
    sp = urllib.parse.urlsplit(url)
    host = sp.hostname or ""
    if host and not host.isascii():
        try:
            host = host.encode("idna").decode("ascii")
        except UnicodeError:
            host = urllib.parse.quote(host, safe="")
        try:
            port = sp.port
        except ValueError:
            port = None
        netloc = f"[{host}]" if ":" in host else host
        if port is not None:
            netloc += f":{port}"
        sp = urllib.parse.urlsplit(
            urllib.parse.urlunsplit((sp.scheme, netloc, sp.path, sp.query, sp.fragment))
        )
    return urllib.parse.urlunsplit(
        (
            sp.scheme,
            sp.netloc,
            urllib.parse.quote(sp.path, safe="/%:@!$&'()*+,;=-._~"),
            urllib.parse.quote(sp.query, safe="&=%/:;+,$@!?'()*~-._"),
            urllib.parse.quote(sp.fragment, safe="&=%/:;+,$@!?'()*~-._"),
        )
    )


@app.post("/api/proxy")
def proxy(body: UrlIn):
    """Загрузка страницы сервером: http/https, лимит 2 МБ и 15 с."""
    url = body.url.strip()
    if "://" not in url:
        url = "https://" + url
    if urllib.parse.urlsplit(url).scheme not in ("http", "https"):
        raise HTTPException(400, "только http/https")
    url = normalize_url(url)
    req = urllib.request.Request(url, headers={"User-Agent": "VanguardBrowser/0.1"})
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            raw = resp.read(2_000_000)
            ctype = resp.headers.get("Content-Type", "")
            final = resp.geturl()
    except Exception as e:
        raise HTTPException(502, f"загрузка не удалась: {e}")
    enc = "utf-8"
    if "charset=" in ctype:
        enc = ctype.split("charset=")[-1].split(";")[0].strip() or "utf-8"
    try:
        page = raw.decode(enc, errors="replace")
    except LookupError:
        page = raw.decode("utf-8", errors="replace")
    if ctype and "html" not in ctype and "xml" not in ctype:
        # Не HTML (txt, json, csv…) — показываем как есть, без экстракции.
        return {
            "kind": "raw",
            "url": final,
            "title": final,
            "text": page[:400_000],
            "links": [],
        }
    return extract(final, page)


# ── Статика фронтенда ──────────────────────────────────────────────────────
@app.get("/")
def index():
    return FileResponse(STATIC / "index.html")


app.mount("/static", StaticFiles(directory=STATIC), name="static")

if __name__ == "__main__":
    import uvicorn

    # Только 127.0.0.1: наружу сервер не торчит ни при каких условиях.
    uvicorn.run(app, host="127.0.0.1", port=8787, log_level="warning")
