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

  // Clip direct (A) : visible seulement pendant un enregistrement.
  $("deck-clip").hidden = !status.recording;

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

  $("cfg-denoise").checked = cfg.mic_denoise;
  $("cfg-audiomode").value = cfg.mix_audio ? "mixed" : "separate";
  $("cfg-keep-last").checked = cfg.keep_only_last;
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
        mix_audio: $("cfg-audiomode").value === "mixed",
        mic_denoise: $("cfg-denoise").checked,
        keep_only_last: $("cfg-keep-last").checked,
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

// Durée lisible en secondes -> "mm:ss" (ou "h:mm:ss").
function fmtTimecode(totalSeconds) {
  const s = Math.max(0, Math.round(totalSeconds));
  const pad = (n) => String(n).padStart(2, "0");
  const h = Math.floor(s / 3600);
  const rest = `${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
  return h > 0 ? `${h}:${rest}` : rest;
}

// "mm:ss" / "h:mm:ss" / "90" -> secondes (NaN si invalide).
function parseTimecode(text) {
  const parts = text.trim().split(":").map((p) => p.trim());
  if (parts.some((p) => p === "" || !/^\d+$/.test(p))) return NaN;
  return parts.reduce((acc, p) => acc * 60 + Number(p), 0);
}

/* ── Clip direct (A) : les N dernières minutes du REC en cours ── */

async function runLiveClip(minutes) {
  const msg = $("clip-live-msg");
  if (!Number.isFinite(minutes) || minutes <= 0) {
    msg.className = "clip-msg err";
    msg.textContent = "durée invalide";
    return;
  }
  const buttons = $("deck-clip").querySelectorAll("button");
  buttons.forEach((b) => (b.disabled = true));
  msg.className = "clip-msg";
  msg.textContent = "découpe…";
  try {
    const out = await invoke("clip_live", { minutes });
    msg.className = "clip-msg ok";
    msg.textContent = `clip prêt : ${out}`;
    refreshRecordings();
  } catch (err) {
    msg.className = "clip-msg err";
    msg.textContent = String(err);
  } finally {
    buttons.forEach((b) => (b.disabled = false));
    setTimeout(() => {
      if (msg.textContent) msg.textContent = "";
    }, 8000);
  }
}

// Champ personnalisé : durée saisie en secondes (clip_live attend des minutes).
function runLiveClipCustom() {
  const seconds = Number($("clip-live-custom").value);
  runLiveClip(seconds / 60);
}

$("deck-clip").addEventListener("click", (e) => {
  const preset = e.target.closest(".clip-live-btn");
  if (preset) runLiveClip(Number(preset.dataset.min));
});
$("clip-live-custom-btn").addEventListener("click", runLiveClipCustom);
$("clip-live-custom").addEventListener("keydown", (e) => {
  if (e.key === "Enter") runLiveClipCustom();
});

// Résolutions proposées à la conversion (valeur = hauteur, "" = source).
const RESOLUTIONS = [
  ["", "Source"],
  ["1080", "1080p"],
  ["720", "720p"],
  ["480", "480p"],
];

function showRecError(msg) {
  const el = $("last-error");
  el.hidden = false;
  el.textContent = msg;
}

// Sélecteur de résolution + bouton « → MP4 » pour un enregistrement MKV.
function buildConvertControls(f, meta) {
  const sel = document.createElement("select");
  sel.className = "tape-res";
  sel.title = "Résolution du MP4";
  for (const [value, label] of RESOLUTIONS) {
    const opt = document.createElement("option");
    opt.value = value;
    opt.textContent = label;
    sel.appendChild(opt);
  }
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "btn btn-small tape-conv";
  btn.textContent = "→ MP4";
  btn.addEventListener("click", async () => {
    const height = sel.value ? Number(sel.value) : null;
    sel.disabled = true;
    btn.disabled = true;
    btn.textContent = "…";
    meta.textContent = "conversion…";
    try {
      await invoke("convert_recording", { name: f.name, height });
      refreshRecordings(); // le MP4 apparaît dans la liste
    } catch (err) {
      sel.disabled = false;
      btn.disabled = false;
      btn.textContent = "→ MP4";
      meta.textContent = fmtSize(f.size_bytes);
      showRecError(`conversion : ${err}`);
    }
  });
  return [sel, btn];
}

/* ── Montage (B) : extrait point d'entrée → point de sortie ──── */

// Bouton ✂ + panneau pliable (début / durée / résolution / exporter) pour un
// enregistrement. La durée totale est chargée à la première ouverture.
function buildClipControls(f) {
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "btn btn-small tape-cut";
  toggle.textContent = "✂";
  toggle.title = "Découper un extrait";

  const panel = document.createElement("div");
  panel.className = "tape-clip";
  panel.hidden = true;

  const mkField = (labelText, input) => {
    const lab = document.createElement("label");
    lab.className = "tape-clip-field";
    const span = document.createElement("span");
    span.textContent = labelText;
    lab.append(span, input);
    return lab;
  };

  const start = document.createElement("input");
  start.type = "text";
  start.value = "0:00";
  start.spellcheck = false;
  start.setAttribute("aria-label", "Début (mm:ss)");

  const dur = document.createElement("input");
  dur.type = "number";
  dur.min = "1";
  dur.value = "30";
  dur.setAttribute("aria-label", "Durée (secondes)");

  const res = document.createElement("select");
  res.className = "tape-res";
  res.title = "Résolution du clip";
  for (const [value, label] of RESOLUTIONS) {
    const opt = document.createElement("option");
    opt.value = value;
    opt.textContent = label;
    res.appendChild(opt);
  }

  const go = document.createElement("button");
  go.type = "button";
  go.className = "btn btn-small btn-accent";
  go.textContent = "Exporter";

  const msg = document.createElement("span");
  msg.className = "clip-msg tape-clip-info";

  panel.append(mkField("de", start), mkField("durée (s)", dur), res, go, msg);

  let durationLoaded = false;
  toggle.addEventListener("click", async () => {
    panel.hidden = !panel.hidden;
    toggle.classList.toggle("on", !panel.hidden);
    if (panel.hidden || durationLoaded) return;
    durationLoaded = true;
    try {
      const total = await invoke("media_duration", { name: f.name });
      msg.className = "clip-msg tape-clip-info";
      msg.textContent = `durée totale ${fmtTimecode(total)}`;
    } catch {
      msg.textContent = "";
    }
  });

  go.addEventListener("click", async () => {
    const startS = parseTimecode(start.value);
    const durationS = Number(dur.value);
    if (!Number.isFinite(startS) || startS < 0) {
      msg.className = "clip-msg err";
      msg.textContent = "début invalide (mm:ss)";
      return;
    }
    if (!Number.isFinite(durationS) || durationS <= 0) {
      msg.className = "clip-msg err";
      msg.textContent = "durée invalide";
      return;
    }
    const height = res.value ? Number(res.value) : null;
    go.disabled = true;
    msg.className = "clip-msg";
    msg.textContent = "découpe…";
    try {
      const out = await invoke("clip_range", {
        name: f.name,
        startS,
        durationS,
        height,
      });
      msg.className = "clip-msg ok";
      msg.textContent = `clip prêt : ${out}`;
      refreshRecordings();
    } catch (err) {
      msg.className = "clip-msg err";
      msg.textContent = String(err);
    } finally {
      go.disabled = false;
    }
  });

  return { toggle, panel };
}

// Boutons « ouvrir » (lecteur système) et « supprimer » (2 clics) par fichier.
function buildFileActions(f) {
  const open = document.createElement("button");
  open.type = "button";
  open.className = "btn btn-small tape-open";
  open.textContent = "▶";
  open.title = "Ouvrir avec le lecteur";
  open.addEventListener("click", async () => {
    try {
      await invoke("open_recording", { name: f.name });
    } catch (err) {
      showRecError(`ouverture : ${err}`);
    }
  });

  const del = document.createElement("button");
  del.type = "button";
  del.className = "btn btn-small tape-del";
  del.textContent = "🗑";
  del.title = "Supprimer le fichier";
  let armed = false;
  let timer = null;
  const reset = () => {
    armed = false;
    del.classList.remove("armed");
    del.textContent = "🗑";
    if (timer) clearTimeout(timer);
  };
  del.addEventListener("click", async () => {
    if (!armed) {
      // 1er clic : on arme (confirmation) pour éviter les suppressions accidentelles.
      armed = true;
      del.classList.add("armed");
      del.textContent = "Supprimer ?";
      timer = setTimeout(reset, 3000);
      return;
    }
    reset();
    try {
      await invoke("delete_recording", { name: f.name });
      refreshRecordings();
    } catch (err) {
      showRecError(`suppression : ${err}`);
    }
  });

  return [open, del];
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
    const actions = document.createElement("span");
    actions.className = "tape-actions";
    const meta = document.createElement("span");
    meta.className = "tape-meta";
    meta.textContent = fmtSize(f.size_bytes);
    actions.appendChild(meta);
    if (f.name.toLowerCase().endsWith(".mkv")) {
      actions.append(...buildConvertControls(f, meta));
    }
    // Montage : découpe un extrait de n'importe quel enregistrement (mkv/mp4).
    const { toggle, panel } = buildClipControls(f);
    actions.appendChild(toggle);
    actions.append(...buildFileActions(f));
    li.append(name, actions, panel);
    ul.appendChild(li);
  }
}

/* ── Actions globales ────────────────────────────────────────── */

$("enabled").addEventListener("change", (e) => {
  invoke("set_enabled", { enabled: e.target.checked });
});

$("open-dir").addEventListener("click", async () => {
  try {
    await invoke("open_recordings_dir");
  } catch (err) {
    const el = $("last-error");
    el.hidden = false;
    el.textContent = `ouverture du dossier : ${err}`;
  }
});
$("quit").addEventListener("click", () => invoke("quit_app"));

/* ── Mises à jour ────────────────────────────────────────────── */

async function checkUpdate() {
  let update = null;
  try {
    update = await invoke("check_update");
  } catch {
    $("update-banner").hidden = true;
    return; // plateforme sans manifeste (.deb) ou hors-ligne : silencieux
  }
  if (!update) {
    $("update-banner").hidden = true; // déjà à jour : pas de bannière
    return;
  }
  $("update-text").textContent = update.installable
    ? `Version ${update.version} disponible (installée : ${update.current})`
    : `Version ${update.version} disponible (installée : ${update.current}) — mise à jour via le paquet`;
  const btn = $("update-btn");
  btn.textContent = update.installable ? "Mettre à jour" : "Télécharger";
  btn.onclick = async () => {
    if (!update.installable) {
      // Linux/.deb : l'updater ne réinstalle pas un paquet, on ouvre la
      // page de release pour télécharger le nouveau .deb.
      try {
        await invoke("open_releases_page");
      } catch (err) {
        $("update-text").textContent = `Impossible d'ouvrir la page de release : ${err}`;
      }
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

