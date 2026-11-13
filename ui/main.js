// Osky Audio Player: interfaz. Toda la lógica pesada (audio, cola, mover
// archivos) vive en Rust; aquí solo se pinta y se mandan comandos.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const dialog = window.__TAURI__.dialog;

const $ = (id) => document.getElementById(id);
const el = (tag, props = {}, ...children) => {
  const n = Object.assign(document.createElement(tag), props);
  n.append(...children.filter((c) => c != null));
  return n;
};

let track = null;      // TrackView actual
let status = null;     // PlayerStatus más reciente
let config = null;
let busy = false;      // evita dos movimientos a la vez
let conflict = null;   // { dest, existing } cuando el destino ya tiene el archivo
let dragging = false;
let wave = null;       // { path, rms, peak } calculada en Rust, llega por evento

// ---------------------------------------------------------------- helpers

const fmtTime = (s) => {
  if (!Number.isFinite(s) || s < 0) s = 0;
  const m = Math.floor(s / 60);
  const sec = Math.floor(s % 60);
  return `${m}:${String(sec).padStart(2, '0')}`;
};

let toastTimer = null;
function toast(msg, kind = '') {
  const t = $('toast');
  t.textContent = msg;
  t.className = `toast ${kind}`;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), kind === 'err' ? 5000 : 2600);
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e), 'err');
    throw e;
  }
}

const stem = (name) => name.replace(/\.[^.]+$/, '');
const parentDir = (p) => p.replace(/[\\/][^\\/]*$/, '');

// ---------------------------------------------------------------- render

function qualityTags(m) {
  const tags = [];
  const lossless = ['FLAC', 'WAV'].includes(m.format);
  tags.push([m.format, lossless ? 'ok' : '']);
  if (m.bitDepth) tags.push([`${m.bitDepth}-bit`, '']);
  if (m.sampleRate) tags.push([`${(m.sampleRate / 1000).toFixed(1).replace('.0', '')} kHz`, '']);
  if (m.bitrate) {
    let cls = '';
    if (!lossless) cls = m.bitrate >= 320 ? 'ok' : m.bitrate >= 256 ? 'warn' : 'bad';
    tags.push([`${m.bitrate} kbps`, cls]);
  }
  if (m.duration) tags.push([fmtTime(m.duration), '']);
  if (m.genre) tags.push([m.genre, '']);
  return tags;
}

function renderTrack() {
  const has = !!track;
  $('empty').hidden = has;
  $('track').hidden = !has;
  $('timeline').hidden = !has;
  $('controls').hidden = !has;
  hideConflict();

  if (!has) {
    $('crumbs').textContent = '';
    $('queue').textContent = '';
    renderMoves();
    return;
  }

  const m = track.meta;
  $('artist').textContent = m.artist || '';
  $('title').textContent = m.title || stem(track.fileName);
  $('filename').textContent = track.fileName;
  $('tech').replaceChildren(...qualityTags(m).map(([t, c]) => el('span', { className: `tag ${c}`, textContent: t })));
  $('error').hidden = !track.error;
  $('error').textContent = track.error || '';

  const crumbs = $('crumbs');
  crumbs.replaceChildren();
  if (track.root) {
    crumbs.append(el('b', { textContent: track.root.name }));
    if (track.folder !== track.root.name) {
      crumbs.append(el('span', { className: 'sep', textContent: '›' }), track.folder);
    }
  } else {
    crumbs.append(track.folder);
  }
  crumbs.title = track.path;

  $('queue').textContent = track.root
    ? `${track.remainingInFolder} en carpeta · ${track.queueLen} por clasificar`
    : `${track.queueIndex + 1} / ${track.queueLen}`;

  renderMoves();
}

function keyFor(i) {
  return i < 9 ? String(i + 1) : i === 9 ? '0' : null;
}

