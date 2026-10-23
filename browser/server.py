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
import re
import socket
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
def core_call(payload: dict, timeout: float = 30.0) -> dict:
    """Один запрос к vanguard_core: строка JSON в TCP, ответ — до финального
    kind (события Event идут перед ним, как в ipc_client)."""
    with socket.create_connection(CORE, timeout=timeout) as s:
        s.sendall((json.dumps(payload) + "\n").encode("utf-8"))
        buf = b""
        while True:
            chunk = s.recv(65536)
            if not chunk:
                raise RuntimeError("ядро закрыло соединение")
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


@app.post("/api/proxy")
def proxy(body: UrlIn):
    """Загрузка страницы сервером: http/https, лимит 2 МБ и 15 с."""
    url = body.url.strip()
    if "://" not in url:
        url = "https://" + url
    if urllib.parse.urlparse(url).scheme not in ("http", "https"):
        raise HTTPException(400, "только http/https")
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
