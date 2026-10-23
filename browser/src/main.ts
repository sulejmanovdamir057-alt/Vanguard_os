// Vanguard Browser — фронтенд на TypeScript (сборка: npx esbuild src/main.ts
// --bundle --outfile=static/app.js --format=iife --target=es2020).
// Табличный текстовый браузер: страницы грузит серверный /api/proxy, системная
// вкладка ходит в ядро Vanguard OS через FastAPI-мост. Вся логика здесь,
// статика — только цвета и раскладка в index.html.

interface Link { text: string; href: string }
interface PageReply {
  kind: string; url: string; title: string; text: string; links: Link[];
}
interface DeviceEntry {
  name: string; class: string; status: string; instance: string; virtual_dev: boolean;
}
interface VirtualDriver { id: string; name: string; class: string; desc: string; enabled: boolean }
interface DeviceView { devices: DeviceEntry[]; drivers: VirtualDriver[] }

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const el = <K extends keyof HTMLElementTagNameMap>(
  tag: K, cls = "", text = "",
): HTMLElementTagNameMap[K] => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text) n.textContent = text;
  return n;
};

const status = (msg: string, bad = false) => {
  const s = $("status");
  s.textContent = msg;
  s.classList.toggle("bad", bad);
};

// Ошибка API: у HTTPException тело JSON {detail}, у необработанного 500 —
// плоский текст; падение r.json() на нём как раз и даёт «not valid JSON».
async function errOf(r: Response): Promise<string> {
  try {
    const j = await r.json() as { detail?: string };
    return j.detail ?? `${r.status} ${r.statusText}`;
  } catch {
    return `${r.status} ${r.statusText}`;
  }
}

// ── История вкладки «Страница» ───────────────────────────────────────────
const history: string[] = [];
let hpos = -1;

async function navigate(url: string, push = true): Promise<void> {
  if (!url.trim()) return;
  status("Загрузка: " + url + " …");
  if (push) {
    history.splice(hpos + 1);
    history.push(url);
    hpos = history.length - 1;
  }
  try {
    const r = await fetch("/api/proxy", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ url }),
    });
    if (!r.ok) throw new Error(await errOf(r));
    renderPage(await r.json() as PageReply);
    status("Готово: " + url);
  } catch (e) {
    status("Сбой: " + (e as Error).message, true);
  }
}

function renderPage(p: PageReply): void {
  $("page-title").textContent = p.title || p.url;
  $("page-meta").textContent = p.url;
  $("page-text").textContent = p.text || "(пустая страница)";
  const box = $("page-links");
  box.replaceChildren();
  if (p.links.length) {
    box.append(el("h3", "", `Ссылки (${p.links.length})`));
    for (const l of p.links) {
      const a = el("a", "link", l.text);
      a.href = "#";
      a.title = l.href;
      a.addEventListener("click", (ev) => { ev.preventDefault(); navigate(l.href); });
      box.append(a);
    }
  }
  // Панель страницы впереди системной вкладки — переключаемся сами.
  switchTab("page");
}

function goBack(): void {
  if (hpos > 0) { hpos -= 1; void navigate(history[hpos], false); }
}
function goForward(): void {
  if (hpos < history.length - 1) { hpos += 1; void navigate(history[hpos], false); }
}

// ── Вкладка «Система»: аппаратура, устройства, реестр приложений ─────────
async function loadSystem(): Promise<void> {
  $("sys-status").textContent = "Опрос ядра…";
  const one = async (path: string): Promise<string> => {
    const r = await fetch(path);
    if (!r.ok) throw new Error(await errOf(r));
    return await r.text();
  };
  try {
    // Устройства — медленный опрос PowerShell в ядре, грузим отдельно от
    // быстрых эндпоинтов, чтобы медленный не держал остальные.
    void fetch("/api/os/devices").then(async (r) => {
      if (!r.ok) throw new Error(await errOf(r));
      renderDevices(await r.json() as DeviceView);
    }).catch((e) => { $("sys-status").textContent = "Устройства: " + e.message; });

    const [hw, apps] = await Promise.all([one("/api/os/hardware"), one("/api/os/apps")]);
    $("sys-hardware").textContent = hw;
    $("sys-apps").textContent = apps;
    $("sys-status").textContent = "Готово";
  } catch (e) {
    $("sys-status").textContent = "Сбой: " + (e as Error).message;
  }
}

function renderDevices(v: DeviceView): void {
  const box = $("sys-devices");
  box.replaceChildren();
  box.append(el("h3", "", `Устройства (${v.devices.length})`));
  // Дерево по классам: список в ядре уже отсортирован (класс, потом имя).
  const tree = el("div", "tree");
  let lastClass = "";
  for (const d of v.devices) {
    if (d.class !== lastClass) {
      lastClass = d.class;
      tree.append(el("div", "cls", lastClass || "(без класса)"));
    }
    const row = el("div", "row");
    const dot = el("span", "dot");
    const up = d.status.toUpperCase();
    dot.classList.add(up === "OK" ? "ok" : up === "" || up === "UNKNOWN" ? "unk" : "bad");
    row.append(dot, el("span", "", d.name || d.instance));
    if (d.virtual_dev) row.append(el("span", "virt", "виртуальный"));
    tree.append(row);
  }
  box.append(tree);
  box.append(el("h3", "", `Виртуальные драйверы (${v.drivers.length})`));
  for (const dr of v.drivers) {
    const row = el("div", "row");
    const btn = el("button", "mini", dr.enabled ? "Вкл" : "Выкл");
    btn.addEventListener("click", () => void toggleDriver(dr, btn));
    row.append(btn, el("span", "", dr.name), el("span", "cls-inline", dr.class));
    if (dr.desc) { const d = el("span", "desc", dr.desc); d.title = dr.desc; row.append(d); }
    box.append(row);
  }
}

async function toggleDriver(dr: VirtualDriver, btn: HTMLButtonElement): Promise<void> {
  btn.disabled = true;
  try {
    const r = await fetch("/api/os/driver", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ id: dr.id, enabled: !dr.enabled }),
    });
    if (!r.ok) throw new Error(await errOf(r));
    renderDevices(await r.json() as DeviceView);
  } catch (e) {
    $("sys-status").textContent = "Драйвер: " + (e as Error).message;
  } finally {
    btn.disabled = false;
  }
}

// ── Переключение вкладок ──────────────────────────────────────────────────
function switchTab(which: "page" | "sys"): void {
  $("pane-page").classList.toggle("hidden", which !== "page");
  $("pane-sys").classList.toggle("hidden", which !== "sys");
  $("tab-page").classList.toggle("active", which === "page");
  $("tab-sys").classList.toggle("active", which === "sys");
  if (which === "sys") void loadSystem();
}

// ── Инициализация ─────────────────────────────────────────────────────────
$("go").addEventListener("click", () => void navigate($<HTMLInputElement>("addr").value));
$<HTMLInputElement>("addr").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") void navigate($<HTMLInputElement>("addr").value);
});
$("back").addEventListener("click", goBack);
$("fwd").addEventListener("click", goForward);
$("tab-page").addEventListener("click", () => switchTab("page"));
$("tab-sys").addEventListener("click", () => switchTab("sys"));
$("sys-refresh").addEventListener("click", () => void loadSystem());
status("Готово. Введите адрес или откройте вкладку «Система».");