function renderMoves() {
  const list = $('dest-list');
  list.replaceChildren();
  $('btn-undo').disabled = !(track?.undoAvailable || lastUndoAvailable);
  const root = track?.root;
  $('no-root').hidden = !track || !!root;
  if (!root) return;

  if (root.destinations.length === 0) {
    list.append(el('div', { className: 'empty-dests', textContent: 'Esta raíz aún no tiene destinos.' }),
      el('button', { className: 'btn', textContent: 'Agregar destinos', onclick: openSettings }));
    return;
  }
  root.destinations.forEach((d, i) => {
    const key = keyFor(i);
    const b = el('button', {
      className: 'dest',
      title: d.path,
      disabled: d.isCurrent,
      onclick: () => moveTo(i),
    },
      key ? el('kbd', { textContent: key }) : null,
      el('span', { className: 'dest-label', textContent: d.label }),
      d.isCurrent ? el('span', { className: 'dest-here', textContent: 'aquí está' }) : null,
    );
    b.dataset.index = i;
    list.append(b);
  });
}

function renderStatus() {
  if (!track || !status) return;
  const dur = status.duration || track.meta.duration || 0;
  const pos = status.position || 0;
  const pct = dur ? Math.min(100, (pos / dur) * 100) : 0;
  if (!dragging) {
    $('tl-played').style.width = `${pct}%`;
    $('tl-head').style.left = `${pct}%`;
    $('t-pos').textContent = fmtTime(pos);
  }
  $('t-dur').textContent = fmtTime(dur);

  // En SVG la propiedad .hidden no existe: hay que usar el atributo.
  $('ico-play').toggleAttribute('hidden', status.playing);
  $('ico-pause').toggleAttribute('hidden', !status.playing);

  const auto = $('btn-auto');
  auto.classList.toggle('on', status.scanActive);
  $('auto-label').textContent = status.scanActive ? 'AUTO' : 'MANUAL';

  // Marcas del escaneo sobre la línea de tiempo.
  const marks = $('tl-marks');
  const key = status.scanActive && dur ? `${dur}|${status.scanPoints.join(',')}|${status.scanIndex}` : '';
  if (marks.dataset.key !== key) {
    marks.dataset.key = key;
    marks.replaceChildren();
    if (key) {
      status.scanPoints.forEach((p, i) => {
        const cls = i === status.scanIndex - 1 ? 'now' : i < status.scanIndex - 1 ? 'done' : '';
        marks.append(el('div', {
          className: `tl-mark ${cls}`,
          style: `left:${(p / dur) * 100}%;width:${Math.max(0.3, (status.scanSegment / dur) * 100)}%`,
        }));
      });
    }
  }
}

async function poll() {
  try {
    if (track) {
      status = await invoke('status');
      renderStatus();
    }
  } catch (_) { /* nada */ }
  setTimeout(poll, 80);
}

// Forma de onda: barras de pico (tenues) y RMS (lo que se percibe) en espejo.
function drawWave() {
  const c = $('tl-wave');
  const r = c.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  c.width = Math.max(1, Math.round(r.width * dpr));
  c.height = Math.max(1, Math.round(r.height * dpr));
  const ctx = c.getContext('2d');
  ctx.clearRect(0, 0, c.width, c.height);
  if (!wave || !track || wave.path !== track.path) return;

  const n = wave.rms.length;
  const W = c.width;
  const mid = c.height / 2;
  const amp = mid - 3 * dpr;
  const bar = Math.max(1, Math.round(2 * dpr));
  const step = bar + Math.max(1, Math.round(dpr));
  for (let x = 0; x < W; x += step) {
    const i0 = Math.floor((x / W) * n);
    const i1 = Math.max(i0 + 1, Math.floor(((x + step) / W) * n));
    let p = 0;
    let q = 0;
    for (let i = i0; i < i1 && i < n; i++) {
      if (wave.peak[i] > p) p = wave.peak[i];
      if (wave.rms[i] > q) q = wave.rms[i];
    }
    const ph = (p / 255) * amp;
    const rh = (q / 255) * amp;
    ctx.fillStyle = 'rgba(139, 144, 168, 0.22)';
    ctx.fillRect(x, mid - ph, bar, ph * 2);
    ctx.fillStyle = 'rgba(205, 210, 230, 0.62)';
    ctx.fillRect(x, mid - rh, bar, Math.max(1, rh * 2));
  }
}

// ---------------------------------------------------------------- acciones

let lastUndoAvailable = false;

