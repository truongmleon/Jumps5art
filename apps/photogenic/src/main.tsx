// Presents the same permission-aware timeline and local queue in both Tauri apps.

import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import "./style.css";

type Access = "request" | "limited" | "full" | "denied" | "restricted";
type Kind = "photo" | "livePhoto" | "video";
type Filter = "All" | "Photos" | "Videos";
type Tile = { id: string; kind: Kind; date?: string };
type Page = {
  access: Access;
  assets: Tile[];
  total: number;
  queueCount: number;
  queueError?: string;
};
type Details = {
  kind: Kind;
  staged: boolean;
  sourceChanged: boolean;
  resources: { role: string; typeIdentifier: string; originalFilename: string }[];
};
type QueueRow = { id: string; kind: Kind; count: number; date?: string };
type Result = { status: "staged" | "waiting" | "attention"; message?: string };

const filters: Filter[] = ["All", "Photos", "Videos"];
const labels: Record<Kind, string> = { photo: "Photo", livePhoto: "Live Photo", video: "Video" };
const roles: Record<string, string> = {
  originalStill: "Original still", originalVideo: "Original video", pairedMotion: "Original paired motion",
  alternateStill: "Alternate still", audio: "Audio", editedRendition: "Edited rendition", editedMotion: "Edited motion",
  adjustment: "Adjustment sidecar", adjustmentBase: "Adjustment base", proxy: "Photo proxy",
};
const monthFormat = new Intl.DateTimeFormat(undefined, { month: "long", year: "numeric", timeZone: "UTC" });
const dateFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeZone: "UTC" });

/** Invoke one native command with only the fields appropriate to that operation. */
function photos<T>(request:
  | { action: "page"; filter: Filter; offset: number }
  | { action: "thumbnail" | "details" | "prepare" | "remove"; id: string }
  | { action: "permission" | "settings" | "queue" },
): Promise<T> {
  return invoke<T>("photos", { request });
}

/** Group the PhotoKit order into months without inferring a capture time zone. */
function groupMonths(tiles: Tile[]) {
  const groups: { key: string; label: string; tiles: Tile[] }[] = [];
  for (const tile of tiles) {
    const key = tile.date?.slice(0, 7) ?? "undated";
    const last = groups.at(-1);
    if (last?.key === key) last.tiles.push(tile);
    else groups.push({ key, label: tile.date ? monthFormat.format(new Date(tile.date)) : "Unknown date", tiles: [tile] });
  }
  return groups;
}

/** Request a local PhotoKit thumbnail only when its tile approaches the viewport. */
function Thumbnail({ id }: { id: string }) {
  const target = useRef<HTMLSpanElement>(null);
  const [image, setImage] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    const element = target.current;
    if (!element) return;
    const observer = new IntersectionObserver((entries) => {
      if (!entries.some((entry) => entry.isIntersecting)) return;
      observer.disconnect();
      void photos<string | null>({ action: "thumbnail", id }).then((value) => {
        if (live && value) setImage(value);
      }, () => {});
    }, { rootMargin: "120px" });
    observer.observe(element);
    return () => { live = false; observer.disconnect(); };
  }, [id]);

  return <span ref={target} className="thumbnail" aria-hidden="true">
    {image
      ? <img src={`data:image/jpeg;base64,${image}`} alt="" />
      : <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
          <rect x="3.5" y="4.5" width="17" height="15" rx="2" />
          <circle cx="9" cy="9.5" r="1.5" />
          <path d="m4 16 4-3 4 3 4-5 4 5" />
        </svg>}
  </span>;
}

