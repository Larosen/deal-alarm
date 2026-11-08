"use strict";

const $ = (sel, root = document) => root.querySelector(sel);
const SHOP_LABELS = { expert: "expert", euronics: "Euronics" };
let state = null;
let pollTimer = null;

const euro = (v) => v == null ? "–" : v.toLocaleString("de-DE", { style: "currency", currency: "EUR" });
const when = (iso) => iso ? new Date(iso).toLocaleString("de-DE", { dateStyle: "short", timeStyle: "short" }) : "–";

async function api(method, url, body) {
  const res = await fetch(url, {
    method,
    headers: body ? { "Content-Type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

async function load() {
  state = await api("GET", "/api/state");
  render();
  clearTimeout(pollTimer);
  pollTimer = setTimeout(load, state.scheduler.running ? 3000 : 30000);
}

function render() {
  const sch = state.scheduler;
  const status = $("#status");
  const names = Object.fromEntries(state.watches.map((w) => [w.id, w.name || "Gerät"]));
  const busy = Object.values(state.activity || {}).filter((a) => a.current || a.queue.length);
  if (busy.length) {
    status.replaceChildren(Object.assign(document.createElement("span"), { className: "dot" }));
    status.append(busy.map((a) => {
      const p = a.progress || {};
      let text = `${a.label}: `;
      text += a.current ? `${names[a.current] || "…"} ${p.planned ? `${p.done}/${p.planned}` : "startet"}` : "wartet";
      if (a.queue.length) text += ` (+${a.queue.length} in Warteschlange)`;
      return text;
    }).join(" · "));
  } else {
    const next = state.settings.scheduler_enabled ? `nächste: ${when(sch.next_run)}` : "Automatik aus";
    status.textContent = `Letzte Prüfung: ${when(sch.last_end)} · ${next}`;
  }

  const list = $("#watches");
  list.replaceChildren(...state.watches.map(renderWatch));
  $("#empty").classList.toggle("hidden", state.watches.length > 0);
}

function renderWatch(w) {
  const card = $("#watch-template").content.firstElementChild.cloneNode(true);
  $(".name", card).textContent = w.name || "(ohne Name)";
  $(".target", card).textContent = `Wunschpreis: ${euro(w.target_price)}`;
  $(".badge.paused", card).classList.toggle("hidden", w.enabled);
  card.classList.toggle("disabled", !w.enabled);

  const shops = $(".shops", card);
  let anyDeal = false;
  for (const shop of ["expert", "euronics"]) {
    const active = w[`${shop}_enabled`] && (w[`${shop}_query`] || w.ean);
    if (!active) continue;
    const r = (w.results || {})[shop];
    const el = document.createElement("div");
    el.className = "shop";
    let price = "–", detail = "Noch nicht geprüft";
    const act = (state.activity || {})[shop];
    const running = act && act.current === w.id;
    const queuePos = act ? act.queue.indexOf(w.id) : -1;
    if (running) {
      const p = act.progress || {};
      el.classList.add("running");
      detail = p.planned ? `Prüfung läuft: ${p.done}/${p.planned} Filialen, ${p.offers || 0} mit Angebot` : "Prüfung startet …";
      if (p.best_price != null) detail += ` · bisher günstigster ${euro(p.best_price)} (${p.best_store})`;
      if (p.throttled) detail += " · gedrosselt";
      if (r && !r.error && r.available) detail += ` · letzter Stand ${euro(r.price)}`;
    } else if (queuePos >= 0) {
      detail = `In Warteschlange (Platz ${queuePos + 1})`;
      if (r && !r.error && r.available) { price = euro(r.price); detail += ` · letzter Stand vom ${when(r.checked_at)}`; }
    } else if (r) {
      if (r.error) {
        el.classList.add("err");
        detail = r.error;
      } else {
        price = r.available ? euro(r.price) : "nicht verfügbar";
        detail = r.detail || "";
        if (r.is_deal) { el.classList.add("deal"); anyDeal = true; }
      }
      if (r.notify_error) detail += ` · ntfy-Fehler: ${r.notify_error}`;
      detail += ` · geprüft ${when(r.checked_at)}`;
    }
    el.innerHTML = `
      <div class="shop-head">
        <span class="shop-name"></span>
        <span class="price"></span>
      </div>
      <div class="shop-detail"></div>
      <div class="shop-scan"></div>`;
    const name = $(".shop-name", el);
    if (r && r.url) {
      const a = document.createElement("a");
      a.href = r.url; a.target = "_blank"; a.rel = "noopener";
      a.textContent = SHOP_LABELS[shop];
      name.append(a);
    } else {
      name.textContent = SHOP_LABELS[shop];
    }
    if (r && r.is_deal) {
      const badge = document.createElement("span");
      badge.className = "badge deal";
      badge.textContent = "DEAL";
      name.append(" ", badge);
    }
    $(".price", el).textContent = price;
    $(".shop-detail", el).textContent = detail;
    if (r && r.suggestion && !running) {
      const btn = document.createElement("button");
      btn.className = "suggest";
      btn.textContent = `Als ${SHOP_LABELS[shop]}-Produkt übernehmen`;
      btn.title = r.suggestion.url;
      btn.addEventListener("click", async (ev) => {
        ev.stopPropagation();
        try {
          await api("PUT", `/api/watches/${w.id}`, { [`${shop}_query`]: r.suggestion.url });
          await api("POST", "/api/check", { watch_id: w.id });
          await load();
        } catch (e) { alert(e.message); }
      });
      el.append(btn);
    }
    if (r && r.title) $(".shop-detail", el).title = r.title;
    if (r && r.scan) {
      const sc = r.scan;
      const scanEl = $(".shop-scan", el);
      const info = document.createElement("span");
      info.textContent = `Filialen: ${sc.checked}/${sc.planned} in diesem Lauf · ${sc.known}/${sc.total} bekannt`
        + (sc.aborted ? " · gedrosselt, Fortsetzung im nächsten Lauf" : "");
      const btn = document.createElement("button");
      btn.className = "linklike";
      btn.textContent = "Alle Filialpreise";
      btn.addEventListener("click", () => openStores(w, shop));
      scanEl.append(info, btn);
    }
    shops.append(el);
  }
  card.classList.toggle("deal", anyDeal);

  $('[data-action="toggle"]', card).textContent = w.enabled ? "Pausieren" : "Aktivieren";
  card.addEventListener("click", async (ev) => {
    const action = ev.target.dataset.action;
    if (!action) return;
    try {
      if (action === "edit") openWatchDialog(w);
      if (action === "delete" && confirm(`„${w.name}“ wirklich löschen?`)) await api("DELETE", `/api/watches/${w.id}`);
      if (action === "toggle") await api("PUT", `/api/watches/${w.id}`, { enabled: !w.enabled });
      if (action === "check") await api("POST", "/api/check", { watch_id: w.id });
      if (action !== "edit") await load();
    } catch (e) { alert(e.message); }
  });
  return card;
}

// ---------- Filialpreise ----------
const storesDialog = $("#stores-dialog");
let storesData = null;

async function openStores(w, shop) {
  $("#stores-title").textContent = `${w.name} – ${SHOP_LABELS[shop]}: Preise aller Filialen`;
  $("#stores-body").innerHTML = '<tr><td colspan="7">Lade …</td></tr>';
  storesDialog.showModal();
  try {
    storesData = await api("GET", `/api/watches/${w.id}/stores?shop=${shop}`);
    renderStores();
  } catch (e) { $("#stores-body").innerHTML = ""; $("#stores-summary").textContent = e.message; }
}

function renderStores() {
  const onlyMatch = $("#stores-only-match").checked;
  const rows = storesData.stores.filter((r) => !onlyMatch || r.matches_filter);
  const prices = storesData.stores.map((r) => r.total_price);
  const min = prices.length ? Math.min(...prices) : null, max = prices.length ? Math.max(...prices) : null;
  $("#stores-summary").textContent = `${storesData.stores.length} Filialen mit Angebot (Spanne ${euro(min)} – ${euro(max)}), `
    + `${storesData.unavailable} ohne Angebot · Wunschpreis ${euro(storesData.target_price)}`;
  const body = $("#stores-body");
  body.replaceChildren();
  rows.forEach((r, i) => {
    const tr = document.createElement("tr");
    if (r.total_price <= storesData.target_price) tr.classList.add("deal");
    if (!r.matches_filter) tr.classList.add("nomatch");
    if (r.stale) tr.classList.add("stale");
    const stock = [r.online_stock > 0 ? `online ${r.online_stock}` : "", r.store_stock > 0 ? `Markt ${r.store_stock}` : ""]
      .filter(Boolean).join(", ") || r.availability || "–";
    const name = r.distance_km != null ? `${r.store_name} · ${r.distance_km} km` : r.store_name;
    const cells = [String(i + 1), name, euro(r.price), euro(r.shipping), euro(r.total_price), stock, when(r.checked)];
    cells.forEach((text, idx) => {
      const td = document.createElement("td");
      if (idx === 1) {
        td.className = "name";
        if (r.url) { const a = document.createElement("a"); a.href = r.url; a.target = "_blank"; a.rel = "noopener"; a.textContent = text; td.append(a); }
        else td.textContent = text;
        if (r.on_display) td.append(Object.assign(document.createElement("span"), { className: "tag", textContent: "Ausstellungsstück" }));
        if (r.stale) td.append(Object.assign(document.createElement("span"), { className: "tag", textContent: "veraltet" }));
      } else td.textContent = text;
      if (idx >= 2 && idx <= 4) td.classList.add("num");
      if (idx === 4) td.classList.add("total");
      tr.append(td);
    });
    body.append(tr);
  });
  if (!rows.length) body.innerHTML = '<tr><td colspan="7">Keine Angebote</td></tr>';
}
$("#stores-only-match").addEventListener("change", () => storesData && renderStores());

// ---------- Formular-Helfer ----------
function fillForm(form, values) {
  for (const el of form.elements) {
    if (!el.name || !(el.name in values)) continue;
    if (el.type === "checkbox") el.checked = !!values[el.name];
    else el.value = values[el.name] ?? "";
  }
}
function readForm(form) {
  const out = {};
  for (const el of form.elements) {
    if (!el.name) continue;
    out[el.name] = el.type === "checkbox" ? el.checked : el.value;
  }
  return out;
}

// ---------- Gerät-Dialog ----------
const watchDialog = $("#watch-dialog");
const watchForm = $("#watch-form");
const WATCH_DEFAULTS = {
  id: "", name: "", target_price: "", enabled: true,
  expert_enabled: true, expert_query: "", expert_all_branches: true, expert_include_display: false,
  expert_include_local: false, expert_max_distance: 50,
  euronics_enabled: true, euronics_query: "", euronics_all_stores: true, euronics_max_distance: 0, ean: "",
};

function openWatchDialog(w) {
  watchForm.reset();
  const values = { ...WATCH_DEFAULTS, ...(w || {}) };
  if (w) values.target_price = String(w.target_price).replace(".", ",");
  fillForm(watchForm, values);
  $("#watch-dialog-title").textContent = w ? "Gerät bearbeiten" : "Gerät hinzufügen";
  $("#watch-error").textContent = "";
  $("#article-results").classList.add("hidden");
  $("#article-query").value = "";
  $("#article-status").textContent = "";
  watchDialog.showModal();
}

watchForm.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const values = readForm(watchForm);
  const id = values.id;
  delete values.id;
  try {
    if (id) await api("PUT", `/api/watches/${id}`, values);
    else await api("POST", "/api/watches", values);
    watchDialog.close();
    await load();
  } catch (e) { $("#watch-error").textContent = e.message; }
});

// Ein Artikel für beide Shops: Auswahl füllt Name, EAN und die Angaben für expert und Euronics
function applyArticle(article) {
  const f = watchForm.elements;
  if (article.title) f.name.value = article.title;
  f.ean.value = article.ean || "";
  f.expert_query.value = article.expert_query || "";
  f.euronics_query.value = article.euronics_query || "";
  f.expert_enabled.checked = true;
  f.euronics_enabled.checked = true;
  const status = [];
  status.push(article.expert_found ? "expert: gefunden ✔" : "expert: nicht im Sortiment – wird trotzdem regelmäßig über die EAN gesucht");
  status.push(article.euronics_query ? "Euronics: URL übernommen ✔" : "Euronics: wird über die EAN gesucht");
  $("#article-status").textContent = status.join(" · ");
  $("#article-results").classList.add("hidden");
}

async function searchArticle() {
  const q = $("#article-query").value.trim();
  const list = $("#article-results");
  if (!q) return;
  $("#article-status").textContent = "Suche …";
  list.classList.add("hidden");
  try {
    const res = await api("GET", `/api/article/resolve?q=${encodeURIComponent(q)}`);
    $("#article-status").textContent = "";
    if (res.article) return applyArticle(res.article);
    list.replaceChildren();
    list.classList.remove("hidden");
    if (!res.results.length) {
      list.innerHTML = "<li>Keine Treffer bei expert – füge eine Euronics-URL oder die EAN ein</li>";
      return;
    }
    for (const hit of res.results) {
      const li = document.createElement("li");
      li.textContent = `${hit.title}${hit.ean ? ` · EAN ${hit.ean}` : ""}`;
      li.addEventListener("click", () => applyArticle({ ...hit, euronics_query: "", expert_found: true }));
      list.append(li);
    }
  } catch (e) { $("#article-status").textContent = e.message; }
}
$("#btn-article-search").addEventListener("click", searchArticle);
$("#article-query").addEventListener("keydown", (ev) => { if (ev.key === "Enter") { ev.preventDefault(); searchArticle(); } });

// ---------- Einstellungen ----------
const settingsDialog = $("#settings-dialog");
const settingsForm = $("#settings-form");

$("#btn-settings").addEventListener("click", () => {
  settingsForm.reset();
  fillForm(settingsForm, state.settings);
  $("#limiter-info").textContent = Object.entries(state.limiters || {})
    .map(([n, l]) => `${n}: aktuell ${l.rate_per_min}/min, ${l.requests} Anfragen, ${l.throttled}× gedrosselt${l.paused ? " (Pause)" : ""}`)
    .join(" · ");
  $("#settings-error").textContent = "";
  previewStores();
  $("#test-result").textContent = "";
  settingsDialog.showModal();
});

settingsForm.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  try {
    await api("PUT", "/api/settings", readForm(settingsForm));
    settingsDialog.close();
    await load();
  } catch (e) { $("#settings-error").textContent = e.message; }
});