function setTrack(t) {
  track = t;
  status = null;
  $('tl-marks').dataset.key = '';
  if (t) lastUndoAvailable = t.undoAvailable;
  renderTrack();
  drawWave();
}

async function openPath(path) {
  if (!path) return;
  setTrack(await call('open_file', { path }));
}

async function openDialog() {
  const path = await dialog.open({
    multiple: false,
    directory: false,
    defaultPath: track ? parentDir(track.path) : undefined,
    filters: [{ name: 'Audio', extensions: ['mp3', 'flac', 'wav', 'aac', 'm4a'] }],
  });
  if (path) openPath(path);
}

async function moveTo(i, rename = false) {
  if (busy || !track?.root) return;
  const d = track.root.destinations[i];
  if (!d || d.isCurrent) return;
  busy = true;
  const btn = document.querySelector(`.dest[data-index="${i}"]`);
  btn?.classList.add('flash');
  try {
    const res = await call('move_current', { dest: i, rename });
    if (res.kind === 'conflict') {
      showConflict(i, res.existing);
      return;
    }
    lastUndoAvailable = true;
    setTrack(res.next);
    toast(res.next ? `✓ Movido a "${res.label}"` : `✓ Movido a "${res.label}". ¡Carpeta terminada!`, 'ok');
  } catch (_) {
    // el toast de error ya se mostró
  } finally {
    busy = false;
    btn?.classList.remove('flash');
  }
}

function showConflict(dest, existing) {
  conflict = { dest, existing };
  $('conflict-path').textContent = existing;
  $('conflict').hidden = false;
}
function hideConflict() {
  conflict = null;
  $('conflict').hidden = true;
}

async function next() {
  if (busy) return;
  setTrack(await call('next_track'));
}
async function prev() {
  if (busy) return;
  setTrack(await call('prev_track'));
}
async function undo() {
  if (busy) return;
  busy = true;
  try {
    const t = await call('undo_move');
    lastUndoAvailable = t?.undoAvailable ?? false;
    setTrack(t);
    toast('↶ Movimiento deshecho', 'ok');
  } catch (_) {
  } finally {
    busy = false;
  }
}

async function togglePlay() {
  if (!track) return;
  await call('toggle_play');
}

async function seekBy(delta) {
  if (!status) return;
  await call('seek', { position: Math.max(0, status.position + delta), manual: true });
}

async function toggleAuto() {
  if (!track || !status) return;
  await call('set_autoscan', { enabled: !status.scanActive });
}

async function setVolume(v) {
  v = Math.min(1, Math.max(0, v));
  $('volume').value = v;
  await call('set_volume', { volume: v });
}

// ---------------------------------------------------------------- timeline

function timelinePos(ev) {
  const r = $('timeline').querySelector('.tl-track').getBoundingClientRect();
  return Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width));
}
function previewAt(f) {
  const dur = status?.duration || track?.meta.duration || 0;
  $('tl-played').style.width = `${f * 100}%`;
  $('tl-head').style.left = `${f * 100}%`;
  $('t-pos').textContent = fmtTime(f * dur);
}

$('timeline').addEventListener('pointerdown', (ev) => {
  if (!track) return;
  dragging = true;
  $('timeline').setPointerCapture(ev.pointerId);
  previewAt(timelinePos(ev));
});
$('timeline').addEventListener('pointermove', (ev) => {
  const f = timelinePos(ev);
  $('tl-hover').style.left = `${f * 100}%`;
  if (dragging) previewAt(f);
});
$('timeline').addEventListener('pointerup', async (ev) => {
  if (!dragging) return;
  dragging = false;
  const dur = status?.duration || track?.meta.duration || 0;
  // El usuario tocó la línea de tiempo: este track pasa a manual.
  await call('seek', { position: timelinePos(ev) * dur, manual: true });
});

// ---------------------------------------------------------------- teclado

