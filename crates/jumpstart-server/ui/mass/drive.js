// Mass's browser interface keeps navigation and selection local. Every catalog
// read or mutation goes through the authenticated loopback bridge.
"use strict";

const byId = id => document.getElementById(id);
const maxUpload = 32 * 1024 * 1024;
const state = {
  token: "",
  trail: [],
  entries: [],
  primary: [],
  next: null,
  search: "",
  layout: sessionStorage.getItem("mass.layout") === "grid" ? "grid" : "list",
  sort: "name",
  ascending: true,
  selected: null,
  activeLoad: 0,
  busy: false,
  retry: null,
};

const fragment = location.hash.slice(1);
if (/^[a-f0-9]{64}$/.test(fragment)) {
  sessionStorage.setItem("mass.session", fragment);
  history.replaceState(null, "", location.pathname + location.search);
}
state.token = sessionStorage.getItem("mass.session") || "";

/** Create a scoped SVG icon from one of the app's static symbols. */
function icon(name) {
  const namespace = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(namespace, "svg");
  const use = document.createElementNS(namespace, "use");
  use.setAttribute("href", "#i-" + name);
  svg.setAttribute("aria-hidden", "true");
  svg.append(use);
  return svg;
}

/** Make a text-only element without treating a server filename as markup. */
function text(tag, value, className) {
  const node = document.createElement(tag);
  node.textContent = value;
  if (className) node.className = className;
  return node;
}

/** Send an authenticated request and surface the daemon's refusal in plain text. */
async function api(path, options = {}, binary = false) {
  if (!state.token) throw new Error("Open the private Mass URL printed by the helper.");
  const headers = new Headers(options.headers || {});
  headers.set("Authorization", "Bearer " + state.token);
  const response = await fetch(path, { ...options, headers, cache: "no-store", credentials: "omit" });
  if (!response.ok) {
    const problem = await response.json().catch(() => null);
    throw new Error(problem?.error || "Request failed (" + response.status + ").");
  }
  return binary ? response.blob() : response.json();
}

/** Show a status message and a retry action only when the original upload is available. */
function notice(message, error = false) {
  byId("notice").hidden = !message;
  byId("notice").classList.toggle("error", error);
  byId("notice-text").textContent = message;
  byId("retry").hidden = !state.retry;
}

/** Return the current folder, or null while browsing the drive root. */
function folder() {
  return state.trail.at(-1) || null;
}

/** Update the visible destination and disable writes when the context is unavailable. */
function destinationControls() {
  const select = byId("primary");
  const current = folder();
  if (current) {
    select.value = current.primary_id;
  } else {
    const saved = sessionStorage.getItem("mass.primary");
    select.value = state.primary.some(primary => primary.id === saved) ? saved : "";
  }
  select.disabled = Boolean(current) || state.primary.length === 0;
  byId("primary-note").textContent = current
    ? "New files follow this folder’s primary."
    : state.primary.length ? "Choose where new files are stored." : "Register a primary to add files.";
  const enabled = Boolean(state.token && (current?.primary_id || select.value) && !state.search && !state.busy);
  for (const id of ["new", "upload", "new-folder", "new-upload"]) byId(id).disabled = !enabled;
}

/** Read selectable primaries and preserve an explicit previous selection. */
async function loadPrimaries() {
  state.primary = await api("/api/primaries");
  const select = byId("primary");
  const placeholder = text("option", state.primary.length ? "Choose a primary" : "No registered primary");
  placeholder.value = "";
  select.replaceChildren(placeholder);
  for (const primary of state.primary) {
    const option = text("option", primary.label);
    option.value = primary.id;
    select.append(option);
  }
  destinationControls();
}

/** Resolve a folder's breadcrumb trail with a bounded walk through its parents. */
async function resolveTrail(id) {
  const trail = [];
  const seen = new Set();
  while (id) {
    if (seen.has(id) || trail.length === 64) throw new Error("This folder path is unavailable.");
    seen.add(id);
    const item = await api("/api/files/" + encodeURIComponent(id));
    if (item.file) throw new Error("The selected item is a file.");
    trail.unshift(item);
    id = item.parent;
  }
  return trail;
}

/** Navigate to a folder, clear search, and reflect its address in browser history. */
async function navigate(id, push = true) {
  const run = ++state.activeLoad;
  try {
    const trail = id ? await resolveTrail(id) : [];
    if (run !== state.activeLoad) return;
    state.trail = trail;
    state.search = "";
    byId("search").value = "";
    closeDetails();
    if (push) history.pushState(null, "", id ? "/?folder=" + encodeURIComponent(id) : "/");
    await loadEntries();
  } catch (error) {
    if (run === state.activeLoad) notice(error.message, true);
  }
}