// Vérifie les mises à jour sans relancer l'app : au démarrage, périodiquement
// et au retour sur la fenêtre — mais au plus une fois toutes les 10 min (le
// manifeste est un fichier statique, inutile de le marteler).
let lastUpdateCheck = 0;
function checkUpdateThrottled(force = false) {
  const now = Date.now();
  if (!force && now - lastUpdateCheck < 10 * 60 * 1000) return;
  lastUpdateCheck = now;
  checkUpdate();
}

/* ── Démarrage ───────────────────────────────────────────────── */

listen("status", (event) => render(event.payload));
listen("recording-saved", () => refreshRecordings());

// Au retour du focus / de visibilité : on relit le dossier (fichiers modifiés
// hors de l'app) ET on revérifie les mises à jour, pour ne plus avoir à
// relancer l'app pour voir une nouvelle version.
// Rafraîchissement auto : on ne reconstruit PAS la liste si un panneau de
// montage est ouvert (sinon la saisie début/durée serait perdue).
function autoRefreshRecordings() {
  if (document.querySelector(".tape-clip:not([hidden])")) return;
  refreshRecordings();
}

window.addEventListener("focus", () => {
  autoRefreshRecordings();
  checkUpdateThrottled();
});
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) {
    autoRefreshRecordings();
    checkUpdateThrottled();
  }
});

(async () => {
  $("app-version").textContent = `v${await invoke("get_app_version")}`;
  await loadConfig();
  await refreshRecordings();
  render(await invoke("get_status"));
  setTimeout(() => checkUpdateThrottled(true), 5000);
  setInterval(() => checkUpdateThrottled(true), 30 * 60 * 1000); // toutes les 30 min
  setInterval(() => {
    if (!document.hidden) autoRefreshRecordings();
  }, 5000);
})();