document.addEventListener('keydown', (ev) => {
  const inField = ev.target.closest('input, textarea, select');
  if (!$('settings').hidden) {
    if (ev.key === 'Escape') closeSettings();
    return;
  }
  if (inField) return;

  const ctrl = ev.ctrlKey || ev.metaKey;
  if (ctrl && ev.key.toLowerCase() === 'z') { ev.preventDefault(); undo(); return; }
  if (ctrl && ev.key.toLowerCase() === 'o') { ev.preventDefault(); openDialog(); return; }
  if (ctrl && ev.key === ',') { ev.preventDefault(); openSettings(); return; }
  if (ctrl || ev.altKey) return;

  if (conflict) {
    if (ev.key === 'Enter') { ev.preventDefault(); const d = conflict.dest; hideConflict(); moveTo(d, true); return; }
    if (ev.key === 'Escape') { ev.preventDefault(); hideConflict(); return; }
  }

  // Mover: 1-9 y 0. Nunca con la tecla sostenida (movería varios tracks).
  if (/^[0-9]$/.test(ev.key) && !ev.shiftKey) {
    ev.preventDefault();
    if (ev.repeat) return;
    const i = ev.key === '0' ? 9 : Number(ev.key) - 1;
    moveTo(i);
    return;
  }

  switch (ev.key) {
    case ' ':
      ev.preventDefault();
      togglePlay();
      break;
    case 's': case 'S': case 'n': case 'N':
      if (!ev.repeat) { hideConflict(); next(); }
      break;
    case 'b': case 'B': case 'p': case 'P':
      if (!ev.repeat) prev();
      break;
    case 'a': case 'A':
      if (!ev.repeat) toggleAuto();
      break;
    case 'ArrowRight':
      ev.preventDefault();
      seekBy(ev.shiftKey ? 30 : 5);
      break;
    case 'ArrowLeft':
      ev.preventDefault();
      seekBy(ev.shiftKey ? -30 : -5);
      break;
    case 'ArrowUp':
      ev.preventDefault();
      setVolume(Number($('volume').value) + 0.05);
      break;
    case 'ArrowDown':
      ev.preventDefault();
      setVolume(Number($('volume').value) - 0.05);
      break;
  }
});

// ---------------------------------------------------------------- configuración

async function saveConfig() {
  const t = await call('save_config', { config });
  config = await call('get_config');
  if (t) setTrack(t);
  renderSettings();
}

async function pickFolder(defaultPath) {
  const p = await dialog.open({ directory: true, multiple: false, defaultPath });
  return typeof p === 'string' ? p : null;
}

async function addRoot(suggest) {
  const path = await pickFolder(suggest);
  if (!path) return;
  if (config.roots.some((r) => r.path === path)) { toast('Esa carpeta ya está agregada', 'err'); return; }
  config.roots.push({ path, destinations: [] });
  await saveConfig();
}

function renderSettings() {
  const roots = $('roots');
  roots.replaceChildren();
  config.roots.forEach((root, ri) => {
    const rows = el('div', { className: 'dest-rows' });
    root.destinations.forEach((d, di) => {
      const label = el('input', { type: 'text', value: d.label, placeholder: 'Nombre del botón' });
      label.addEventListener('change', () => { d.label = label.value; saveConfig(); });
      const move = (delta) => {
        const j = di + delta;
        if (j < 0 || j >= root.destinations.length) return;
        [root.destinations[di], root.destinations[j]] = [root.destinations[j], root.destinations[di]];
        saveConfig();
      };
      rows.append(el('div', { className: 'dest-row' },
        el('kbd', { textContent: keyFor(di) ?? '·' }),
        label,
        el('span', { className: 'path', title: d.path, textContent: `\u200E${d.path}\u200E` }),
        el('span', { className: 'acts' },
          el('button', { title: 'Subir', textContent: '↑', onclick: () => move(-1) }),
          el('button', { title: 'Bajar', textContent: '↓', onclick: () => move(1) }),
          el('button', { title: 'Quitar botón (no borra la carpeta)', textContent: '✕', onclick: () => { root.destinations.splice(di, 1); saveConfig(); } }),
        ),
      ));
    });

    const newName = el('input', { type: 'text', placeholder: 'Nueva subcarpeta, p. ej. Las que suenan' });
    const create = async () => {
      const name = newName.value.trim();
      if (!name) { newName.focus(); return; }
      const path = await call('create_subfolder', { parent: root.path, name });
      if (root.destinations.some((d) => d.path === path)) { toast('Ese destino ya existe', 'err'); return; }
      root.destinations.push({ label: name, path });
      await saveConfig();
    };
    newName.addEventListener('keydown', (e) => { if (e.key === 'Enter') create(); });

    roots.append(el('div', { className: 'root-card' },
      el('div', { className: 'root-head' },
        el('div', { className: 'root-path', textContent: root.path }),
        el('button', { className: 'btn btn-small', textContent: 'Quitar raíz', onclick: () => { config.roots.splice(ri, 1); saveConfig(); } }),
      ),
      root.destinations.length ? rows : el('div', { className: 'empty-dests', textContent: 'Sin destinos. Crea subcarpetas o elige carpetas existentes (pueden estar en cualquier lugar).' }),
      el('div', { className: 'root-actions' },
        newName,
        el('button', { className: 'btn btn-small btn-accent', textContent: 'Crear', onclick: create }),
        el('button', {
          className: 'btn btn-small', textContent: 'Elegir carpeta existente…',
          onclick: async () => {
            const path = await pickFolder(root.path);
            if (!path) return;
            if (root.destinations.some((d) => d.path === path)) { toast('Ese destino ya existe', 'err'); return; }
            root.destinations.push({ label: path.split(/[\\/]/).pop(), path });
            saveConfig();
          },
        }),
      ),
    ));
  });

  $('scan-enabled').checked = config.scan.enabled;
  $('scan-seconds').value = config.scan.segmentSecs;
  $('scan-points').value = config.scan.points;
  $('scan-repeat').checked = config.scan.repeat;
  $('remove-empty').checked = config.removeEmptyFolders;
}

