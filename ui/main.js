// Discord REC — interface. Aucune lib externe : window.__TAURI__ (CSP 'self').
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// Pas de menu contextuel du navigateur (sauf champs de saisie, pour coller).
document.addEventListener("contextmenu", (e) => {
  if (!e.target.closest("input, textarea")) e.preventDefault();
});

let startedAtMs = null;

/* ── Rendu de l'état ─────────────────────────────────────────── */

function setLed(el, on, color) {
  el.classList.toggle("lit-green", on && color === "green");
  el.classList.toggle("lit-red", on && color === "red");
  el.classList.toggle("lit-amber", on && color === "amber");
}

function render(status) {
  startedAtMs = status.recording ? status.started_at_ms : null;

  const tally = $("tally");
  tally.classList.toggle("on", status.recording);
  tally.classList.toggle("standby", !status.recording && status.enabled);

  setLed($("meter-voice"), status.in_voice, "green");
  setLed($("meter-rec"), status.recording, "red");
  setLed($("meter-video"), status.recording && status.video_active, "amber");

  document.querySelector(".deck").classList.toggle("live", status.recording);

  $("headline").textContent = !status.enabled
    ? "auto désactivé"
    : status.recording
      ? `enregistrement en cours${status.encoder ? ` — ${status.encoder}` : ""}`
      : status.in_voice
        ? "vocal détecté…"
        : "en attente du vocal…";

  $("current-file").textContent =
    status.file ?? "aucun enregistrement en cours";

  const err = $("last-error");
  err.hidden = !status.last_error;
  err.textContent = status.last_error ?? "";

  $("enabled").checked = status.enabled;
  tick();
}

function tick() {
  let text = "00:00:00";
  if (startedAtMs) {
    const s = Math.max(0, Math.floor((Date.now() - startedAtMs) / 1000));
    const pad = (n) => String(n).padStart(2, "0");
    text = `${pad(Math.floor(s / 3600))}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
  }
  $("timecode").textContent = text;
}

setInterval(tick, 500);

/* ── Réglages ────────────────────────────────────────────────── */

async function loadConfig() {
  const cfg = await invoke("get_config");
  $("cfg-dir").value = cfg.output_dir;
  $("cfg-video").checked = cfg.video;
  $("cfg-vbr").value = cfg.video_bitrate_kbps;
  $("cfg-abr").value = cfg.audio_bitrate_kbps;
  $("cfg-fps").value = cfg.framerate;
  $("cfg-debounce").value = cfg.stop_debounce_s;

  const select = $("cfg-mic");
  const mics = await invoke("list_mics").catch(() => []);
  for (const mic of mics) {
    const opt = document.createElement("option");
    opt.value = mic.id;
    opt.textContent = mic.label;
    select.appendChild(opt);
  }
  // Micro mémorisé mais débranché : on l'affiche quand même, marqué absent.
  if (cfg.mic_target && !mics.some((m) => m.id === cfg.mic_target)) {
    const opt = document.createElement("option");
    opt.value = cfg.mic_target;
    opt.textContent = "(micro mémorisé, non détecté)";
    select.appendChild(opt);
  }
  select.value = cfg.mic_target ?? "";
}

function flash(msg, ok) {
  const el = $("settings-msg");
  el.textContent = msg;
  el.className = ok ? "hint ok" : "hint err";
  setTimeout(() => {
    el.textContent = "";
  }, 4000);
}

$("settings").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    await invoke("set_config", {
      ui: {
        output_dir: $("cfg-dir").value.trim(),
        video: $("cfg-video").checked,
        video_bitrate_kbps: Number($("cfg-vbr").value),
        audio_bitrate_kbps: Number($("cfg-abr").value),
        framerate: Number($("cfg-fps").value),
        stop_debounce_s: Number($("cfg-debounce").value),
        mic_target: $("cfg-mic").value || null,
      },
    });
    flash("réglages enregistrés ✓", true);
    refreshRecordings();
  } catch (err) {
    flash(String(err), false);
  }
});

$("reset-token").addEventListener("click", async () => {
  try {
    await invoke("reset_window_token");
    flash("fenêtre oubliée — choix redemandé au prochain REC ✓", true);
  } catch (err) {
    flash(String(err), false);
  }
});

/* ── Enregistrements ─────────────────────────────────────────── */

function fmtSize(bytes) {
  if (bytes >= 1e9) return (bytes / 1e9).toFixed(2) + " Go";
  if (bytes >= 1e6) return (bytes / 1e6).toFixed(1) + " Mo";
  return Math.round(bytes / 1e3) + " ko";
}

async function refreshRecordings() {
  const files = await invoke("list_recordings");
  const ul = $("recordings");
  ul.replaceChildren();
  if (!files.length) {
    const li = document.createElement("li");
    li.className = "tape-empty";
    li.textContent = "aucun fichier";
    ul.appendChild(li);
    return;
  }
  for (const f of files) {
    const li = document.createElement("li");
    const name = document.createElement("span");
    name.className = "tape-name";
    name.textContent = f.name;
    const meta = document.createElement("span");
    meta.className = "tape-meta";
    meta.textContent = fmtSize(f.size_bytes);
    li.append(name, meta);
    ul.appendChild(li);
  }
}

/* ── Actions globales ────────────────────────────────────────── */

$("enabled").addEventListener("change", (e) => {
  invoke("set_enabled", { enabled: e.target.checked });
});

$("open-dir").addEventListener("click", () => invoke("open_recordings_dir"));
$("quit").addEventListener("click", () => invoke("quit_app"));

/* ── Mises à jour ────────────────────────────────────────────── */

async function checkUpdate() {
  let update = null;
  try {
    update = await invoke("check_update");
  } catch {
    return; // plateforme sans manifeste (.deb) ou hors-ligne : silencieux
  }
  if (!update) return;
  $("update-text").textContent = `Version ${update.version} disponible`;
  const btn = $("update-btn");
  btn.textContent = update.installable ? "Mettre à jour" : "Voir la release";
  btn.onclick = async () => {
    if (!update.installable) {
      invoke("open_releases_page");
      return;
    }
    btn.disabled = true;
    btn.textContent = "Téléchargement…";
    try {
      await invoke("install_update"); // redémarre l'app si OK
    } catch (err) {
      btn.disabled = false;
      btn.textContent = "Réessayer";
      $("update-text").textContent = `Échec de la mise à jour : ${err}`;
    }
  };
  $("update-banner").hidden = false;
}

/* ── Démarrage ───────────────────────────────────────────────── */

listen("status", (event) => render(event.payload));
listen("recording-saved", () => refreshRecordings());

(async () => {
  await loadConfig();
  await refreshRecordings();
  render(await invoke("get_status"));
  setTimeout(checkUpdate, 5000);
  setInterval(checkUpdate, 6 * 3600 * 1000); // re-vérifie toutes les 6 h
})();