/** Load a page and discard any response from a previous navigation. */
async function loadEntries(append = false, keepNotice = false) {
  const run = ++state.activeLoad;
  const params = new URLSearchParams();
  if (state.search) params.set("query", state.search);
  else if (folder()) params.set("parent", folder().id);
  if (append && state.next) params.set("after", state.next);
  byId("more").disabled = true;
  if (!keepNotice) notice("");
  if (!append) {
    state.entries = [];
    state.next = null;
    render(true);
  }
  try {
    const page = await api("/api/files?" + params);
    if (run !== state.activeLoad) return;
    state.entries.push(...page.items);
    state.next = page.next_after;
    setConnected(true);
    render();
  } catch (error) {
    if (run !== state.activeLoad) return;
    setConnected(false);
    render(false, true);
    notice(error.message, true);
  } finally {
    if (run === state.activeLoad) byId("more").disabled = false;
  }
}

/** Show whether the browser can currently read the protected catalog. */
function setConnected(connected) {
  byId("session-dot").classList.toggle("connected", connected);
  byId("session-label").textContent = connected ? "Local session" : "Unavailable";
}

/** Repaint the current workspace and preserve folder-first ordering. */
function render(loading = false, unavailable = false) {
  renderBreadcrumbs();
  destinationControls();
  const count = state.entries.length;
  byId("entry-count").textContent = loading ? "Loading files…" : unavailable
    ? "The catalog is unavailable" : count + (count === 1 ? " item" : " items") + (state.next ? " shown" : "");
  byId("title").textContent = state.search ? "Search results" : folder()?.name || "My Drive";
  byId("empty-title").textContent = state.search ? "No matching files" : "This folder is empty";
  byId("empty-copy").textContent = state.search ? "Try a different filename." : "Drop files here, or use New to get started.";
  byId("empty").hidden = loading || unavailable || count !== 0;
  byId("more").hidden = !state.next;
  byId("list-head").hidden = state.layout !== "list" || !count;
  byId("list-view").setAttribute("aria-pressed", state.layout === "list");
  byId("grid-view").setAttribute("aria-pressed", state.layout === "grid");
  for (const field of ["name", "date"]) {
    byId("sort-" + field).lastElementChild.textContent = state.sort === field ? (state.ascending ? "↑" : "↓") : "";
  }
  const order = state.ascending ? 1 : -1;
  const sorted = [...state.entries].sort((left, right) => {
    if (Boolean(left.file) !== Boolean(right.file)) return left.file ? 1 : -1;
    const comparison = state.sort === "date" ? Number(left.created_at_ms || 0) - Number(right.created_at_ms || 0) : 0;
    return order * (comparison || left.name.localeCompare(right.name, undefined, { numeric: true }));
  });
  const entries = byId("entries");
  entries.classList.toggle("grid", state.layout === "grid");
  entries.replaceChildren(...sorted.map(renderEntry));
}

/** Build the Drive-style breadcrumb bar with keyboard-accessible folder links. */
function renderBreadcrumbs() {
  const container = byId("breadcrumbs");
  container.replaceChildren();
  const parts = [{ id: null, name: "My Drive" }, ...state.trail];
  if (state.search) parts.push({ id: null, name: "Search" });
  parts.forEach((part, index) => {
    if (index > 0) container.append(icon("chevron"));
    if (index === parts.length - 1) {
      const current = text("span", part.name);
      current.setAttribute("aria-current", "page");
      container.append(current);
    } else {
      const button = text("button", part.name);
      button.type = "button";
      button.addEventListener("click", () => navigate(part.id));
      container.append(button);
    }
  });
}

/** Return a folder or file-type icon without hydrating any original bytes. */
function iconType(entry) {
  if (!entry.file) return "folder";
  const suffix = entry.name.split(".").at(-1).toLowerCase();
  if (["png", "jpg", "jpeg", "webp", "gif", "heic"].includes(suffix)) return "image";
  if (["mp3", "aac", "m4a", "wav", "flac"].includes(suffix)) return "music";
  return "file";
}

