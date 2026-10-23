(() => {
  // browser/src/main.ts
  var $ = (id) => document.getElementById(id);
  var el = (tag, cls = "", text = "") => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text) n.textContent = text;
    return n;
  };
  var status = (msg, bad = false) => {
    const s = $("status");
    s.textContent = msg;
    s.classList.toggle("bad", bad);
  };
  var history = [];
  var hpos = -1;
  async function navigate(url, push = true) {
    if (!url.trim()) return;
    status("\u0417\u0430\u0433\u0440\u0443\u0437\u043A\u0430: " + url + " \u2026");
    if (push) {
      history.splice(hpos + 1);
      history.push(url);
      hpos = history.length - 1;
    }
    try {
      const r = await fetch("/api/proxy", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ url })
      });
      if (!r.ok) throw new Error((await r.json()).detail ?? r.statusText);
      renderPage(await r.json());
      status("\u0413\u043E\u0442\u043E\u0432\u043E: " + url);
    } catch (e) {
      status("\u0421\u0431\u043E\u0439: " + e.message, true);
    }
  }
  function renderPage(p) {
    $("page-title").textContent = p.title || p.url;
    $("page-meta").textContent = p.url;
    $("page-text").textContent = p.text || "(\u043F\u0443\u0441\u0442\u0430\u044F \u0441\u0442\u0440\u0430\u043D\u0438\u0446\u0430)";
    const box = $("page-links");
    box.replaceChildren();
    if (p.links.length) {
      box.append(el("h3", "", `\u0421\u0441\u044B\u043B\u043A\u0438 (${p.links.length})`));
      for (const l of p.links) {
        const a = el("a", "link", l.text);
        a.href = "#";
        a.title = l.href;
        a.addEventListener("click", (ev) => {
          ev.preventDefault();
          navigate(l.href);
        });
        box.append(a);
      }
    }
    switchTab("page");
  }
  function goBack() {
    if (hpos > 0) {
      hpos -= 1;
      void navigate(history[hpos], false);
    }
  }
  function goForward() {
    if (hpos < history.length - 1) {
      hpos += 1;
      void navigate(history[hpos], false);
    }
  }
  async function loadSystem() {
    $("sys-status").textContent = "\u041E\u043F\u0440\u043E\u0441 \u044F\u0434\u0440\u0430\u2026";
    const one = async (path) => {
      const r = await fetch(path);
      if (!r.ok) throw new Error((await r.json()).detail ?? r.statusText);
      return await r.text();
    };
    try {
      void fetch("/api/os/devices").then(async (r) => {
        if (!r.ok) throw new Error((await r.json()).detail ?? r.statusText);
        renderDevices(await r.json());
      }).catch((e) => {
        $("sys-status").textContent = "\u0423\u0441\u0442\u0440\u043E\u0439\u0441\u0442\u0432\u0430: " + e.message;
      });
      const [hw, apps] = await Promise.all([one("/api/os/hardware"), one("/api/os/apps")]);
      $("sys-hardware").textContent = hw;
      $("sys-apps").textContent = apps;
      $("sys-status").textContent = "\u0413\u043E\u0442\u043E\u0432\u043E";
    } catch (e) {
      $("sys-status").textContent = "\u0421\u0431\u043E\u0439: " + e.message;
    }
  }
  function renderDevices(v) {
    const box = $("sys-devices");
    box.replaceChildren();
    box.append(el("h3", "", `\u0423\u0441\u0442\u0440\u043E\u0439\u0441\u0442\u0432\u0430 (${v.devices.length})`));
    const tree = el("div", "tree");
    let lastClass = "";
    for (const d of v.devices) {
      if (d.class !== lastClass) {
        lastClass = d.class;
        tree.append(el("div", "cls", lastClass || "(\u0431\u0435\u0437 \u043A\u043B\u0430\u0441\u0441\u0430)"));
      }
      const row = el("div", "row");
      const dot = el("span", "dot");
      const up = d.status.toUpperCase();
      dot.classList.add(up === "OK" ? "ok" : up === "" || up === "UNKNOWN" ? "unk" : "bad");
      row.append(dot, el("span", "", d.name || d.instance));
      if (d.virtual_dev) row.append(el("span", "virt", "\u0432\u0438\u0440\u0442\u0443\u0430\u043B\u044C\u043D\u044B\u0439"));
      tree.append(row);
    }
    box.append(tree);
    box.append(el("h3", "", `\u0412\u0438\u0440\u0442\u0443\u0430\u043B\u044C\u043D\u044B\u0435 \u0434\u0440\u0430\u0439\u0432\u0435\u0440\u044B (${v.drivers.length})`));
    for (const dr of v.drivers) {
      const row = el("div", "row");
      const btn = el("button", "mini", dr.enabled ? "\u0412\u043A\u043B" : "\u0412\u044B\u043A\u043B");
      btn.addEventListener("click", () => void toggleDriver(dr, btn));
      row.append(btn, el("span", "", dr.name), el("span", "cls-inline", dr.class));
      if (dr.desc) {
        const d = el("span", "desc", dr.desc);
        d.title = dr.desc;
        row.append(d);
      }
      box.append(row);
    }
  }
  async function toggleDriver(dr, btn) {
    btn.disabled = true;
    try {
      const r = await fetch("/api/os/driver", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id: dr.id, enabled: !dr.enabled })
      });
      if (!r.ok) throw new Error((await r.json()).detail ?? r.statusText);
      renderDevices(await r.json());
    } catch (e) {
      $("sys-status").textContent = "\u0414\u0440\u0430\u0439\u0432\u0435\u0440: " + e.message;
    } finally {
      btn.disabled = false;
    }
  }
  function switchTab(which) {
    $("pane-page").classList.toggle("hidden", which !== "page");
    $("pane-sys").classList.toggle("hidden", which !== "sys");
    $("tab-page").classList.toggle("active", which === "page");
    $("tab-sys").classList.toggle("active", which === "sys");
    if (which === "sys") void loadSystem();
  }
  $("go").addEventListener("click", () => void navigate($("addr").value));
  $("addr").addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") void navigate($("addr").value);
  });
  $("back").addEventListener("click", goBack);
  $("fwd").addEventListener("click", goForward);
  $("tab-page").addEventListener("click", () => switchTab("page"));
  $("tab-sys").addEventListener("click", () => switchTab("sys"));
  $("sys-refresh").addEventListener("click", () => void loadSystem());
  status("\u0413\u043E\u0442\u043E\u0432\u043E. \u0412\u0432\u0435\u0434\u0438\u0442\u0435 \u0430\u0434\u0440\u0435\u0441 \u0438\u043B\u0438 \u043E\u0442\u043A\u0440\u043E\u0439\u0442\u0435 \u0432\u043A\u043B\u0430\u0434\u043A\u0443 \xAB\u0421\u0438\u0441\u0442\u0435\u043C\u0430\xBB.");
})();
