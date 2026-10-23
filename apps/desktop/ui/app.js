"use strict";

const tauri = window.__TAURI__;
const invoke = tauri.core.invoke;
const listen = tauri.event.listen;
const dialog = tauri.dialog;

const $ = (id) => document.getElementById(id);

const CATEGORIES = [
  ["all", "All files", "#64748b"],
  ["photo", "Photos", "#2563eb"],
  ["video", "Videos", "#9333ea"],
  ["audio", "Music & audio", "#db2777"],
  ["document", "Documents", "#ea580c"],
  ["archive", "Archives", "#0891b2"],
  ["database", "Databases", "#65a30d"],
  ["other", "Other", "#94a3b8"],
];
const CHANCE = {
  high: ["Excellent", "The file's data is intact. It should open normally."],
  medium: ["Good", "The data is very likely intact, but it could not be fully checked."],
  low: ["Poor", "Part of the file is damaged or was overwritten by newer files. It may open only partly."],
};
const CHANCE_RANK = { low: 0, medium: 1, high: 2 };
const IMAGE_EXT = ["jpg", "jpeg", "png", "gif", "bmp", "webp", "avif", "ico"];
const VIDEO_EXT = ["mp4", "m4v", "webm", "mov", "ogv", "3gp"];
const AUDIO_EXT = ["mp3", "wav", "ogg", "oga", "opus", "m4a", "flac"];
const MEDIA_TYPES = {
  mp4: "video/mp4", m4v: "video/mp4", "3gp": "video/mp4", mov: "video/quicktime", webm: "video/webm",
  ogv: "video/ogg", mp3: "audio/mpeg", wav: "audio/wav", ogg: "audio/ogg", oga: "audio/ogg",
  opus: "audio/ogg", m4a: "audio/mp4", flac: "audio/flac",
};
const MAX_PREVIEW = 64 * 1024 * 1024;
const narrow = window.matchMedia("(max-width: 760px)");
// Rows are taller on phones, for fingers.
const rowHeight = () => (narrow.matches ? 56 : 36);
const TILE = 168;

const FOUND_AS = {
  trashed: ["in trash", "Deleted, still in the trash (phones keep deleted files for 30 days)"],
  thumbnail: ["thumbnail", "A thumbnail: a small copy kept after the photo or video"],
  hidden: ["hidden", "In a hidden folder that galleries don't show"],
  embedded: ["in cache", "Found inside another file, such as a thumbnail cache"],
};

const state = {
  platform: "",
  source: null,
  items: [],
  shown: [],
  selected: new Set(),
  category: "all",
  folder: null,
  openFolders: new Set(["/"]),
  search: "",
  deletedOnly: true,
  minChance: "low",
  sort: { key: "name", dir: 1 },
  view: "list",
  current: null,
  scanning: false,
  scanKind: null,
};

// ---------- helpers ----------

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k === "text") e.textContent = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const c of children) if (c != null) e.append(c);
  return e;
}