function bindSettingInputs() {
  const on = (id, fn) => $(id).addEventListener('change', () => { fn($(id)); saveConfig(); });
  on('scan-enabled', (i) => (config.scan.enabled = i.checked));
  on('scan-seconds', (i) => (config.scan.segmentSecs = Number(i.value) || 1));
  on('scan-points', (i) => (config.scan.points = Math.round(Number(i.value)) || 12));
  on('scan-repeat', (i) => (config.scan.repeat = i.checked));
  on('remove-empty', (i) => (config.removeEmptyFolders = i.checked));
}

async function openSettings() {
  config = await call('get_config');
  renderSettings();
  $('settings').hidden = false;
}
function closeSettings() {
  $('settings').hidden = true;
}

// ---------------------------------------------------------------- arranque

async function init() {
  $('btn-open').onclick = openDialog;
  $('btn-settings').onclick = openSettings;
  $('btn-settings-close').onclick = closeSettings;
  $('settings').addEventListener('pointerdown', (e) => { if (e.target === $('settings')) closeSettings(); });
  $('btn-play').onclick = togglePlay;
  $('btn-next').onclick = () => { hideConflict(); next(); };
  $('btn-prev').onclick = prev;
  $('btn-auto').onclick = toggleAuto;
  $('btn-undo').onclick = undo;
  $('btn-add-root').onclick = () => addRoot();
  $('btn-config-here').onclick = async () => {
    await openSettings();
    addRoot(track ? parentDir(parentDir(track.path)) : undefined);
  };
  $('btn-conflict-rename').onclick = () => { const d = conflict.dest; hideConflict(); moveTo(d, true); };
  $('btn-conflict-skip').onclick = () => { hideConflict(); next(); };
  $('btn-conflict-cancel').onclick = hideConflict;
  $('volume').addEventListener('input', (e) => setVolume(Number(e.target.value)));
  bindSettingInputs();

  config = await call('get_config');
  $('volume').value = config.volume;

  await listen('open-file', (e) => openPath(e.payload));
  await listen('waveform', (e) => {
    wave = e.payload;
    drawWave();
  });
  new ResizeObserver(() => drawWave()).observe($('tl-track'));

  // Arrastrar y soltar un archivo sobre la ventana.
  const win = window.__TAURI__.webviewWindow.getCurrentWebviewWindow();
  await win.onDragDropEvent((e) => {
    if (e.payload.type === 'drop' && e.payload.paths?.length) openPath(e.payload.paths[0]);
  });

  const start = await call('startup_file');
  if (start) await openPath(start);
  else renderTrack();
  poll();
}

init();