/** Build one entry with explicit open and detail actions. */
function renderEntry(entry) {
  const row = document.createElement("div");
  row.className = "entry-row";
  row.setAttribute("role", "listitem");
  row.classList.toggle("selected", state.selected === entry.id);
  const name = document.createElement("button");
  name.type = "button";
  name.className = "entry-name";
  name.title = entry.name;
  const type = iconType(entry);
  const glyph = document.createElement("span");
  glyph.className = "entry-icon " + type;
  glyph.append(icon(type));
  name.append(glyph, text("span", entry.name));
  name.addEventListener("click", () => entry.file ? showDetails(entry) : navigate(entry.id));
  const detail = document.createElement("button");
  detail.type = "button";
  detail.className = "icon-button";
  detail.title = "Details";
  detail.setAttribute("aria-label", "Details for " + entry.name);
  detail.append(icon("info"));
  detail.addEventListener("click", () => showDetails(entry));
  row.append(name, text("span", date(entry.created_at_ms), "entry-meta entry-date"),
    text("span", entry.file ? size(entry.file.byte_length) : "—", "entry-meta"), detail);
  return row;
}

/** Format the exact catalog byte count for display. */
function size(bytes) {
  const count = Number(bytes);
  if (count < 1024) return count + " B";
  if (count < 1024 * 1024) return (count / 1024).toFixed(1) + " KiB";
  return (count / (1024 * 1024)).toFixed(1) + " MiB";
}

/** Convert a server timestamp into the browser's display date. */
function date(milliseconds) {
  return milliseconds ? new Intl.DateTimeFormat(undefined, { dateStyle: "medium" }).format(Number(milliseconds)) : "—";
}

/** Show file identity and download without opening arbitrary content in the UI. */
function showDetails(entry) {
  state.selected = entry.id;
  byId("details").hidden = false;
  const body = byId("detail-body");
  const facts = document.createElement("dl");
  const fields = [
    ["Type", entry.file ? "File" : "Folder"],
    ["Date added", date(entry.created_at_ms)],
    ["Primary", state.primary.find(primary => primary.id === entry.primary_id)?.label || "Unavailable"],
  ];
  if (entry.file) fields.push(["Size", size(entry.file.byte_length)], ["BLAKE3", entry.file.blob_id]);
  for (const [label, value] of fields) facts.append(text("dt", label), text("dd", value));
  body.replaceChildren(text("p", entry.name, "detail-name"), facts);
  if (entry.file) {
    const download = document.createElement("button");
    download.className = "button secondary";
    download.type = "button";
    download.append(icon("down"), " Download");
    download.addEventListener("click", () => downloadFile(entry, download));
    body.append(download);
  }
  render();
}

/** Hide details and clear the selected row or tile. */
function closeDetails() {
  state.selected = null;
  byId("details").hidden = true;
}

/** Download only after the bridge has verified the complete version. */
async function downloadFile(entry, button) {
  button.disabled = true;
  try {
    const blob = await api("/api/files/" + encodeURIComponent(entry.id) + "/download", {}, true);
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = entry.name;
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 30000);
    notice("Downloaded " + entry.name);
  } catch (error) {
    notice(error.message, true);
  } finally {
    button.disabled = false;
  }
}

/** Capture the currently selected namespace and a stable local request ID. */
function destination(name) {
  const current = folder();
  const primary = current?.primary_id || byId("primary").value;
  if (!primary || state.search) throw new Error("Open a destination folder or choose a primary.");
  return { request_id: crypto.randomUUID(), primary_id: primary, parent: current?.id || null, name: name.normalize("NFC") };
}

/** Upload selected files in sequence and retain the refused request for retry. */
async function uploadFiles(files, retry = null) {
  if (!files.length || state.busy) return;
  state.busy = true;
  state.retry = null;
  destinationControls();
  try {
    const batch = retry || destination(files[0].name);
    for (const [index, file] of files.entries()) {
      if (file.size > maxUpload) throw new Error(file.name + " exceeds the 32 MiB preview limit.");
      const target = index === 0 ? batch : {
        ...batch, name: file.name.normalize("NFC"), request_id: crypto.randomUUID(),
      };
      const params = new URLSearchParams();
      for (const [key, value] of Object.entries(target)) if (value !== null) params.set(key, value);
      notice("Uploading " + file.name + "…");
      try {
        await api("/api/import?" + params, { method: "PUT", body: file });
      } catch (error) {
        state.retry = { file, target };
        throw error;
      }
    }
    notice(files.length === 1 ? "Uploaded " + files[0].name : "Uploaded " + files.length + " files");
    await loadEntries(false, true);
  } catch (error) {
    notice(error.message, true);
  } finally {
    state.busy = false;
    destinationControls();
  }
}