function formatSize(n) {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n;
  let i = -1;
  do { v /= 1024; i++; } while (v >= 1024 && i < units.length - 1);
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

function formatDate(t) {
  if (t == null) return "";
  const d = new Date(t * 1000);
  // File system times are stored as local or UTC times; show them as stored.
  const p = (x) => String(x).padStart(2, "0");
  return `${d.getUTCFullYear()}-${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())} ${p(d.getUTCHours())}:${p(d.getUTCMinutes())}`;
}

function previewUrl(id) {
  return tauri.core.convertFileSrc(String(id), "preview");
}

function show(view) {
  for (const v of ["home", "source", "results"]) $(`view-${v}`).classList.toggle("hidden", v !== view);
  document.body.classList.remove("previewing");
  renderCrumbs(view);
}

function renderCrumbs(view) {
  const c = $("crumbs");
  c.replaceChildren();
  const steps = [["home", "Choose drive"]];
  if (view !== "home" && state.source) steps.push(["source", state.source.title]);
  if (view === "results") steps.push(["results", "Results"]);
  steps.forEach(([v, label], i) => {
    if (i > 0) c.append(el("span", { class: "sep", text: "›" }));
    const last = i === steps.length - 1;
    c.append(last ? el("span", { text: label }) : el("button", { text: label, onclick: () => !state.scanning && show(v) }));
  });
}

function modal({ title, body, progress = false, actions = [] }) {
  $("modal-title").textContent = title;
  const b = $("modal-body");
  b.replaceChildren();
  if (typeof body === "string") b.append(el("p", { text: body }));
  else if (body) b.append(body);
  $("modal-progress-wrap").classList.toggle("hidden", !progress);
  $("modal-progress").style.width = "0%";
  const a = $("modal-actions");
  a.replaceChildren(...actions.map(([label, fn, cls]) => el("button", { class: `btn ${cls || ""}`, text: label, onclick: fn })));
  $("modal").classList.remove("hidden");
}

function closeModal() {
  $("modal").classList.add("hidden");
}

function error(message) {
  modal({ title: "Something went wrong", body: String(message), actions: [["OK", closeModal, "primary"]] });
}

const ICONS = {
  disk: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3" y="5" width="18" height="14" rx="2"/><path d="M7 15h.01M11 15h6"/></svg>',
  usb: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="7" y="9" width="10" height="13" rx="2"/><path d="M9 9V3h6v6M10 6h.01M14 6h.01"/></svg>',
  phone: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="6" y="2" width="12" height="20" rx="2"/><path d="M11 18h2"/></svg>',
  file: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/></svg>',
};

function svgIcon(name) {
  const t = document.createElement("template");
  t.innerHTML = ICONS[name];
  return t.content.firstChild;
}

// ---------- 1. drives ----------

async function loadDrives() {
  const box = $("drives");
  box.replaceChildren(el("div", { class: "no-drives", text: "Looking for drives…" }));
  let drives = [];
  try {
    drives = await invoke("list_drives");
  } catch (e) {
    console.error(e);
  }
  box.replaceChildren();
  if (!drives.length) {
    box.append(el("div", { class: "no-drives", text: "No drives found. You can still open a disk image file." }));
    return;
  }
  for (const d of drives) {
    const folder = d.kind === "folder";
    const size = d.size ? formatSize(d.size) : folder ? null : "size unknown";
    const meta = [size, d.removable ? "removable" : null, d.path].filter(Boolean).join(" · ");
    const open = () => (folder ? openFolderSource(d.path, d.name) : openSource(d.path, d.name));
    const icon = folder && !d.removable ? "phone" : d.removable ? "usb" : "disk";
    box.append(el("button", { class: "drive", onclick: open },
      el("div", { class: "icon" }, svgIcon(icon)),
      el("div", {}, el("div", { class: "name", text: d.name }), el("div", { class: "meta", text: meta }))));
  }
}

async function openImageFile() {
  const path = await dialog.open({ title: "Open a disk image", multiple: false, directory: false });
  if (path) openSource(path, path.split(/[\\/]/).pop());
}

async function openFolderDialog() {
  const path = await dialog.open({ title: "Choose a folder to search for lost photos and videos", directory: true, multiple: false });
  if (path) openFolderSource(path, path.split(/[\\/]/).filter(Boolean).pop() || path);
}

// ---------- 2. source ----------

function setSourceMode(folder) {
  for (const id of ["btn-deep", "drive-options"]) $(id).classList.toggle("hidden", folder);
  $("partitions").classList.toggle("hidden", folder);
  $("quick-title").textContent = folder ? "Find lost photos & videos" : "Quick scan";
  $("quick-text").textContent = folder
    ? "Looks in the trash, thumbnails, hidden folders and caches for photos and videos you lost."
    : "Finds deleted files with their original names, folders and dates. Usually takes seconds.";
}

async function openFolderSource(path, title) {
  try {
    const info = await invoke("open_folder_source", { path });
    state.source = { ...info, title };
  } catch (e) {
    error(e);
    return;
  }
  setSourceMode(true);
  $("source-title").textContent = title;
  $("source-sub").textContent = path;
  const note = $("folder-note");
  const card = state.platform === "android" && path !== "/storage/emulated/0";
  note.classList.toggle("hidden", !card);
  note.textContent = "Android only lets apps read the files on a memory card, not the card itself. "
    + "To find deleted files on this card, put it in a card reader and use Full Recovery on a computer.";
  show("source");
}

async function openSource(path, title) {
  try {
    const info = await invoke("open_source", { path });
    state.source = { ...info, title };
  } catch (e) {
    error(e);
    return;
  }
  setSourceMode(false);
  $("folder-note").classList.add("hidden");
  $("source-title").textContent = title;
  $("source-sub").textContent = `${formatSize(state.source.size)} · ${path}`;
  const parts = $("partitions");
  parts.replaceChildren();
  for (const p of state.source.partitions) {
    parts.append(el("div", { class: "part" },
      el("b", { text: `Partition ${p.index}` }),
      ` · ${p.file_system || "unknown file system"} · ${formatSize(p.size)}${p.name ? ` · ${p.name}` : ""}`));
  }
  if (!state.source.partitions.length) {
    parts.append(el("div", { class: "part", text: "No partitions found. Try the deep scan, or search for lost partitions." }));
  }
  show("source");
}

// ---------- scanning ----------

function resetResults() {
  state.items = [];
  state.selected.clear();
  state.current = null;
  state.folder = null;
  state.category = "all";
  $("preview-body").replaceChildren(el("div", { class: "preview-empty", text: "Select a file to preview it." }));
}

async function startScan(kind) {
  // A deep scan adds to the quick scan's results.
  if (kind === "quick") {
    resetResults();
    state.deletedOnly = true;
    $("chk-deleted").checked = true;
  }
  try {
    if (kind === "quick") await invoke("start_quick_scan", { searchLost: $("chk-lost").checked });
    else await invoke("start_deep_scan");
  } catch (e) {
    error(e);
    return;
  }
  state.scanning = true;
  state.scanKind = kind;
  $("scanbar").classList.remove("hidden");
  $("scan-phase").textContent = kind === "quick" ? "Quick scan" : "Deep scan";
  $("scan-found").textContent = "";
  $("scan-progress").style.width = "0%";
  show("results");
  refresh();
}

// Requests run one after another, so no file is added twice.
let fetchQueue = Promise.resolve();

function fetchItems() {
  fetchQueue = fetchQueue
    .then(async () => {
      const more = await invoke("get_items", { from: state.items.length });
      const fresh = more.filter((i) => i.id >= state.items.length);
      if (fresh.length) {
        state.items.push(...fresh);
        refresh();
      }
    })
    .catch(console.error);
  return fetchQueue;
}

listen("scan-progress", ({ payload: p }) => {
  if (!state.scanning) return;
  $("scan-phase").textContent = p.phase;
  $("scan-found").textContent = `· ${p.found.toLocaleString()} files found`;
  $("scan-progress").style.width = `${p.total ? (100 * p.done) / p.total : 0}%`;
  fetchItems();
});

listen("scan-done", async ({ payload: p }) => {
  state.scanning = false;
  $("scanbar").classList.add("hidden");
  await fetchItems();
  // The deep scan may have upgraded the chance of files found earlier.
  if (state.scanKind === "deep") {
    fetchQueue = fetchQueue.then(async () => { state.items = await invoke("get_items", { from: 0 }); });
    await fetchQueue;
    const current = state.items.find((i) => i.id === state.current);
    if (current) setCurrent(current);
  }
  if (p.error) error(p.error);
  refresh();
});

// ---------- 3. results ----------

function matches(item, ignoreFolder = false) {
  // The deep scan can't tell whether a file was deleted, so it is always shown.
  if (state.deletedOnly && item.method === "files" && !item.deleted) return false;
  if (CHANCE_RANK[item.confidence] < CHANCE_RANK[state.minChance]) return false;
  if (state.search && !item.name.toLowerCase().includes(state.search)) return false;
  if (!ignoreFolder && state.folder && !(item.folder === state.folder || item.folder.startsWith(`${state.folder}/`))) return false;
  return true;
}

function refresh() {
  const base = state.items.filter((i) => matches(i, true));
  renderCategories(base);
  const inCategory = base.filter((i) => state.category === "all" || i.category === state.category);
  renderFolders(inCategory);
  state.shown = inCategory.filter((i) => matches(i));
  sortShown();
  renderList();
  renderSelection();
  const empty = $("empty");
  if (!state.shown.length) {
    empty.textContent = state.scanning ? "Searching…" : state.items.length
      ? "No files match. Try “Deleted files only” off, or another file type."
      : "No files were found. Try the deep scan.";
    empty.classList.remove("hidden");
  } else {
    empty.classList.add("hidden");
  }
  const folder = Boolean(state.source?.folder);
  $("btn-deep-again").classList.toggle("hidden", state.scanKind === "deep" || folder);
  // Folder finds are lost files of every kind, not just deleted ones.
  $("deleted-filter").classList.toggle("hidden", folder);
}

function sortShown() {
  const { key, dir } = state.sort;
  const value = (i) => (key === "confidence" ? CHANCE_RANK[i.confidence] : i[key] ?? -1);
  state.shown.sort((a, b) => {
    const x = value(a);
    const y = value(b);
    if (typeof x === "string") return dir * x.localeCompare(y, undefined, { numeric: true, sensitivity: "base" });
    return dir * (x - y);
  });
  document.querySelectorAll(".list-head [data-sort]").forEach((h) => {
    h.classList.toggle("sorted", h.dataset.sort === key);
    h.classList.toggle("desc", h.dataset.sort === key && dir < 0);
  });
}

function renderCategories(items) {
  const counts = { all: items.length };
  for (const i of items) counts[i.category] = (counts[i.category] || 0) + 1;
  const ul = $("categories");
  ul.replaceChildren();
  for (const [key, label, color] of CATEGORIES) {
    if (key !== "all" && !counts[key]) continue;
    ul.append(el("li", {
      class: state.category === key ? "on" : "",
      onclick: () => { state.category = key; state.folder = null; refresh(); },
    }, el("span", { class: "dot", style: `background:${color}` }), label,
    el("span", { class: "count", text: (counts[key] || 0).toLocaleString() })));
  }
}

function renderFolders(items) {
  // Build a tree of folders with file counts.
  const root = { name: "All folders", path: "/", count: 0, children: new Map() };
  for (const i of items) {
    root.count++;
    let node = root;
    let path = "";
    for (const part of i.folder.split("/").filter(Boolean)) {
      path += `/${part}`;
      if (!node.children.has(part)) node.children.set(part, { name: part, path, count: 0, children: new Map() });
      node = node.children.get(part);
      node.count++;
    }
  }
  const draw = (node, depth) => {
    const open = state.openFolders.has(node.path) || depth === 0;
    const isOn = (state.folder ?? "/") === node.path;
    const kids = [...node.children.values()].sort((a, b) => a.name.localeCompare(b.name));
    const row = el("div", {
      class: `node${isOn ? " on" : ""}`,
      onclick: () => {
        state.folder = node.path === "/" ? null : node.path;
        if (kids.length) {
          if (state.openFolders.has(node.path) && isOn) state.openFolders.delete(node.path);
          else state.openFolders.add(node.path);
        }
        refresh();
      },
    }, el("span", { class: "twist", text: kids.length ? (open ? "▾" : "▸") : "" }),
    el("span", { class: "label", text: node.name, title: node.path }),
    el("span", { class: "count", text: node.count.toLocaleString() }));
    const wrap = el("div", {}, row);
    if (open && kids.length) wrap.append(el("div", { class: "children" }, ...kids.map((k) => draw(k, depth + 1))));
    return wrap;
  };
  $("folders").replaceChildren(draw(root, 0));
}

// The list only creates elements for the rows on screen, so even hundreds
// of thousands of files scroll smoothly.
function renderList() {
  const list = $("list");
  const spacer = $("list-spacer");
  const grid = state.view === "grid";
  $("list-head").classList.toggle("hidden", grid);
  const width = list.clientWidth || 600;
  const cols = grid ? Math.max(1, Math.floor((width - 12) / TILE)) : 1;
  const height = grid ? TILE : rowHeight();
  const rows = Math.ceil(state.shown.length / cols);
  spacer.style.height = `${rows * height + (grid ? 12 : 0)}px`;
  const first = Math.max(0, Math.floor(list.scrollTop / height) - 3);
  const last = Math.min(rows, Math.ceil((list.scrollTop + list.clientHeight) / height) + 3);
  for (const old of list.querySelectorAll(".row-item, .tile")) old.remove();
  const frag = document.createDocumentFragment();
  for (let r = first; r < last; r++) {
    for (let c = 0; c < cols; c++) {
      const item = state.shown[r * cols + c];
      if (!item) break;
      frag.append(grid ? tile(item, r, c) : row(item, r));
    }
  }
  list.append(frag);
}

function checkbox(item) {
  const box = el("input", { type: "checkbox", title: "Select" });
  box.checked = state.selected.has(item.id);
  box.addEventListener("click", (e) => {
    e.stopPropagation();
    if (box.checked) state.selected.add(item.id);
    else state.selected.delete(item.id);
    renderSelection();
  });
  return box;
}

function row(item, r) {
  const [label, why] = CHANCE[item.confidence];
  const e = el("div", {
    class: `row-item${state.current === item.id ? " current" : ""}`,
    style: `top:${r * rowHeight()}px`,
    onclick: () => setCurrent(item),
    ondblclick: () => toggleSelect(item),
  },
  el("div", { class: "check-cell" }, checkbox(item)),
  el("div", { class: "name", title: item.name }, tag(item), el("span", { text: item.name })),
  el("div", { class: "folder", text: item.folder, title: item.folder }),
  el("div", { class: "size", text: formatSize(item.size) }),
  el("div", { class: "date", text: formatDate(item.modified) }),
  el("div", {}, el("span", { class: `chance ${item.confidence}`, text: label, title: why })));
  return e;
}

function tag(item) {
  if (item.found_as) {
    const [label, why] = FOUND_AS[item.found_as];
    return el("span", { class: `del ${item.found_as}`, text: label, title: why });
  }
  return item.deleted ? el("span", { class: "del", text: "deleted" }) : null;
}

function tile(item, r, c) {
  const isImage = IMAGE_EXT.includes(item.ext) && item.size <= 16 * 1024 * 1024 && !item.problem;
  const thumb = el("div", { class: "thumb" });
  if (isImage) {
    const img = el("img", { loading: "lazy", alt: "", src: previewUrl(item.id) });
    img.addEventListener("error", () => img.replaceWith(item.ext || "?"));
    thumb.append(img);
  } else {
    thumb.textContent = item.ext || "?";
  }
  return el("div", {
    class: `tile${state.current === item.id ? " current" : ""}`,
    style: `top:${r * TILE + 8}px;left:${c * TILE + 8}px;width:${TILE - 10}px;height:${TILE - 10}px`,
    onclick: () => setCurrent(item),
    ondblclick: () => toggleSelect(item),
  }, checkbox(item), thumb,
  el("div", { class: "cap" }, el("span", { class: `chance-dot ${item.confidence}`, title: CHANCE[item.confidence][0] }), el("span", { text: item.name, title: item.name })));
}

function toggleSelect(item) {
  if (state.selected.has(item.id)) state.selected.delete(item.id);
  else state.selected.add(item.id);
  renderList();
  renderSelection();
}

function renderSelection() {
  const chosen = state.items.filter((i) => state.selected.has(i.id));
  const bytes = chosen.reduce((s, i) => s + i.size, 0);
  $("selection-info").textContent = chosen.length
    ? `${chosen.length.toLocaleString()} selected · ${formatSize(bytes)}`
    : `${state.shown.length.toLocaleString()} files shown — select files to recover, or recover all shown`;
  $("btn-recover").textContent = chosen.length ? `Recover ${chosen.length.toLocaleString()} files` : "Recover all shown";
  $("btn-recover").disabled = !chosen.length && !state.shown.length;
  $("chk-all").checked = state.shown.length > 0 && state.shown.every((i) => state.selected.has(i.id));
}

// ---------- preview ----------

function status(item) {
  if (item.found_as === "trashed") return "Deleted, still in the trash";
  if (item.found_as) return "A copy kept by the system; the original may be deleted";
  if (item.method === "deep") return "Unknown (found by its content)";
  return item.deleted ? "Deleted" : "Not deleted";
}

function foundBy(item) {
  if (item.found_as) return FOUND_AS[item.found_as][1];
  return item.method === "files" ? "Quick scan (original name)" : "Deep scan (content)";
}

async function setCurrent(item) {
  state.current = item.id;
  renderList();
  document.body.classList.add("previewing");
  const pane = $("preview-body");
  const [label, why] = CHANCE[item.confidence];
  const media = el("div", { class: "media" });
  const facts = el("dl", {},
    el("dt", { text: "Folder" }), el("dd", { text: item.folder }),
    el("dt", { text: "Size" }), el("dd", { text: `${formatSize(item.size)} (${item.size.toLocaleString()} bytes)` }),
    item.modified != null ? el("dt", { text: "Modified" }) : null,
    item.modified != null ? el("dd", { text: formatDate(item.modified) }) : null,
    el("dt", { text: "Status" }),
    el("dd", { text: status(item) }),
    el("dt", { text: "Found by" }), el("dd", { text: foundBy(item) }),
    el("dt", { text: "Chance" }), el("dd", {}, el("span", { class: `chance ${item.confidence}`, text: label })));
  pane.replaceChildren(...[media, el("h3", { text: item.name }), facts,
    el("p", { class: "explain", text: item.problem ? `Can't be recovered yet: ${item.problem}.` : why }),
    item.verified ? el("p", { class: "explain verified", text: "✓ The deep scan checked this file's content: it is intact." }) : null,
    el("button", { class: "btn primary", text: "Recover this file", onclick: () => recoverItems([item.id]) })]
    .filter(Boolean));

  if (item.problem) {
    media.textContent = "No preview";
  } else if (item.size > MAX_PREVIEW) {
    media.textContent = "Too large to preview";
  } else if (IMAGE_EXT.includes(item.ext)) {
    const img = el("img", { alt: item.name, src: previewUrl(item.id) });
    img.addEventListener("error", () => textPreview(item, media));
    media.append(img);
  } else if (VIDEO_EXT.includes(item.ext) || AUDIO_EXT.includes(item.ext)) {
    playMedia(item, media);
  } else {
    textPreview(item, media);
  }
}

let mediaUrl = null;

// Videos and audio are loaded into memory and played from there.
async function playMedia(item, media) {
  media.replaceChildren(el("span", { class: "muted", text: "Loading…" }));
  let bytes;
  try {
    bytes = await invoke("preview_bytes", { id: item.id });
  } catch (e) {
    media.replaceChildren(el("span", { class: "muted", text: `No preview: ${e}` }));
    return;
  }
  if (state.current !== item.id) return;
  if (mediaUrl) URL.revokeObjectURL(mediaUrl);
  mediaUrl = URL.createObjectURL(new Blob([bytes], { type: MEDIA_TYPES[item.ext] }));
  const video = VIDEO_EXT.includes(item.ext);
  const player = el(video ? "video" : "audio", { controls: "", preload: "metadata", src: mediaUrl });
  // Formats the system can't play are shown as data instead.
  player.addEventListener("error", () => textPreview(item, media));
  media.replaceChildren(player);
}

async function textPreview(item, media) {
  media.replaceChildren(el("span", { class: "muted", text: "Loading…" }));
  try {
    const p = await invoke("text_preview", { id: item.id });
    if (state.current !== item.id) return;
    const text = p.content + (p.truncated ? "\n…" : "");
    media.replaceChildren(el("pre", { class: p.kind, text: text || "(empty file)" }));
  } catch (e) {
    media.replaceChildren(el("span", { class: "muted", text: `No preview: ${e}` }));
  }
}

// ---------- recovering ----------

async function chooseOutput() {
  if (state.platform === "android") {
    // No folder picker that gives a real path: save to Download.
    return invoke("default_output_dir");
  }
  return dialog.open({
    title: "Choose where to save the recovered files (use a different drive)",
    directory: true,
    multiple: false,
  });
}

async function recoverItems(ids) {
  if (!ids.length) return;
  const out = await chooseOutput();
  if (!out) return;
  try {
    await invoke("recover", { ids, out });
  } catch (e) {
    error(e);
    return;
  }
  modal({
    title: `Recovering ${ids.length.toLocaleString()} files…`,
    body: `Saving to ${out}`,
    progress: true,
    actions: [["Stop", () => invoke("cancel")]],
  });
}

listen("recover-progress", ({ payload: p }) => {
  $("modal-progress").style.width = `${(100 * p.done) / Math.max(1, p.total)}%`;
});

listen("recover-done", ({ payload: p }) => {
  const body = el("div", {},
    el("p", { text: `${p.saved.toLocaleString()} files were saved to ${p.folder}.${p.cancelled ? " Recovery was stopped." : ""}` }));
  if (p.failed) {
    body.append(el("p", { text: `${p.failed.toLocaleString()} files could not be recovered:` }),
      el("ul", {}, ...p.errors.map((e) => el("li", { text: e }))));
  }
  modal({
    title: p.failed ? "Recovery finished with problems" : "Your files are back",
    body,
    actions: state.platform === "android"
      ? [["OK", closeModal, "primary"]]
      : [["Close", closeModal], ["Open folder", () => { invoke("open_folder", { path: p.folder }); closeModal(); }, "primary"]],
  });
});

// ---------- disk image ----------

async function makeImage() {
  const dest = await dialog.save({ title: "Save the disk image (on a different drive)", defaultPath: "drive-copy.img" });
  if (!dest) return;
  try {
    await invoke("create_image", { dest });
  } catch (e) {
    error(e);
    return;
  }
  modal({
    title: "Copying the drive…",
    body: "Unreadable parts are skipped and filled with zeros. This can take a long time for a failing drive.",
    progress: true,
    actions: [["Stop", () => invoke("cancel")]],
  });
}

listen("image-progress", ({ payload: p }) => {
  $("modal-progress").style.width = `${(100 * p.done) / Math.max(1, p.total)}%`;
});

listen("image-done", ({ payload: p }) => {
  if (p.error) {
    error(p.error);
    return;
  }
  const bad = p.bad_bytes ? ` ${formatSize(p.bad_bytes)} could not be read and were filled with zeros.` : "";
  modal({
    title: p.cancelled ? "Copy stopped" : "Copy finished",
    body: `${formatSize(p.bytes)} copied to ${p.path}.${bad} You can now recover from the copy.`,
    actions: [["Close", closeModal], ["Recover from the copy", () => { closeModal(); openSource(p.path, p.path.split(/[\\/]/).pop()); }, "primary"]],
  });
});

// ---------- wiring ----------

$("btn-refresh").addEventListener("click", loadDrives);
$("btn-open-image").addEventListener("click", openImageFile);
$("btn-open-folder").addEventListener("click", openFolderDialog);
$("btn-close-preview").addEventListener("click", () => document.body.classList.remove("previewing"));
$("btn-quick").addEventListener("click", () => startScan("quick"));
$("btn-deep").addEventListener("click", () => startScan("deep"));
$("btn-deep-again").addEventListener("click", () => startScan("deep"));
$("btn-image").addEventListener("click", makeImage);
$("btn-cancel").addEventListener("click", () => invoke("cancel"));
$("btn-new").addEventListener("click", () => { if (!state.scanning) { show("home"); loadDrives(); } });
$("btn-recover").addEventListener("click", () => {
  const ids = state.selected.size ? [...state.selected] : state.shown.map((i) => i.id);
  recoverItems(ids);
});
$("search").addEventListener("input", (e) => { state.search = e.target.value.trim().toLowerCase(); refresh(); });
$("chk-deleted").addEventListener("change", (e) => { state.deletedOnly = e.target.checked; refresh(); });
$("sel-chance").addEventListener("change", (e) => { state.minChance = e.target.value; refresh(); });
$("chk-all").addEventListener("change", (e) => {
  for (const i of state.shown) {
    if (e.target.checked) state.selected.add(i.id);
    else state.selected.delete(i.id);
  }
  renderList();
  renderSelection();
});
for (const [id, view] of [["btn-list", "list"], ["btn-grid", "grid"]]) {
  $(id).addEventListener("click", () => {
    state.view = view;
    $("btn-list").classList.toggle("on", view === "list");
    $("btn-grid").classList.toggle("on", view === "grid");
    $("list").scrollTop = 0;
    renderList();
  });
}
document.querySelectorAll(".list-head [data-sort]").forEach((h) => h.addEventListener("click", () => {
  const key = h.dataset.sort;
  state.sort = { key, dir: state.sort.key === key ? -state.sort.dir : 1 };
  sortShown();
  renderList();
}));
$("list").addEventListener("scroll", () => requestAnimationFrame(renderList));
window.addEventListener("resize", () => renderList());
$("list").addEventListener("keydown", (e) => {
  const at = state.shown.findIndex((i) => i.id === state.current);
  const step = state.view === "grid" ? Math.max(1, Math.floor(($("list").clientWidth - 12) / TILE)) : 1;
  let next = null;
  if (e.key === "ArrowDown") next = at + step;
  else if (e.key === "ArrowUp") next = at - step;
  else if (e.key === "ArrowRight" && state.view === "grid") next = at + 1;
  else if (e.key === "ArrowLeft" && state.view === "grid") next = at - 1;
  else if (e.key === " " && at >= 0) { e.preventDefault(); toggleSelect(state.shown[at]); return; }
  if (next == null) return;
  e.preventDefault();
  const item = state.shown[Math.max(0, Math.min(state.shown.length - 1, next))];
  if (!item) return;
  const list = $("list");
  const height = state.view === "grid" ? TILE : rowHeight();
  const top = Math.floor(state.shown.indexOf(item) / step) * height;
  if (top < list.scrollTop) list.scrollTop = top;
  else if (top + height > list.scrollTop + list.clientHeight) list.scrollTop = top + height - list.clientHeight;
  setCurrent(item);
});

// Android: the app needs "all files access" to look through the phone.
// MainActivity.kt provides window.FullRecoveryAndroid.
function checkAndroidAccess() {
  const android = window.FullRecoveryAndroid;
  if (!android) return;
  const allowed = android.hasAllFilesAccess();
  $("android-access").classList.toggle("hidden", allowed);
  if (allowed) loadDrives();
}

$("btn-allow").addEventListener("click", () => window.FullRecoveryAndroid?.requestAllFilesAccess());
document.addEventListener("visibilitychange", () => { if (!document.hidden) checkAndroidAccess(); });

async function start() {
  try {
    state.platform = await invoke("platform");
  } catch (e) {
    console.error(e);
  }
  document.body.classList.add(`platform-${state.platform}`);
  show("home");
  loadDrives();
  checkAndroidAccess();
}

start();