async function previewStores() {
  const out = $("#stores-preview");
  try {
    const f = settingsForm.elements;
    const res = await api("POST", "/api/euronics/stores/preview", { settings: {
      euronics_stores: f.euronics_stores.value, euronics_use_directory: f.euronics_use_directory.checked, plz: f.plz.value } });
    const rate = Number(f.euronics_rate_per_min.value) || 6;
    const parts = [];
    if (res.directory_count) parts.push(`Verzeichnis: ${res.directory_count} Shops (${res.directory_source})`);
    if (res.manual.length) parts.push(`eigene Einträge: ${res.manual.map((s) => s.name).join(", ")}`);
    const minutes = Math.max(0, ...Object.values(res.per_host)) / rate;
    parts.push(res.total
      ? `zusammen ${res.total} Märkte (${Object.entries(res.per_host).map(([h, n]) => `${h}: ${n}`).join(", ")}) – `
        + `ein vollständiger Durchlauf dauert pro Gerät ca. ${Math.ceil(minutes)} Min. bei ${rate} Anfragen/Min.`
      : "keine Märkte – es wird nur der Onlineshop geprüft");
    out.textContent = parts.join(" · ");
  } catch (e) { out.textContent = e.message; }
}
let previewTimer = null;
for (const name of ["euronics_stores", "euronics_use_directory", "euronics_rate_per_min", "plz"]) {
  settingsForm.elements[name].addEventListener("input", () => { clearTimeout(previewTimer); previewTimer = setTimeout(previewStores, 400); });
}