/** Close the New menu and return its expanded state to false. */
function closeNew() {
  byId("new-menu").hidden = true;
  byId("new").setAttribute("aria-expanded", "false");
}

/** Open the browser picker for the active Mass destination. */
function pickFiles() {
  closeNew();
  if (!byId("upload").disabled) byId("file-input").click();
}

byId("brand").addEventListener("click", event => { event.preventDefault(); navigate(null); });
byId("my-drive").addEventListener("click", () => navigate(null));
byId("refresh").addEventListener("click", async () => {
  try { await loadPrimaries(); } catch (error) { notice(error.message, true); }
  await loadEntries();
});
byId("more").addEventListener("click", () => loadEntries(true));
byId("primary").addEventListener("change", event => {
  sessionStorage.setItem("mass.primary", event.target.value);
  destinationControls();
});
byId("search-form").addEventListener("submit", event => {
  event.preventDefault();
  closeDetails();
  state.search = byId("search").value.trim();
  loadEntries();
});
byId("search").addEventListener("search", event => {
  if (event.target.value === "" && state.search) {
    state.search = "";
    loadEntries();
  }
});
for (const layout of ["list", "grid"]) {
  byId(layout + "-view").addEventListener("click", () => {
    state.layout = layout;
    sessionStorage.setItem("mass.layout", layout);
    render();
  });
}
for (const field of ["name", "date"]) {
  byId("sort-" + field).addEventListener("click", () => {
    state.ascending = state.sort === field ? !state.ascending : field === "name";
    state.sort = field;
    render();
  });
}
byId("close-details").addEventListener("click", () => { closeDetails(); render(); });
byId("new").addEventListener("click", () => {
  const menu = byId("new-menu");
  menu.hidden = !menu.hidden;
  byId("new").setAttribute("aria-expanded", !menu.hidden);
});
document.addEventListener("click", event => {
  if (!event.target.closest(".new-wrap")) closeNew();
});
byId("new-folder").addEventListener("click", () => {
  closeNew();
  byId("folder-name").value = "";
  byId("folder-dialog").showModal();
});
byId("cancel-folder").addEventListener("click", () => byId("folder-dialog").close());
byId("folder-form").addEventListener("submit", async event => {
  event.preventDefault();
  const name = byId("folder-name").value;
  byId("folder-dialog").close();
  try {
    await api("/api/folders", {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(destination(name)),
    });
    notice("Created " + name);
    await loadEntries(false, true);
  } catch (error) {
    notice(error.message, true);
  }
});
byId("new-upload").addEventListener("click", pickFiles);
byId("upload").addEventListener("click", pickFiles);
byId("file-input").addEventListener("change", event => {
  uploadFiles([...event.target.files]);
  event.target.value = "";
});
byId("retry").addEventListener("click", () => {
  if (state.retry) uploadFiles([state.retry.file], state.retry.target);
});
byId("drive").addEventListener("dragover", event => {
  if (!event.dataTransfer.types.includes("Files")) return;
  event.preventDefault();
  byId("drop-mask").hidden = byId("upload").disabled;
});
byId("drive").addEventListener("dragleave", event => {
  if (!byId("drive").contains(event.relatedTarget)) byId("drop-mask").hidden = true;
});
byId("drive").addEventListener("drop", event => {
  event.preventDefault();
  byId("drop-mask").hidden = true;
  if (!byId("upload").disabled) uploadFiles([...event.dataTransfer.files]);
});
byId("mount-guide").addEventListener("click", () => byId("mount-dialog").showModal());
byId("close-mount").addEventListener("click", () => byId("mount-dialog").close());
byId("copy-mount").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(byId("mount-command").textContent);
    byId("mount-dialog").close();
    notice("Mount command copied.");
  } catch (error) {
    byId("mount-dialog").close();
    notice("Clipboard access is unavailable.", true);
  }
});
document.addEventListener("keydown", event => {
  if (event.key === "/"
      && !event.target.closest("input, textarea, select")
      && !document.querySelector("dialog[open]")) {
    event.preventDefault();
    byId("search").focus();
  }
  if (event.key === "Escape") closeNew();
});
window.addEventListener("popstate", () => navigate(new URLSearchParams(location.search).get("folder"), false));

Promise.allSettled([
  loadPrimaries(),
  navigate(new URLSearchParams(location.search).get("folder"), false),
]);