/** Coordinate paginated library reads and display the current permission scope. */
function App() {
  const [filter, setFilter] = useState<Filter>("All");
  const [page, setPage] = useState<Page>();
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [selection, setSelection] = useState<Tile>();
  const [queue, setQueue] = useState(false);
  const generation = useRef(0);
  const offset = useRef(0);

  /** Ignore stale responses when the filter or authorization changes. */
  const refresh = useCallback(async (append = false) => {
    const current = ++generation.current;
    const start = append ? offset.current : 0;
    if (!append) offset.current = 0;
    setLoading(true);
    setError("");
    try {
      const next = await photos<Page>({ action: "page", filter, offset: start });
      if (generation.current !== current) return;
      offset.current = start + next.assets.length;
      setPage((previous) => {
        if (!append || !previous || previous.access !== next.access) return next;
        const seen = new Set(previous.assets.map(({ id }) => id));
        return { ...next, assets: [...previous.assets, ...next.assets.filter(({ id }) => !seen.has(id))] };
      });
      if (next.access !== "full" && next.access !== "limited") setSelection(undefined);
    } catch (cause) {
      if (generation.current === current) setError(String(cause));
    } finally {
      if (generation.current === current) setLoading(false);
    }
  }, [filter]);

  useEffect(() => {
    void refresh();
    const onFocus = () => { void refresh(); };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refresh]);

  /** Request authorization or open Settings after the user presses the button. */
  async function changeAccess(action: "permission" | "settings") {
    setLoading(true);
    try {
      await photos({ action });
      await refresh();
    } catch (cause) {
      setError(String(cause));
      setLoading(false);
    }
  }

  const visible = page?.access === "limited" || page?.access === "full";
  const scope: Record<Access, [string, string]> = {
    request: ["Connect Apple Photos", "Choose what Photogenic can see. Hidden and protected media are excluded."],
    limited: ["Limited Photos access", "Only the items you selected appear here. The rest of the camera roll is outside this preview."],
    full: ["Photos access granted", "Hidden and protected media are excluded."],
    denied: ["Photos access is off", "Allow Photogenic in system settings to browse your library."],
    restricted: ["Photos access is restricted", "Device policy prevents Photogenic from opening this library."],
  };

  return <main>
    <header>
      <div className="identity"><span className="brand-mark" aria-hidden="true">P</span><span>JUMPSTART / PHOTOS</span></div>
      <h1>Photogenic</h1>
      <p>Browse your Apple Photos library and prepare originals on this device.</p>
    </header>

    <section className="access" aria-live="polite">
      <div>
        <h2>{page ? scope[page.access][0] : "Checking Photos access"}</h2>
        {page && <p>{scope[page.access][1]}</p>}
        <small>Server backup is unavailable in this preview.</small>
      </div>
      <div className="actions">
        {page?.access === "request" && <button disabled={loading} onClick={() => void changeAccess("permission")}>Allow Photos Access</button>}
        {(page?.access === "limited" || page?.access === "denied") &&
          <button disabled={loading} onClick={() => void changeAccess("settings")}>Open Photos Settings</button>}
        {!!page?.queueCount && <button className="secondary" onClick={() => setQueue(true)}>Local Queue ({page.queueCount})</button>}
      </div>
    </section>

    {(error || page?.queueError) && <p role="alert" className="error">{error || page?.queueError}</p>}
    {visible && page && <>
      <nav className="filters" aria-label="Media filters">
        <div>{filters.map((option) => <button key={option} className="secondary" aria-pressed={filter === option}
          onClick={() => { if (option !== filter) { setPage({ ...page, assets: [] }); setFilter(option); } }}>{option}</button>)}</div>
        <button className="secondary" disabled={loading} onClick={() => void refresh()}>Refresh</button>
      </nav>
      <p className="caption">{page.total} visible in this view · Dates shown in UTC</p>
      {!page.assets.length && <p className="empty">{loading ? "Loading photos…" : "No photos or videos are available in this view."}</p>}
      {groupMonths(page.assets).map((month) => <section className="month" key={month.key}>
        <h2>{month.label}</h2>
        <div className="grid">
          {month.tiles.map((tile) => <button key={tile.id} className="tile"
            aria-label={`Open ${labels[tile.kind]}${tile.date ? ` from ${dateFormat.format(new Date(tile.date))}` : ""}`}
            onClick={() => setSelection(tile)}>
            <Thumbnail id={tile.id} />
            {tile.kind !== "photo" && <span className="badge">{labels[tile.kind]}</span>}
          </button>)}
        </div>
      </section>)}
      {offset.current < page.total &&
        <button className="secondary more" disabled={loading} onClick={() => void refresh(true)}>{loading ? "Loading…" : "Load More"}</button>}
    </>}

    {selection && <AssetPanel key={selection.id} tile={selection}
      close={() => setSelection(undefined)} changed={() => void refresh()} />}
    {queue && <QueuePanel close={() => setQueue(false)} changed={() => void refresh()} />}
  </main>;
}