$("#btn-test-notify").addEventListener("click", async () => {
  const out = $("#test-result");
  out.textContent = "Speichere & sende …";
  try {
    await api("PUT", "/api/settings", readForm(settingsForm));
    const res = await api("POST", "/api/test-notify");
    out.textContent = `Von ntfy angenommen ✔ (Nachricht ${res.id} an ${res.target})`;
  } catch (e) { out.textContent = `Fehler: ${e.message}`; }
});

// ---------- Protokoll ----------
const logDialog = $("#log-dialog");
let logAfter = 0, logTimer = null;

async function loadLogs() {
  try {
    const lines = await api("GET", `/api/logs?after=${logAfter}`);
    const box = $("#log-lines");
    const atBottom = box.scrollTop + box.clientHeight >= box.scrollHeight - 20;
    for (const line of lines) {
      const row = document.createElement("div");
      row.className = `log-line ${line.level.toLowerCase()}`;
      const time = new Date(line.time).toLocaleTimeString("de-DE");
      row.textContent = `${time}  ${line.message}`;
      box.append(row);
      logAfter = line.id;
    }
    if (lines.length && atBottom) box.scrollTop = box.scrollHeight;
    if (!box.children.length) box.textContent = "Noch keine Einträge.";
  } catch (e) { $("#log-lines").textContent = e.message; }
  if (logDialog.open) logTimer = setTimeout(loadLogs, 3000);
}
$("#btn-log").addEventListener("click", () => {
  $("#log-lines").replaceChildren();
  logAfter = 0;
  logDialog.showModal();
  loadLogs();
});
logDialog.addEventListener("close", () => clearTimeout(logTimer));

// ---------- Allgemein ----------
document.querySelectorAll("[data-close]").forEach((b) => b.addEventListener("click", () => b.closest("dialog").close()));
$("#btn-add").addEventListener("click", () => openWatchDialog(null));
$("#btn-check-all").addEventListener("click", async () => {
  try { await api("POST", "/api/check"); await load(); } catch (e) { alert(e.message); }
});

load().catch((e) => { $("#status").textContent = `Fehler: ${e.message}`; });