/** Open a modal with native keyboard dismissal and restore focus on close. */
function Panel({ title, close, children }: { title: string; close: () => void; children: React.ReactNode }) {
  const element = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = element.current;
    dialog?.showModal();
    return () => dialog?.close();
  }, []);
  return <dialog ref={element} aria-label={title} onCancel={(event) => { event.preventDefault(); close(); }}>
    <div className="panel-header"><h2>{title}</h2><button className="secondary" onClick={close}>Done</button></div>
    {children}
  </dialog>;
}

/** Show exact PhotoKit resources and the result of staging the selected asset. */
function AssetPanel({ tile, close, changed }: { tile: Tile; close: () => void; changed: () => void }) {
  const [details, setDetails] = useState<Details>();
  const [result, setResult] = useState<Result>();
  const [working, setWorking] = useState(false);
  const [error, setError] = useState("");
  const id = tile.id;

  useEffect(() => {
    let live = true;
    void photos<Details>({ action: "details", id }).then((value) => {
      if (live) setDetails(value);
    }, (cause) => { if (live) setError(String(cause)); });
    return () => { live = false; };
  }, [id]);

  /** Stage every component before showing the package in the local queue. */
  async function prepare() {
    setWorking(true);
    try {
      const next = await photos<Result>({ action: "prepare", id });
      setResult(next);
      if (next.status === "staged") changed();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setWorking(false);
    }
  }

  return <Panel title={`${labels[tile.kind]} originals`} close={close}>
    <div className="detail-preview"><Thumbnail id={id} /></div>
    <p className="caption">Capture time zone unknown</p>
    {details && <>
      <ul className="resources">{details.resources.map((resource, index) => <li key={index}>
        <strong>{resource.originalFilename}</strong>
        <span>{roles[resource.role] ?? "Other PhotoKit resource"} · {resource.typeIdentifier}</span>
      </li>)}</ul>
      {details.staged || result?.status === "staged"
        ? <p role="status"><strong>Staged on this device.</strong> {details.sourceChanged &&
            "This copy is from an earlier Photos snapshot. Remove it from Local Queue to stage again."}</p>
        : <><button disabled={working} onClick={() => void prepare()}>{working ? "Reading originals…" : "Prepare Originals"}</button>
          <p className="caption">Uses originals already available on this device.</p></>}
    </>}
    {result?.message && <div role="status"><strong>{result.status === "waiting" ? "Waiting for Source" : "Needs Attention"}</strong><p>{result.message}</p></div>}
    {error && <p role="alert" className="error">{error}</p>}
  </Panel>;
}

/** Manage private staged packages even after access to their source has changed. */
function QueuePanel({ close, changed }: { close: () => void; changed: () => void }) {
  const [rows, setRows] = useState<QueueRow[]>([]);
  const [pending, setPending] = useState<string>();
  const [error, setError] = useState("");
  useEffect(() => {
    let live = true;
    void photos<QueueRow[]>({ action: "queue" }).then((value) => { if (live) setRows(value); },
      (cause) => { if (live) setError(String(cause)); });
    return () => { live = false; };
  }, []);

  /** Remove the confirmed local package while leaving its Apple Photos source alone. */
  async function remove(id: string) {
    try {
      await photos({ action: "remove", id });
      setRows((previous) => previous.filter((row) => row.id !== id));
      changed();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setPending(undefined);
    }
  }

  return <Panel title="Local Queue" close={close}>
    <p>These copies are staged on this device. Removing a package leaves Apple Photos untouched.</p>
    <ul className="resources">{rows.map((row) => <li key={row.id}>
      <strong>{labels[row.kind]}{row.date && ` · ${dateFormat.format(new Date(row.date))}`}</strong>
      <span>{row.count} local resources</span>
      {pending === row.id
        ? <div className="actions"><button className="danger" onClick={() => void remove(row.id)}>Confirm Removal</button>
            <button className="secondary" onClick={() => setPending(undefined)}>Cancel</button></div>
        : <button className="secondary" onClick={() => setPending(row.id)}>Remove</button>}
    </li>)}</ul>
    {error && <p role="alert" className="error">{error}</p>}
  </Panel>;
}

const root = document.getElementById("root");
if (root) createRoot(root).render(<App />);
