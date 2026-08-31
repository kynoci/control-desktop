<script setup lang="ts">
import { ref, computed, onMounted, nextTick, watch } from "vue";
import { invoke, Channel } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import Database from "@tauri-apps/plugin-sql";
import { SCANCODES } from "./scancodes";
import { keysymFor } from "./keysyms";

// Default listening port per protocol (RDP 3389, VNC display :0 = 5900).
const DEFAULT_PORTS: Record<string, number> = { rdp: 3389, vnc: 5900 };

// Custom title-bar window controls (the OS frame is disabled).
const appWindow = getCurrentWindow();
const isMaximized = ref(false);
const isFullscreen = ref(false);
async function refreshMaximized() {
  isMaximized.value = await appWindow.isMaximized();
}
// Drive the OS window in/out of true fullscreen. Hiding the title bar (via the
// `fullscreen` class on .root) lets the active session fill the whole screen;
// the resize event then reflows the canvas and resizes the remote desktop.
async function toggleFullscreen() {
  const next = !(await appWindow.isFullscreen());
  await appWindow.setFullscreen(next);
  isFullscreen.value = next;
}
function winMinimize() {
  appWindow.minimize();
}
async function winToggleMaximize() {
  await appWindow.toggleMaximize();
  refreshMaximized();
}
function winClose() {
  appWindow.close();
}

type Connection = {
  id: number;
  name: string;
  host: string;
  port: number;
  username: string;
  protocol: string;
  /// 1 = skip the Graphics Pipeline and use the legacy RDP bitmap path (SQLite has no bool).
  legacy_graphics: number;
  folder_id: number | null;
  sort_order: number;
  created_at: number;
};

type Folder = {
  id: number;
  name: string;
  parent_id: number | null;
  sort_order: number;
  created_at: number;
};

// One entry of a folder's contents. Folders and connections share a single
// `sort_order` sequence per parent, so drag-sorting can freely interleave them.
type Item =
  | { kind: "folder"; id: number; name: string; sort: number; folder: Folder }
  | { kind: "conn"; id: number; name: string; sort: number; conn: Connection };

type Draft = {
  id: number | null;
  name: string;
  host: string;
  port: number;
  username: string;
  protocol: string;
  password: string;
  folder_id: number | null;
  legacy_graphics: boolean;
};

const blankDraft = (folderId: number | null = null): Draft => ({
  id: null,
  name: "",
  host: "",
  port: 3389,
  username: "",
  protocol: "rdp",
  password: "",
  folder_id: folderId,
  legacy_graphics: false,
});

const db = ref<Database | null>(null);
const connections = ref<Connection[]>([]);
const folders = ref<Folder[]>([]);
// Folder whose contents the card grid shows; null = the root ("Home").
const currentFolder = ref<number | null>(null);
// Ids of the folders expanded in the sidebar tree.
const expandedFolders = ref<number[]>([]);
// Latest remote-desktop thumbnail per connection id (data URL), cached locally.
const thumbnails = ref<Record<number, string>>({});
const draft = ref<Draft>(blankDraft());
const editing = ref(false);
const status = ref<string>("");
const sidebarCollapsed = ref(false);

// When the protocol changes in the form, snap the port to that protocol's
// default — but only if the user hasn't typed a non-default port themselves.
watch(
  () => draft.value.protocol,
  (proto, prev) => {
    const def = DEFAULT_PORTS[proto];
    if (def && draft.value.port === DEFAULT_PORTS[prev]) {
      draft.value.port = def;
    }
  },
);

async function loadAll() {
  if (!db.value) return;
  folders.value = await db.value.select<Folder[]>(
    "SELECT id, name, parent_id, sort_order, created_at FROM folders ORDER BY sort_order, name",
  );
  connections.value = await db.value.select<Connection[]>(
    "SELECT id, name, host, port, username, protocol, folder_id, sort_order, created_at, legacy_graphics FROM connections ORDER BY sort_order, name",
  );
  // The open folder may be gone (deleted); fall back to the root.
  if (currentFolder.value != null && !folders.value.some((f) => f.id === currentFolder.value)) {
    currentFolder.value = null;
  }
  loadThumbnails();
}

// --- Folder tree ------------------------------------------------------------

// The ordered contents of one folder (null = root): its sub-folders and its
// connections, merged into the single `sort_order` sequence they share.
function itemsIn(folderId: number | null): Item[] {
  const items: Item[] = [];
  for (const f of folders.value) {
    if ((f.parent_id ?? null) === folderId)
      items.push({ kind: "folder", id: f.id, name: f.name, sort: f.sort_order, folder: f });
  }
  for (const c of connections.value) {
    if ((c.folder_id ?? null) === folderId)
      items.push({ kind: "conn", id: c.id, name: c.name, sort: c.sort_order, conn: c });
  }
  return items.sort((a, b) => a.sort - b.sort || a.name.localeCompare(b.name));
}

const currentItems = computed(() => itemsIn(currentFolder.value));

// Folder ids and connection ids come from different tables, so neither alone is
// a unique key across a mixed list.
function itemKey(i: { kind: string; id: number }) {
  return `${i.kind}:${i.id}`;
}

function folderCount(id: number) {
  return itemsIn(id).length;
}

// True when `id` is `ancestorId` itself or sits anywhere below it. Used to keep
// a folder from being dropped inside its own subtree (which would orphan it).
function isDescendantOf(id: number | null, ancestorId: number): boolean {
  let cur = id;
  const seen = new Set<number>();
  while (cur != null) {
    if (cur === ancestorId) return true;
    if (seen.has(cur)) return false; // cycle guard
    seen.add(cur);
    cur = folders.value.find((f) => f.id === cur)?.parent_id ?? null;
  }
  return false;
}

// Root → current folder, for the breadcrumb bar.
const breadcrumb = computed(() => {
  const path: Folder[] = [];
  let id = currentFolder.value;
  const seen = new Set<number>();
  while (id != null && !seen.has(id)) {
    seen.add(id);
    const f = folders.value.find((x) => x.id === id);
    if (!f) break;
    path.unshift(f);
    id = f.parent_id ?? null;
  }
  return path;
});

// The sidebar tree, flattened to rows so it renders without a recursive
// component; collapsed folders simply don't contribute their children. An
// expanded but empty folder still emits a placeholder row, so the row that
// follows it can't be mistaken for its child.
type TreeRow =
  | {
      kind: "item";
      key: string;
      depth: number;
      // Folder the row lives in — a drop between two rows reorders within it.
      parent: number | null;
      item: Item;
      expanded: boolean;
      count: number;
    }
  | { kind: "empty"; key: string; depth: number; folderId: number };
const treeRows = computed(() => {
  const rows: TreeRow[] = [];
  const walk = (parent: number | null, depth: number) => {
    for (const it of itemsIn(parent)) {
      if (it.kind === "folder") {
        const open = expandedFolders.value.includes(it.id);
        const kids = itemsIn(it.id);
        rows.push({ kind: "item", key: itemKey(it), depth, parent, item: it, expanded: open, count: kids.length });
        if (!open) continue;
        if (kids.length === 0)
          rows.push({ kind: "empty", key: `empty:${it.id}`, depth: depth + 1, folderId: it.id });
        else walk(it.id, depth + 1);
      } else {
        rows.push({ kind: "item", key: itemKey(it), depth, parent, item: it, expanded: false, count: 0 });
      }
    }
  };
  walk(null, 0);
  return rows;
});

// Flat "pick a folder" list for the two forms, indented to show nesting.
const folderOptions = computed(() => {
  const out: { id: number; label: string }[] = [];
  const walk = (parent: number | null, depth: number) => {
    for (const it of itemsIn(parent)) {
      if (it.kind !== "folder") continue;
      out.push({ id: it.id, label: `${"   ".repeat(depth)}${depth ? "└ " : ""}${it.name}` });
      walk(it.id, depth + 1);
    }
  };
  walk(null, 0);
  return out;
});

// Same list, minus the folder being edited and its own subtree.
const parentOptions = computed(() => {
  const id = folderDraft.value.id;
  if (id == null) return folderOptions.value;
  return folderOptions.value.filter((o) => !isDescendantOf(o.id, id));
});

function toggleFolder(id: number) {
  const i = expandedFolders.value.indexOf(id);
  if (i === -1) expandedFolders.value.push(id);
  else expandedFolders.value.splice(i, 1);
}

function openFolder(id: number | null) {
  currentFolder.value = id;
  editing.value = false;
  editingFolder.value = false;
  if (id != null && !expandedFolders.value.includes(id)) expandedFolders.value.push(id);
}

// --- Folder create / rename / delete ----------------------------------------

const folderDraft = ref<{ id: number | null; name: string; parent_id: number | null }>({
  id: null,
  name: "",
  parent_id: null,
});
const editingFolder = ref(false);

function newFolder() {
  folderDraft.value = { id: null, name: "", parent_id: currentFolder.value };
  editing.value = false;
  editingFolder.value = true;
}

function renameFolder(f: Folder) {
  folderDraft.value = { id: f.id, name: f.name, parent_id: f.parent_id ?? null };
  editing.value = false;
  editingFolder.value = true;
}

async function saveFolder() {
  if (!db.value) return;
  const d = folderDraft.value;
  const name = d.name.trim();
  if (!name) {
    status.value = "Folder name is required";
    return;
  }
  if (d.id != null && d.parent_id != null && isDescendantOf(d.parent_id, d.id)) {
    status.value = "A folder cannot be moved inside itself";
    return;
  }
  try {
    if (d.id == null) {
      await db.value.execute(
        "INSERT INTO folders (name, parent_id, sort_order) VALUES ($1, $2, $3)",
        [name, d.parent_id, itemsIn(d.parent_id).length],
      );
    } else {
      const prev = folders.value.find((f) => f.id === d.id);
      // Re-parenting puts it last in the destination; a plain rename keeps its place.
      const order =
        (prev?.parent_id ?? null) !== d.parent_id
          ? itemsIn(d.parent_id).length
          : (prev?.sort_order ?? 0);
      await db.value.execute(
        "UPDATE folders SET name=$1, parent_id=$2, sort_order=$3 WHERE id=$4",
        [name, d.parent_id, order, d.id],
      );
    }
    await loadAll();
    editingFolder.value = false;
    status.value = "Saved";
  } catch (e) {
    status.value = `Save failed: ${e}`;
  }
}

// Deleting a folder never deletes connections: everything inside moves up to
// the deleted folder's parent.
async function deleteFolder(f: Folder) {
  if (!db.value) return;
  const n = folderCount(f.id);
  const msg = n
    ? `Delete folder "${f.name}"?\n\nIts ${n} item(s) will move to the parent folder.`
    : `Delete folder "${f.name}"?`;
  if (!confirm(msg)) return;
  const parent = f.parent_id ?? null;
  try {
    await db.value.execute("UPDATE folders SET parent_id=$1 WHERE parent_id=$2", [parent, f.id]);
    await db.value.execute("UPDATE connections SET folder_id=$1 WHERE folder_id=$2", [parent, f.id]);
    await db.value.execute("DELETE FROM folders WHERE id=$1", [f.id]);
    if (currentFolder.value === f.id) currentFolder.value = parent;
    await loadAll();
    // Renumber the destination so the merged items get a clean order.
    await persistOrder(itemsIn(parent));
    await loadAll();
    status.value = "Folder deleted";
  } catch (e) {
    status.value = `Delete failed: ${e}`;
  }
}

// Restore cached thumbnails for the current connections from localStorage.
function loadThumbnails() {
  const map: Record<number, string> = {};
  for (const c of connections.value) {
    const t = localStorage.getItem(`thumb:${c.id}`);
    if (t) map[c.id] = t;
  }
  thumbnails.value = map;
}

// --- Drag & drop: reorder cards, or drop one onto a folder to move it in -----
//
// Deliberately built on plain mouse events rather than HTML5 drag-and-drop:
// the webview hands native drag/drop to the OS, so `dragover`/`drop` are
// unreliable inside a Tauri window. Mouse events always arrive, and they also
// let us draw our own drag ghost. Drop targets are found by hit-testing
// `data-drop-*` attributes under the cursor.

// The item being dragged (source), and where the drop would currently land.
const dragItem = ref<{ kind: "folder" | "conn"; id: number } | null>(null);
type DropHint =
  // `parent` is the folder the reorder happens in — the grid can only reorder
  // the folder it has open, but the sidebar tree shows several at once.
  | { key: string; mode: "before" | "after"; target: Item; parent: number | null }
  | { key: string; mode: "into"; folderId: number | null };
const dropHint = ref<DropHint | null>(null);
// Label + position of the ghost that follows the cursor while dragging.
const dragGhost = ref<{ label: string; x: number; y: number } | null>(null);

// A drag ends with a mouseup, which the browser follows with a click; that
// click must not also "connect" or "open" the item that was dropped.
let suppressClick = false;
// Pending press: a drag only starts once the pointer has moved a few pixels,
// so ordinary clicks still work.
let press: { item: Item; x: number; y: number } | null = null;

function onItemMouseDown(e: MouseEvent, i: Item) {
  // Left button only, and never when the press began on a control.
  if (e.button !== 0) return;
  if ((e.target as HTMLElement).closest("button, .tree-caret")) return;
  // Clear any guard left over from a previous drag whose click never landed.
  suppressClick = false;
  press = { item: i, x: e.clientX, y: e.clientY };
  window.addEventListener("mousemove", onDragMove);
  window.addEventListener("mouseup", onDragUp);
}

function onDragMove(e: MouseEvent) {
  if (!press) return;
  if (!dragItem.value) {
    // Below the threshold this is still a click, not a drag.
    if (Math.hypot(e.clientX - press.x, e.clientY - press.y) < 5) return;
    dragItem.value = { kind: press.item.kind, id: press.item.id };
    dragGhost.value = { label: press.item.name, x: e.clientX, y: e.clientY };
    document.body.classList.add("dragging");
  }
  e.preventDefault();
  dragGhost.value = { label: press.item.name, x: e.clientX, y: e.clientY };
  updateDropHint(e.clientX, e.clientY);
}

async function onDragUp() {
  window.removeEventListener("mousemove", onDragMove);
  window.removeEventListener("mouseup", onDragUp);
  const d = dragItem.value;
  const hint = dropHint.value;
  press = null;
  dragItem.value = null;
  dragGhost.value = null;
  dropHint.value = null;
  document.body.classList.remove("dragging");
  if (!d || !hint) return;
  suppressClick = true;
  await applyDrop(d, hint);
}

// Cancel an in-flight drag when the window loses the mouse (e.g. Esc/alt-tab).
function cancelDrag() {
  if (!press && !dragItem.value) return;
  window.removeEventListener("mousemove", onDragMove);
  window.removeEventListener("mouseup", onDragUp);
  press = null;
  dragItem.value = null;
  dragGhost.value = null;
  dropHint.value = null;
  document.body.classList.remove("dragging");
}

// Swallow the click the browser emits right after a completed drag.
function onItemClick(i: Item) {
  if (suppressClick) {
    suppressClick = false;
    return;
  }
  if (i.kind === "folder") openFolder(i.id);
  else connectBuiltin(i.conn);
}

// Hit-test whatever is under the cursor for the nearest declared drop target.
function updateDropHint(x: number, y: number) {
  const d = dragItem.value;
  if (!d) return;
  const el = (document.elementFromPoint(x, y) as HTMLElement | null)?.closest<HTMLElement>(
    "[data-drop-key]",
  );
  if (!el) {
    dropHint.value = null;
    return;
  }
  const key = el.dataset.dropKey!;
  const kind = el.dataset.dropKind;

  if (kind === "card") {
    const target = currentItems.value.find((i) => itemKey(i) === key);
    if (!target) {
      dropHint.value = null;
      return;
    }
    const r = el.getBoundingClientRect();
    const rel = (x - r.left) / r.width;
    const self = d.kind === target.kind && d.id === target.id;
    // The middle of a folder card means "put it inside"; the outer quarters
    // (and anywhere on a connection card) mean "insert on this side".
    if (target.kind === "folder" && !self && rel > 0.25 && rel < 0.75 && canMove(d, target.id)) {
      dropHint.value = { key, mode: "into", folderId: target.id };
    } else {
      dropHint.value = {
        key,
        mode: rel < 0.5 ? "before" : "after",
        target,
        parent: currentFolder.value,
      };
    }
    return;
  }

  // A sidebar row. Same idea as a card but split vertically: the middle band of
  // a folder row means "inside", the top/bottom mean "insert above/below".
  if (kind === "tree") {
    const parentRaw = el.dataset.dropParent;
    const parent = parentRaw ? Number(parentRaw) : null;
    const target = itemsIn(parent).find((i) => itemKey(i) === el.dataset.dropItem);
    if (!target) {
      dropHint.value = null;
      return;
    }
    const r = el.getBoundingClientRect();
    const rel = (y - r.top) / r.height;
    const self = d.kind === target.kind && d.id === target.id;
    if (target.kind === "folder" && !self && rel > 0.25 && rel < 0.75 && canMove(d, target.id)) {
      dropHint.value = { key, mode: "into", folderId: target.id };
    } else {
      dropHint.value = { key, mode: rel < 0.5 ? "before" : "after", target, parent };
    }
    return;
  }

  // The grid background. Most of the pointer's travel across a card grid is
  // over the gutters between cards, so treat those as insertion points against
  // the nearest card in the same row — otherwise a near-miss on a reorder would
  // silently become "move to the end of this folder".
  if (key === "grid") {
    const near = nearestCardHint(x, y);
    if (near) {
      dropHint.value = near;
      return;
    }
  }

  // A folder target that isn't a card: a breadcrumb, a sidebar row, or the
  // empty space below the cards (= the folder currently open).
  const raw = el.dataset.dropFolder;
  const folderId = raw ? Number(raw) : null;
  dropHint.value = canMove(d, folderId) ? { key, mode: "into", folderId } : null;
}

// Closest card to the cursor within the same row band, as an insertion point.
// Returns null when the cursor is below/away from every card, which the caller
// reads as "drop into the open folder, at the end".
function nearestCardHint(x: number, y: number): DropHint | null {
  const cards = document.querySelectorAll<HTMLElement>('.conn-grid [data-drop-kind="card"]');
  let best: { el: HTMLElement; rect: DOMRect; dist: number } | null = null;
  for (const el of cards) {
    const rect = el.getBoundingClientRect();
    // Only cards on the pointer's row, with a little slack for the gutter.
    if (y < rect.top - 12 || y > rect.bottom + 12) continue;
    const dist = Math.abs(x - (rect.left + rect.width / 2));
    if (!best || dist < best.dist) best = { el, rect, dist };
  }
  if (!best) return null;
  const key = best.el.dataset.dropKey!;
  const target = currentItems.value.find((i) => itemKey(i) === key);
  if (!target) return null;
  return {
    key,
    mode: x < best.rect.left + best.rect.width / 2 ? "before" : "after",
    target,
    parent: currentFolder.value,
  };
}

async function applyDrop(d: { kind: "folder" | "conn"; id: number }, hint: DropHint) {
  if (!db.value) return;
  if (hint.mode === "into") {
    await moveInto(d, hint.folderId);
    return;
  }

  const target = hint.target;
  if (d.kind === target.kind && d.id === target.id) return;

  try {
    const list = itemsIn(hint.parent);
    const to = list.findIndex((i) => i.kind === target.kind && i.id === target.id);
    if (to === -1) return;
    let insertAt = hint.mode === "after" ? to + 1 : to;
    const from = list.findIndex((i) => i.kind === d.kind && i.id === d.id);

    if (from === -1) {
      // Dragged in from another folder: bring it into this one first, then slot
      // it into the requested position.
      await moveInto(d, hint.parent);
      const fresh = itemsIn(hint.parent);
      const idx = fresh.findIndex((i) => i.kind === d.kind && i.id === d.id);
      if (idx === -1) return;
      const [moved] = fresh.splice(idx, 1);
      fresh.splice(Math.min(insertAt, fresh.length), 0, moved);
      await persistOrder(fresh);
    } else {
      const [moved] = list.splice(from, 1);
      if (from < insertAt) insertAt--; // removing shifted everything after it
      list.splice(insertAt, 0, moved);
      await persistOrder(list);
    }
    await loadAll();
  } catch (e) {
    status.value = `Reorder failed: ${e}`;
  }
}

function canMove(d: { kind: string; id: number }, folderId: number | null): boolean {
  if (d.kind !== "folder" || folderId == null) return true;
  return !isDescendantOf(folderId, d.id);
}

// Rewrite `sort_order` as 0..n-1 over the given order. Lists are short (one
// folder's contents), so a straightforward loop of UPDATEs is plenty.
async function persistOrder(list: Item[]) {
  if (!db.value) return;
  for (let i = 0; i < list.length; i++) {
    const it = list[i];
    const table = it.kind === "folder" ? "folders" : "connections";
    await db.value.execute(`UPDATE ${table} SET sort_order=$1 WHERE id=$2`, [i, it.id]);
  }
}

// Move an item into `folderId`, placing it last there.
async function moveInto(d: { kind: "folder" | "conn"; id: number }, folderId: number | null) {
  if (!db.value) return;
  if (!canMove(d, folderId)) {
    status.value = "A folder cannot be moved inside itself";
    return;
  }
  const order = itemsIn(folderId).length;
  try {
    if (d.kind === "folder") {
      await db.value.execute("UPDATE folders SET parent_id=$1, sort_order=$2 WHERE id=$3", [
        folderId, order, d.id,
      ]);
    } else {
      await db.value.execute("UPDATE connections SET folder_id=$1, sort_order=$2 WHERE id=$3", [
        folderId, order, d.id,
      ]);
    }
    await loadAll();
  } catch (e) {
    status.value = `Move failed: ${e}`;
  }
}

// Which drop indicator the target with this key should show right now.
function dropClassFor(key: string) {
  const h = dropHint.value;
  return h && h.key === key ? `drop-${h.mode}` : "";
}
function dropClass(i: Item) {
  return dropClassFor(itemKey(i));
}

function newConnection() {
  draft.value = blankDraft(currentFolder.value);
  editing.value = true;
  editingFolder.value = false;
}

function editConn(c: Connection) {
  draft.value = {
    id: c.id,
    name: c.name,
    host: c.host,
    port: c.port,
    username: c.username,
    protocol: c.protocol,
    password: "",
    folder_id: c.folder_id ?? null,
    legacy_graphics: !!c.legacy_graphics,
  };
  editing.value = true;
  editingFolder.value = false;
}

async function saveDraft() {
  if (!db.value) return;
  const d = draft.value;
  if (!d.name || !d.host || !d.username) {
    status.value = "Name, host, and username are required";
    return;
  }
  try {
    let id: number;
    if (d.id == null) {
      const res = await db.value.execute(
        "INSERT INTO connections (name, host, port, username, protocol, folder_id, sort_order, legacy_graphics) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        [d.name, d.host, d.port, d.username, d.protocol, d.folder_id, itemsIn(d.folder_id).length, d.legacy_graphics ? 1 : 0],
      );
      id = Number(res.lastInsertId);
    } else {
      id = d.id;
      const prev = connections.value.find((c) => c.id === id);
      // Changing folder in the form drops it at the end of the new folder;
      // otherwise the manual position is preserved.
      const order =
        (prev?.folder_id ?? null) !== d.folder_id
          ? itemsIn(d.folder_id).length
          : (prev?.sort_order ?? 0);
      await db.value.execute(
        "UPDATE connections SET name=$1, host=$2, port=$3, username=$4, protocol=$5, folder_id=$6, sort_order=$7, legacy_graphics=$8 WHERE id=$9",
        [d.name, d.host, d.port, d.username, d.protocol, d.folder_id, order, d.legacy_graphics ? 1 : 0, id],
      );
    }
    if (d.password) {
      await invoke("save_password", { connectionId: id, password: d.password });
    }
    await loadAll();
    editing.value = false;
    status.value = "Saved";
  } catch (e) {
    status.value = `Save failed: ${e}`;
  }
}

async function deleteConn(c: Connection) {
  if (!db.value) return;
  if (!confirm(`Delete "${c.name}"?`)) return;
  try {
    await db.value.execute("DELETE FROM connections WHERE id=$1", [c.id]);
    await invoke("delete_password", { connectionId: c.id });
    try { localStorage.removeItem(`thumb:${c.id}`); } catch { /* ignore */ }
    await loadAll();
    status.value = "Deleted";
  } catch (e) {
    status.value = `Delete failed: ${e}`;
  }
}

// Native Windows fallback: hand off to mstsc.exe in its own window.
async function connectMstsc(c: Connection) {
  try {
    status.value = `Launching Windows RDP for ${c.name}...`;
    await invoke("connect_rdp", {
      req: {
        id: c.id,
        host: c.host,
        port: c.port,
        username: c.username,
        protocol: c.protocol,
        client: "mstsc",
      },
    });
    status.value = `Launched ${c.name} (Windows RDP)`;
  } catch (e) {
    status.value = `Connect failed (Windows RDP): ${e}`;
  }
}

// --- Built-in (embedded IronRDP) sessions: tabbed, several at once -----------

// Reactive metadata shown in the tab strip and the connecting overlay; one
// entry per open session. `state` drives the overlay: it covers the canvas
// until the first frame arrives, and returns if the session ends.
type SessionMeta = {
  id: number;
  name: string;
  status: string;
  host: string;
  port: number;
  username: string;
  protocol: string;
  state: "connecting" | "live" | "ended";
  since: number;
};
const sessions = ref<SessionMeta[]>([]);
// Active tab: "home" = the connection manager, or a session id.
const activeTab = ref<number | "home">("home");
let nextSessionId = 1;

// Let the mouse wheel scroll the tab strip horizontally.
function onTabsWheel(e: WheelEvent) {
  (e.currentTarget as HTMLElement).scrollLeft += e.deltaY || e.deltaX;
}

// Non-reactive per-session runtime. The canvas/ctx must NOT be Vue-proxied, so
// these live outside the reactive system, keyed by session id.
type Runtime = {
  canvas: HTMLCanvasElement | null;
  ctx: CanvasRenderingContext2D | null;
  width: number;
  height: number;
  lastX: number;
  lastY: number;
  connId: number; // which saved connection this session belongs to
  protocol: string; // "rdp" | "vnc" — routes input/disconnect/resize commands
  lastThumb: number; // timestamp of the last thumbnail capture
  audioCtx: AudioContext | null;
  audioNextTime: number;
  audioRate: number;
  audioChannels: number;
};
const runtimes = new Map<number, Runtime>();

function setCanvas(id: number, el: Element | null) {
  const rt = runtimes.get(id);
  if (rt) rt.canvas = (el as HTMLCanvasElement) ?? null;
}

// Run `fn` when the main thread is idle, so it never delays a frame. Safari and
// older WebViews have no requestIdleCallback; a macrotask is close enough.
function scheduleIdle(fn: () => void) {
  const ric = (window as unknown as {
    requestIdleCallback?: (cb: () => void, opts?: { timeout: number }) => number;
  }).requestIdleCallback;
  if (ric) ric(fn, { timeout: 2000 });
  else setTimeout(fn, 0);
}

// Snapshot the session canvas into a small JPEG thumbnail for its connection.
function captureThumbnail(sessionId: number) {
  const rt = runtimes.get(sessionId);
  if (!rt || !rt.canvas || rt.width === 0 || rt.height === 0) return;
  const tw = 320;
  const th = Math.max(1, Math.round((tw * rt.height) / rt.width));
  const off = document.createElement("canvas");
  off.width = tw;
  off.height = th;
  const octx = off.getContext("2d");
  if (!octx) return;
  try {
    octx.drawImage(rt.canvas, 0, 0, tw, th);
    const url = off.toDataURL("image/jpeg", 0.6);
    thumbnails.value = { ...thumbnails.value, [rt.connId]: url };
    localStorage.setItem(`thumb:${rt.connId}`, url);
  } catch {
    /* canvas not ready / storage full — skip this capture */
  }
}

function setStatus(id: number, text: string, state?: SessionMeta["state"]) {
  const s = sessions.value.find((s) => s.id === id);
  if (!s) return;
  s.status = text;
  if (state) s.state = state;
}

function handleFrame(id: number, data: ArrayBuffer | unknown) {
  if (!(data instanceof ArrayBuffer)) return;
  const rt = runtimes.get(id);
  if (!rt) return;
  const view = new DataView(data);
  const tag = view.getUint8(0);
  if (tag === 0) {
    // init: [0][width u16][height u16] — first frame means we're connected.
    rt.width = view.getUint16(1, true);
    rt.height = view.getUint16(3, true);
    if (rt.canvas) {
      rt.canvas.width = rt.width;
      rt.canvas.height = rt.height;
      rt.ctx = rt.canvas.getContext("2d");
    }
    setStatus(id, "", "live");
  } else if (tag === 3) {
    // rect: [3][x u16][y u16][w u16][h u16][rgba...] — an arbitrary dirty
    // rectangle: a VNC/RFB update, or an RDP Graphics Pipeline (EGFX) surface
    // update. Neither arrives as a full-width strip the way legacy RDP does.
    if (!rt.ctx) return;
    const x = view.getUint16(1, true);
    const y = view.getUint16(3, true);
    const w = view.getUint16(5, true);
    const h = view.getUint16(7, true);
    const rgba = new Uint8ClampedArray(data.slice(9));
    if (rgba.length === w * h * 4) {
      rt.ctx.putImageData(new ImageData(rgba, w, h), x, y);
    }
    // A JPEG encode plus a localStorage write costs several milliseconds; running
    // it between frames would show up as a hitch every few seconds. Stamp the time
    // now so the capture is only queued once, and let it run when the main thread
    // is free.
    if (Date.now() - rt.lastThumb > 4000) {
      rt.lastThumb = Date.now();
      scheduleIdle(() => captureThumbnail(id));
    }
  } else if (tag === 2) {
    // status: [2][utf-8 message] — a disconnect/close/error notice.
    setStatus(id, new TextDecoder().decode(new Uint8Array(data, 1)));
  } else if (tag === 7) {
    // clipboard: [7][utf-8 text] — remote copied text; mirror to local clipboard.
    // Was tag 3 until EGFX landed, which collided with the rect tag above — the
    // rect branch matched first, so this branch was unreachable.
    const text = new TextDecoder().decode(new Uint8Array(data, 1));
    navigator.clipboard.writeText(text).catch(() => {
      /* clipboard write blocked (no permission / not focused) — ignore */
    });
  } else if (tag === 4) {
    console.log(`[audio] tag 4 received, bytes=${data.byteLength}`);
    // audio: [4][pcm] — remote sound, interleaved 16-bit PCM in the negotiated format.
    playAudio(id, data);
  } else if (tag === 6) {
    // ended: [6][utf-8 message] — the session stopped, for good or ill.
    setStatus(id, new TextDecoder().decode(new Uint8Array(data, 1)), "ended");
  } else if (tag === 5) {
    // audio format: [5][rate u32][channels u8][bits u8] — sizes the player.
    const rt = runtimes.get(id);
    if (rt) {
      rt.audioRate = view.getUint32(1, true);
      rt.audioChannels = view.getUint8(5) || 2;
      // Recreate the context at the new rate on the next chunk.
      rt.audioCtx?.close().catch(() => {});
      rt.audioCtx = null;
      rt.audioNextTime = 0;
      console.log(`[audio] format ${rt.audioRate}Hz ${rt.audioChannels}ch`);
    }
  }
}

// Play one PCM chunk, scheduled after the previous one so playback is gapless.
function playAudio(id: number, data: ArrayBuffer) {
  const rt = runtimes.get(id);
  if (!rt) return;
  const rate = rt.audioRate || 44100;
  const channels = rt.audioChannels || 2;
  if (!rt.audioCtx) {
    rt.audioCtx = new AudioContext({ sampleRate: rate });
    rt.audioNextTime = 0;
  }
  const ctx = rt.audioCtx;
  // Browsers start the context suspended until a user gesture; nudge it.
  if (ctx.state === "suspended") {
    ctx.resume()
      .then(() => console.log(`[audio] resumed, state=${ctx.state}`))
      .catch((e) => console.warn("[audio] resume failed", e));
  }

  // Copy from byte offset 1 (skip the tag) into an aligned Int16 view.
  const pcm = new Int16Array(data.slice(1));
  const frames = Math.floor(pcm.length / channels);
  if (frames === 0) return;

  if (rt.audioNextTime === 0) {
    console.log(`[audio] first chunk: ${pcm.length} samples, ctx.state=${ctx.state}`);
  }

  const buf = ctx.createBuffer(channels, frames, rate);
  for (let ch = 0; ch < channels; ch++) {
    const out = buf.getChannelData(ch);
    for (let i = 0; i < frames; i++) out[i] = pcm[i * channels + ch] / 32768;
  }

  const node = ctx.createBufferSource();
  node.buffer = buf;
  node.connect(ctx.destination);
  const now = ctx.currentTime;
  // If we've fallen behind (underrun), restart the playhead from now.
  if (rt.audioNextTime < now) rt.audioNextTime = now;
  node.start(rt.audioNextTime);
  rt.audioNextTime += buf.duration;
}

// Read the local OS clipboard and offer it to the remote session, so a paste
// (Ctrl+V) inside the remote finds the same text. Best-effort: reading can fail
// without focus/permission, in which case we just skip.
async function offerLocalClipboard(id: number) {
  try {
    const text = await navigator.clipboard.readText();
    if (text) await invoke("rdp_clipboard_offer", { sessionId: id, text });
  } catch {
    /* clipboard not readable — ignore */
  }
}

// Map a canvas pointer event to remote-desktop pixel coordinates.
function toRemoteCoords(id: number, e: MouseEvent): { x: number; y: number } | null {
  const rt = runtimes.get(id);
  if (!rt || !rt.canvas || rt.width === 0) return null;
  const rect = rt.canvas.getBoundingClientRect();
  if (rect.width === 0 || rect.height === 0) return null;
  const x = Math.round(((e.clientX - rect.left) / rect.width) * rt.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * rt.height);
  return {
    x: Math.max(0, Math.min(rt.width - 1, x)),
    y: Math.max(0, Math.min(rt.height - 1, y)),
  };
}

function sendInput(id: number, event: Record<string, unknown>) {
  const cmd = runtimes.get(id)?.protocol === "vnc" ? "vnc_input" : "rdp_input";
  invoke(cmd, { sessionId: id, event }).catch(() => {});
}

function onMouseMove(id: number, e: MouseEvent) {
  const rt = runtimes.get(id);
  const p = toRemoteCoords(id, e);
  if (!rt || !p || (p.x === rt.lastX && p.y === rt.lastY)) return;
  rt.lastX = p.x;
  rt.lastY = p.y;
  sendInput(id, { kind: "move", x: p.x, y: p.y });
}

function onMouseButton(id: number, e: MouseEvent, pressed: boolean) {
  e.preventDefault();
  runtimes.get(id)?.canvas?.focus();
  const p = toRemoteCoords(id, e);
  if (p) sendInput(id, { kind: "move", x: p.x, y: p.y });
  sendInput(id, { kind: "button", button: e.button, pressed });
}

function onWheel(id: number, e: WheelEvent) {
  e.preventDefault();
  // RDP uses ~120 units per wheel notch; up/left are positive.
  const vertical = Math.abs(e.deltaY) >= Math.abs(e.deltaX);
  const raw = vertical ? -e.deltaY : -e.deltaX;
  const delta = Math.max(-32768, Math.min(32767, Math.round(raw * 4)));
  sendInput(id, { kind: "wheel", vertical, delta });
}

// Keyboard is captured globally and routed to the active session tab only.
// VNC speaks X11 keysyms; RDP speaks set-1 scancodes.
function onKey(e: KeyboardEvent, pressed: boolean) {
  if (typeof activeTab.value !== "number") return;
  const id = activeTab.value;
  if (runtimes.get(id)?.protocol === "vnc") {
    const keysym = keysymFor(e);
    if (keysym === undefined) return;
    e.preventDefault();
    sendInput(id, { kind: "key", keysym, pressed });
  } else {
    const scancode = SCANCODES[e.code];
    if (scancode === undefined) return;
    e.preventDefault();
    sendInput(id, { kind: "key", scancode, pressed });
  }
}

const keyDown = (e: KeyboardEvent) => onKey(e, true);
const keyUp = (e: KeyboardEvent) => onKey(e, false);

// Desktop size to request: the available session area, clamped to RDP's limits
// (200–8192) with an even width, as the protocol requires.
function currentSize(): { width: number; height: number } {
  const even = (n: number) => Math.floor(n / 2) * 2;
  const clamp = (n: number) => Math.max(200, Math.min(8192, n));
  // In fullscreen the title bar is hidden, so the session owns the full height.
  const chrome = isFullscreen.value ? 0 : 38; // tab bar height (controls overlay)
  return {
    width: clamp(even(window.innerWidth)),
    height: clamp(even(window.innerHeight - chrome)),
  };
}

let resizeTimer: number | undefined;
function onWindowResize() {
  if (typeof activeTab.value !== "number") return;
  const id = activeTab.value;
  window.clearTimeout(resizeTimer);
  // Debounce: only ask the server to resize once the drag settles.
  resizeTimer = window.setTimeout(() => {
    const { width, height } = currentSize();
    // vnc_resize is best-effort (server-fixed size); rdp_resize drives the
    // Display Control channel. Route to the active session's protocol.
    const cmd = runtimes.get(id)?.protocol === "vnc" ? "vnc_resize" : "rdp_resize";
    invoke(cmd, { sessionId: id, width, height }).catch(() => {});
  }, 250);
}
function unlockAudio(id: number) {
  const rt = runtimes.get(id);
  if (!rt) return;

  if (!rt.audioCtx) {
    rt.audioCtx = new AudioContext({ sampleRate: rt.audioRate || 44100 });
    rt.audioNextTime = 0;
  }

  rt.audioCtx.resume().then(() => {
    console.log(`[audio] unlocked, state=${rt.audioCtx?.state}`);
  }).catch((e) => {
    console.warn("[audio] unlock failed", e);
  });
}
async function connectBuiltin(c: Connection) {
  const id = nextSessionId++;
  
  runtimes.set(id, {
    canvas: null, ctx: null, width: 0, height: 0, lastX: -1, lastY: -1,
    connId: c.id, protocol: c.protocol, lastThumb: 0, audioCtx: null, audioNextTime: 0,
    audioRate: 44100, audioChannels: 2,
  });
  unlockAudio(id);
  
  sessions.value.push({
    id,
    name: c.name,
    status: "Starting...",
    host: c.host,
    port: c.port,
    username: c.username,
    protocol: c.protocol,
    state: "connecting",
    since: Date.now(),
  });
  activeTab.value = id;
  await nextTick();
  await startSession(id, c);
}

// Open (or re-open) the backend session for an existing tab. The backend keys
// sessions by id and cancels any previous one under the same key, so this
// doubles as reconnect.
async function startSession(id: number, c: Connection) {
  // Request a desktop size matching the available window.
  const { width, height } = currentSize();

  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (data) => handleFrame(id, data);

  runtimes.get(id)?.canvas?.focus();

  const cmd = c.protocol === "vnc" ? "vnc_connect" : "rdp_connect";
  try {
    await invoke(cmd, {
      sessionId: id,
      req: {
        id: c.id,
        host: c.host,
        port: c.port,
        username: c.username,
        protocol: c.protocol,
        client: "ironrdp",
        legacy_graphics: !!c.legacy_graphics,
      },
      width,
      height,
      onFrame: channel,
    });
  } catch (e) {
    setStatus(id, `Connect failed: ${e}`, "ended");
  }
}

// Retry a session that ended, reusing the same tab.
async function retrySession(id: number) {
  const rt = runtimes.get(id);
  const c = connections.value.find((x) => x.id === rt?.connId);
  if (!rt || !c) {
    setStatus(id, "That connection no longer exists", "ended");
    return;
  }
  rt.width = 0;
  rt.height = 0;
  setStatus(id, "Starting...", "connecting");
  await startSession(id, c);
}

async function closeTab(id: number) {
  const idx = sessions.value.findIndex((s) => s.id === id);
  // Grab a final snapshot of the remote before tearing the session down.
  captureThumbnail(id);
  const cmd = runtimes.get(id)?.protocol === "vnc" ? "vnc_disconnect" : "rdp_disconnect";
  try {
    await invoke(cmd, { sessionId: id });
  } catch {
    // ignore — session may already be gone
  }
  runtimes.get(id)?.audioCtx?.close().catch(() => {});
  runtimes.delete(id);
  if (idx !== -1) sessions.value.splice(idx, 1);
  if (activeTab.value === id) {
    // Fall back to the neighbouring tab, or home if none remain.
    activeTab.value = sessions.value.length
      ? sessions.value[Math.min(idx, sessions.value.length - 1)].id
      : "home";
  }
}

function cancelEdit() {
  editing.value = false;
  editingFolder.value = false;
  draft.value = blankDraft(currentFolder.value);
  status.value = "";
}

onMounted(async () => {
  // Global key capture; handlers no-op unless a session tab is active.
  window.addEventListener("keydown", keyDown, true);
  window.addEventListener("keyup", keyUp, true);
  window.addEventListener("resize", onWindowResize);
  // Abandon an in-flight card drag if the window loses focus or Esc is pressed.
  window.addEventListener("blur", cancelDrag);
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") cancelDrag();
  });
  // When the app regains focus (e.g. after copying in another app), prime the
  // active session's remote clipboard with the local clipboard contents.
  window.addEventListener("focus", () => {
    if (typeof activeTab.value === "number") offerLocalClipboard(activeTab.value);
  });
  // Keep the maximize/restore button in sync with the actual window state.
  refreshMaximized();
  appWindow.onResized(async () => {
    refreshMaximized();
    isFullscreen.value = await appWindow.isFullscreen();
  });
  try {
    db.value = await Database.load("sqlite:remote.db");
    await loadAll();
  } catch (e) {
    status.value = `DB load failed: ${e}`;
  }
});
</script>

<template>
  <div class="root" :class="{ fullscreen: isFullscreen }">
    <div class="titlebar" data-tauri-drag-region>
      <div class="brand" data-tauri-drag-region>
        <img src="/app-icon.png" class="brand-icon" alt="" data-tauri-drag-region />
        <span class="brand-title" data-tauri-drag-region>Control Desktop</span>
      </div>
      <nav class="tabs" @wheel.prevent="onTabsWheel">
        <button
          class="tab"
          :class="{ active: activeTab === 'home' }"
          @click="activeTab = 'home'"
        >
          Connections
        </button>
        <button
          v-for="s in sessions"
          :key="s.id"
          class="tab"
          :class="{ active: activeTab === s.id }"
          @click="activeTab = s.id"
        >
          <span class="tab-name">{{ s.name }}</span>
          <span class="tab-close" title="Disconnect" @click.stop="closeTab(s.id)">×</span>
        </button>
      </nav>
      <div class="drag-spacer" data-tauri-drag-region></div>
      <div class="window-controls">
        <button class="win-btn" title="Minimize" @click="winMinimize">—</button>
        <button class="win-btn" :title="isMaximized ? 'Restore' : 'Maximize'" @click="winToggleMaximize">
          {{ isMaximized ? "❐" : "▢" }}
        </button>
        <button class="win-btn close" title="Close" @click="winClose">✕</button>
      </div>
    </div>

  <div class="app" v-show="activeTab === 'home'">
    <button
      v-show="sidebarCollapsed"
      class="sidebar-expand"
      title="Show connections"
      @click="sidebarCollapsed = false"
    >
      ☰
    </button>
    <aside class="sidebar" :class="{ collapsed: sidebarCollapsed }">
      <div class="sidebar-header">
        <button class="icon-btn" title="Collapse" @click="sidebarCollapsed = true">‹</button>
        <h2>Connections</h2>
        <button class="icon-btn" title="New folder" @click="newFolder">🗀</button>
        <button class="primary" @click="newConnection">+ New</button>
      </div>
      <!-- Folder tree. Rows are pre-flattened with a depth, so no recursive
           component is needed; folders are also drop targets for moves. -->
      <ul class="conn-list tree">
        <li
          class="tree-row root-row"
          :class="{
            active: currentFolder === null,
            'drop-into': dropHint?.key === 'tree:root',
          }"
          data-drop-key="tree:root"
          data-drop-kind="folder"
          @click="openFolder(null)"
        >
          <span class="tree-caret"></span>
          <span class="tree-icon">🏠</span>
          <span class="conn-name">Home</span>
        </li>
        <template v-for="row in treeRows" :key="row.key">
          <li
            v-if="row.kind === 'item'"
            class="tree-row"
            :class="[
              {
                'is-folder': row.item.kind === 'folder',
                active: row.item.kind === 'folder' && currentFolder === row.item.id,
              },
              dropClassFor('tree:' + row.key),
            ]"
            :title="
              row.item.kind === 'folder'
                ? `Open ${row.item.name}`
                : `Connect to ${row.item.name} over ${row.item.conn.protocol.toUpperCase()}`
            "
            :data-drop-key="'tree:' + row.key"
            data-drop-kind="tree"
            :data-drop-item="row.key"
            :data-drop-parent="row.parent ?? ''"
            :data-drop-folder="row.item.kind === 'folder' ? row.item.id : undefined"
            @mousedown="(e) => onItemMouseDown(e, row.item)"
            @click="onItemClick(row.item)"
          >
            <!-- One guide line per nesting level, so a child is unmistakably
                 a child and a following sibling is unmistakably not. Everything
                 is a child of the Home row, hence depth + 1. -->
            <span v-for="n in row.depth + 1" :key="n" class="tree-indent"></span>
            <template v-if="row.item.kind === 'folder'">
              <span class="tree-caret" @click.stop="toggleFolder(row.item.id)">
                {{ row.expanded ? "▾" : "▸" }}
              </span>
              <span class="tree-icon">{{ row.expanded ? "📂" : "📁" }}</span>
              <span class="conn-name">{{ row.item.name }}</span>
              <span class="tree-count">{{ row.count }}</span>
            </template>
            <template v-else>
              <span class="tree-caret"></span>
              <!-- Spell the protocol out; a colour alone isn't readable. -->
              <span class="tree-proto" :class="row.item.conn.protocol">
                {{ row.item.conn.protocol.toUpperCase() }}
              </span>
              <span class="tree-conn">
                <span class="conn-name">{{ row.item.name }}</span>
                <span class="conn-host">
                  {{ row.item.conn.username }}@{{ row.item.conn.host }}:{{ row.item.conn.port }}
                </span>
              </span>
            </template>
          </li>
          <!-- Also a drop target: dropping here fills the empty folder. -->
          <li
            v-else
            class="tree-row tree-empty"
            :class="dropClassFor('tree:' + row.key)"
            :data-drop-key="'tree:' + row.key"
            data-drop-kind="folder"
            :data-drop-folder="row.folderId"
          >
            <span v-for="n in row.depth + 1" :key="n" class="tree-indent"></span>
            <span class="tree-caret"></span>
            <span>Empty</span>
          </li>
        </template>
        <li v-if="treeRows.length === 0" class="empty">
          No saved connections yet
        </li>
      </ul>
    </aside>

    <main class="main" :class="{ shifted: sidebarCollapsed }">
      <section v-if="editing" class="form">
        <h2>{{ draft.id == null ? "New connection" : "Edit connection" }}</h2>
        <label>
          Name
          <input v-model="draft.name" placeholder="My work PC" />
        </label>
        <label>
          Host / IP
          <input v-model="draft.host" placeholder="192.168.1.10 or pc.local" />
        </label>
        <label>
          Port
          <input v-model.number="draft.port" type="number" />
        </label>
        <label>
          Username
          <input v-model="draft.username" placeholder="Administrator" />
        </label>
        <label>
          Password
          <input
            v-model="draft.password"
            type="password"
            :placeholder="draft.id == null ? 'Required' : 'Leave blank to keep existing'"
          />
        </label>
        <label>
          Protocol
          <select v-model="draft.protocol">
            <option value="rdp">RDP</option>
            <option value="vnc">VNC</option>
          </select>
        </label>
        <label v-if="draft.protocol === 'rdp'" class="check">
          <input type="checkbox" v-model="draft.legacy_graphics" />
          <span>
            Legacy graphics
            <small>Skip the Graphics Pipeline. Use for Windows hosts that render wrong; leave off for Linux/GNOME, which requires it.</small>
          </span>
        </label>
        <label>
          Folder
          <select v-model="draft.folder_id">
            <option :value="null">Home (no folder)</option>
            <option v-for="o in folderOptions" :key="o.id" :value="o.id">{{ o.label }}</option>
          </select>
        </label>
        <div class="actions">
          <button class="primary" @click="saveDraft">Save</button>
          <button @click="cancelEdit">Cancel</button>
        </div>
      </section>

      <section v-else-if="editingFolder" class="form">
        <h2>{{ folderDraft.id == null ? "New folder" : "Rename folder" }}</h2>
        <label>
          Name
          <input v-model="folderDraft.name" placeholder="Production servers" @keyup.enter="saveFolder" />
        </label>
        <label>
          Inside
          <select v-model="folderDraft.parent_id">
            <option :value="null">Home (top level)</option>
            <option v-for="o in parentOptions" :key="o.id" :value="o.id">{{ o.label }}</option>
          </select>
        </label>
        <div class="actions">
          <button class="primary" @click="saveFolder">Save</button>
          <button @click="cancelEdit">Cancel</button>
        </div>
      </section>

      <template v-else>
        <!-- Breadcrumb: navigates, and each crumb is a drop target so items can
             be dragged back out to any ancestor folder. -->
        <nav class="crumbs">
          <button
            class="crumb"
            :class="{ 'drop-into': dropHint?.key === 'crumb:root' }"
            data-drop-key="crumb:root"
            data-drop-kind="folder"
            @click="openFolder(null)"
          >
            🏠 Home
          </button>
          <template v-for="f in breadcrumb" :key="f.id">
            <span class="crumb-sep">›</span>
            <button
              class="crumb"
              :class="{ 'drop-into': dropHint?.key === 'crumb:' + f.id }"
              :data-drop-key="'crumb:' + f.id"
              data-drop-kind="folder"
              :data-drop-folder="f.id"
              @click="openFolder(f.id)"
            >
              {{ f.name }}
            </button>
          </template>
          <span class="crumbs-spacer"></span>
          <button class="ghost" title="Create a folder here" @click="newFolder">🗀 New folder</button>
        </nav>

        <!-- Cards for the open folder. Dropping on the empty space below moves
             the dragged item into this folder, at the end. -->
        <div
          class="conn-grid"
          :class="{ 'drop-into': dropHint?.key === 'grid' }"
          data-drop-key="grid"
          data-drop-kind="folder"
          :data-drop-folder="currentFolder ?? ''"
        >
          <div
            v-for="it in currentItems"
            :key="itemKey(it)"
            class="conn-card"
            :class="[it.kind === 'folder' ? 'folder-card' : '', dropClass(it)]"
            :title="it.kind === 'folder' ? `Open ${it.name}` : `Connect to ${it.name}`"
            :data-drop-key="itemKey(it)"
            data-drop-kind="card"
            @mousedown="(e) => onItemMouseDown(e, it)"
            @click="onItemClick(it)"
          >
            <template v-if="it.kind === 'folder'">
              <span class="conn-card-proto folder">FOLDER</span>
              <div class="conn-card-actions">
                <button class="icon-btn" title="Rename / move" @click.stop="renameFolder(it.folder)">✎</button>
                <button class="icon-btn danger" title="Delete folder" @click.stop="deleteFolder(it.folder)">🗑</button>
              </div>
              <div class="conn-card-thumb folder-thumb">
                <span class="folder-glyph">📁</span>
              </div>
              <div class="conn-card-info">
                <div class="conn-card-name">{{ it.name }}</div>
                <div class="conn-card-host">
                  {{ folderCount(it.id) }} item{{ folderCount(it.id) === 1 ? "" : "s" }}
                </div>
              </div>
            </template>
            <template v-else>
              <span class="conn-card-proto" :class="it.conn.protocol">
                {{ it.conn.protocol.toUpperCase() }}
              </span>
              <div class="conn-card-actions">
                <button v-if="it.conn.protocol === 'rdp'" class="icon-btn" title="Open with Windows RDP" @click.stop="connectMstsc(it.conn)">🖥</button>
                <button class="icon-btn" title="Edit" @click.stop="editConn(it.conn)">✎</button>
                <button class="icon-btn danger" title="Delete" @click.stop="deleteConn(it.conn)">🗑</button>
              </div>
              <div class="conn-card-thumb">
                <img v-if="thumbnails[it.id]" :src="thumbnails[it.id]" alt="" draggable="false" />
                <span v-else class="conn-card-thumb-empty">🖥</span>
              </div>
              <div class="conn-card-info">
                <div class="conn-card-name">{{ it.name }}</div>
                <div class="conn-card-host">
                  {{ it.conn.username }}@{{ it.conn.host }}:{{ it.conn.port }}
                </div>
              </div>
            </template>
          </div>

          <button class="conn-card add" @click="newConnection">
            <span class="plus">＋</span>
            <span>New connection</span>
          </button>
        </div>
      </template>

      <div v-if="status" class="status">{{ status }}</div>
    </main>

    <!-- Follows the cursor while dragging; the webview gives us no native
         drag image, so we draw our own. -->
    <div
      v-if="dragGhost"
      class="drag-ghost"
      :style="{ left: dragGhost.x + 12 + 'px', top: dragGhost.y + 12 + 'px' }"
    >
      {{ dragGhost.label }}
    </div>
  </div>

    <div
      v-for="s in sessions"
      :key="s.id"
      class="session"
      v-show="activeTab === s.id"
    >
      <!-- Thin catch strip at top-centre; hovering it reveals the controls. -->
      <div class="session-reveal">
        <!-- Always-visible hint that controls are hidden here. -->
        <div class="session-hint">⌄</div>
        <div class="session-controls">
          <span v-if="s.status && s.state === 'live'" class="session-status-mini">{{ s.status }}</span>
          <button
            class="session-fullscreen"
            :title="isFullscreen ? 'Exit fullscreen' : 'Fullscreen'"
            @click="toggleFullscreen"
          >
            {{ isFullscreen ? "🗗 Exit fullscreen" : "⛶ Fullscreen" }}
          </button>
          <button class="session-disconnect" title="Disconnect" @click="closeTab(s.id)">
            ⏻ Disconnect
          </button>
        </div>
      </div>
      <!-- Covers the canvas until the first frame arrives, and comes back if
           the session ends. The tiny strip up top has no room for detail. -->
      <div v-if="s.state !== 'live'" class="session-overlay">
        <div class="session-overlay-card">
          <div v-if="s.state === 'connecting'" class="spinner"></div>
          <div v-else class="session-overlay-icon">⚠</div>
          <div class="session-overlay-name">{{ s.name }}</div>
          <dl class="session-overlay-facts">
            <dt>Address</dt>
            <dd>{{ s.host }}:{{ s.port }}</dd>
            <dt>User</dt>
            <dd>{{ s.username }}</dd>
            <dt>Protocol</dt>
            <dd>
              <span class="tree-proto" :class="s.protocol">{{ s.protocol.toUpperCase() }}</span>
              <span class="session-overlay-client">
                {{ s.protocol === "vnc" ? "built-in RFB client" : "built-in IronRDP client" }}
              </span>
            </dd>
          </dl>
          <div class="session-overlay-status" :class="{ failed: s.state === 'ended' }">
            {{ s.status }}
          </div>
          <div class="session-overlay-actions">
            <button v-if="s.state === 'ended'" class="primary" @click="retrySession(s.id)">
              Reconnect
            </button>
            <button @click="closeTab(s.id)">Close tab</button>
          </div>
        </div>
      </div>
      <div class="session-canvas-wrap">
        <canvas
          :ref="(el) => setCanvas(s.id, el as Element | null)"
          tabindex="0"
          class="screen"
          @focus="() => offerLocalClipboard(s.id)"
          @mousemove="(e) => onMouseMove(s.id, e)"
          @mousedown="(e) => onMouseButton(s.id, e, true)"
          @mouseup="(e) => onMouseButton(s.id, e, false)"
          @wheel.prevent="(e) => onWheel(s.id, e)"
          @contextmenu.prevent
        ></canvas>
      </div>
    </div>
  </div>
</template>

<style>
:root {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
  color: #1a1a1a;
  background: #f5f5f7;
}
* { box-sizing: border-box; }
body, html, #app { margin: 0; height: 100%; }

.root {
  display: flex;
  flex-direction: column;
  height: 100vh;
}

/* Custom title bar: tabs + drag region + window controls, all in one row. */
.titlebar {
  display: flex;
  align-items: stretch;
  flex: 0 0 38px;
  background: #1f1f23;
  border-bottom: 1px solid #2a2a2e;
  position: relative;
  z-index: 100;
}
/* Fullscreen: hide the title bar so the active session owns the whole screen. */
.root.fullscreen .titlebar { display: none; }
/* App icon + title on the far left of the title bar. */
.brand {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: 0 0 auto;
  padding: 0 12px;
  user-select: none;
}
.brand-icon { width: 18px; height: 18px; }
.brand-title {
  font-size: 13px;
  font-weight: 600;
  color: #e4e4e7;
  white-space: nowrap;
}

.tabs {
  display: flex;
  align-items: center;
  gap: 5px;
  padding: 0 6px;
  flex: 0 1 auto;
  min-width: 0;
  /* Scroll horizontally only; never show a vertical scrollbar. */
  overflow-x: auto;
  overflow-y: hidden;
}
.tabs::-webkit-scrollbar { height: 6px; }
.tabs::-webkit-scrollbar-thumb { background: #3f3f46; border-radius: 3px; }
.tabs::-webkit-scrollbar-track { background: transparent; }

/* Draggable empty area between tabs and the window controls. */
.drag-spacer { flex: 1 1 auto; min-width: 24px; }

.window-controls { display: flex; flex: 0 0 auto; }
.win-btn {
  width: 46px;
  height: 38px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: none;
  border-radius: 0;
  background: transparent;
  color: #c5c5cb;
  font-size: 12px;
  cursor: pointer;
}
.win-btn:hover { background: #2f2f35; color: #fff; }
.win-btn.close:hover { background: #c81e1e; color: #fff; }
.tab {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: 0 0 auto;
  padding: 0 12px;
  height: 28px;
  border: 1px solid transparent;
  border-radius: 7px;
  background: rgba(255, 255, 255, 0.05);
  color: #c5c5cb;
  font-size: 13px;
  white-space: nowrap;
  cursor: pointer;
}
.tab:hover { background: rgba(255, 255, 255, 0.11); }
.tab.active {
  background: #f5f5f7;
  color: #1a1a1a;
  border-color: #f5f5f7;
}
.tab-name { max-width: 180px; overflow: hidden; text-overflow: ellipsis; }
.tab-close {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 18px;
  height: 18px;
  border-radius: 4px;
  font-size: 16px;
  line-height: 1;
  color: inherit;
  opacity: 0.6;
}
.tab-close:hover { background: rgba(128, 128, 128, 0.35); opacity: 1; }

.app {
  display: flex;
  flex: 1;
  min-height: 0;
  position: relative;
}

.sidebar {
  width: 280px;
  flex: 0 0 280px;
  border-right: 1px solid #d0d0d5;
  background: #fafafa;
  display: flex;
  flex-direction: column;
  overflow: hidden;
  transition: width 0.18s ease, flex-basis 0.18s ease;
}
.sidebar.collapsed {
  width: 0;
  flex-basis: 0;
  border-right: none;
}
.sidebar-header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 12px 14px;
  border-bottom: 1px solid #d0d0d5;
}
.sidebar-header h2 { margin: 0; font-size: 15px; flex: 1; white-space: nowrap; }

.icon-btn {
  border: none;
  background: transparent;
  font-size: 18px;
  line-height: 1;
  padding: 2px 8px;
  border-radius: 6px;
  color: inherit;
  cursor: pointer;
}
.icon-btn:hover { background: rgba(128, 128, 128, 0.18); }

/* Floating button to reopen the sidebar when collapsed. */
.sidebar-expand {
  position: absolute;
  top: 10px;
  left: 10px;
  z-index: 5;
  width: 34px;
  height: 30px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1px solid #d0d0d5;
  border-radius: 7px;
  background: #fff;
  font-size: 15px;
  cursor: pointer;
}
.sidebar-expand:hover { background: #f0f0f3; }

.conn-list {
  list-style: none;
  padding: 0;
  margin: 0;
  overflow-y: auto;
  flex: 1;
}
.conn-list li {
  padding: 10px 14px;
  cursor: pointer;
  border-bottom: 1px solid #eee;
}
.conn-list li:hover { background: #ececef; }
.conn-list li.active { background: #d6e3ff; }
.conn-list li.empty {
  color: #888;
  font-style: italic;
  cursor: default;
}
.conn-list li.empty:hover { background: transparent; }
.conn-name { font-weight: 600; font-size: 14px; }
.conn-host { font-size: 12px; color: #666; margin-top: 2px; }

/* Sidebar folder tree: flat rows, indented by depth. */
.conn-list.tree li.tree-row {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 7px 10px;
  border-bottom: none;
  user-select: none;
}
.tree-row.active { background: #e3ebff; }
/* One per nesting level: a fixed-width spacer carrying the guide line that
   marks which folder a row belongs to. */
.tree-indent {
  flex: 0 0 14px;
  align-self: stretch;
  border-left: 1px solid rgba(128, 128, 128, 0.3);
}
.tree-empty {
  font-size: 12px;
  font-style: italic;
  color: #999;
  cursor: default;
}
.tree-empty:hover { background: transparent; }
.tree-caret {
  flex: 0 0 14px;
  font-size: 10px;
  color: #888;
  text-align: center;
  border-radius: 3px;
}
.tree-row.is-folder .tree-caret:hover { background: rgba(128, 128, 128, 0.25); }
.tree-icon { flex: 0 0 auto; font-size: 13px; }
/* Protocol chip where a folder row has its icon, so rows still line up. Fixed
   width keeps the names in a column regardless of RDP/VNC. */
.tree-proto {
  flex: 0 0 30px;
  text-align: center;
  font-size: 9px;
  font-weight: 700;
  letter-spacing: 0.04em;
  color: #fff;
  background: #3b5bdb;
  border-radius: 3px;
  padding: 2px 0;
}
.tree-proto.vnc { background: #0d9488; }
.tree-conn { flex: 1 1 auto; min-width: 0; display: flex; flex-direction: column; }
.tree-row .conn-name {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tree-conn .conn-host {
  margin-top: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* Only folder rows need the name to push the item count to the right edge. */
.tree-row.is-folder .conn-name { flex: 1 1 auto; }
.tree-count {
  flex: 0 0 auto;
  font-size: 11px;
  color: #888;
  background: rgba(128, 128, 128, 0.16);
  border-radius: 9px;
  padding: 1px 6px;
}
/* Highlight a folder row while an item is dragged over it. */
.tree-row.drop-into {
  background: #dbeafe;
  outline: 2px solid #2563eb;
  outline-offset: -2px;
}
/* Insertion bars run horizontally here — the list is vertical, unlike the grid. */
.tree-row.drop-before,
.tree-row.drop-after {
  position: relative;
}
.tree-row.drop-before::after,
.tree-row.drop-after::after {
  content: "";
  position: absolute;
  left: 8px;
  right: 8px;
  height: 3px;
  border-radius: 2px;
  background: #2563eb;
}
.tree-row.drop-before::after { top: -2px; }
.tree-row.drop-after::after { bottom: -2px; }

.main {
  flex: 1;
  padding: 24px 32px;
  overflow-y: auto;
}
/* Leave room for the floating expand button when the sidebar is hidden. */
.main.shifted { padding-left: 56px; }

/* Breadcrumb bar above the card grid; each crumb is also a drop target. */
.crumbs {
  display: flex;
  align-items: center;
  gap: 2px;
  margin-bottom: 14px;
  min-height: 30px;
  flex-wrap: wrap;
}
.crumb {
  padding: 4px 9px;
  border: 1px solid transparent;
  border-radius: 6px;
  background: transparent;
  font-size: 13px;
  color: inherit;
  white-space: nowrap;
}
.crumb:hover { background: rgba(128, 128, 128, 0.16); }
.crumb.drop-into {
  background: #dbeafe;
  border-color: #2563eb;
  color: #1e40af;
}
.crumb-sep { color: #999; font-size: 13px; }
.crumbs-spacer { flex: 1; }
button.ghost {
  padding: 5px 10px;
  font-size: 12px;
  background: transparent;
  border-color: #d0d0d5;
}

/* Connection cards — click to connect (built-in IronRDP). */
.conn-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: 14px;
  align-content: start;
  /* Give the grid a floor so there is always empty space to drop onto. */
  min-height: 240px;
  border-radius: 10px;
  border: 2px dashed transparent;
  padding: 4px;
  margin: -4px;
}
/* Dragging over the grid background = move into the folder that's open. */
.conn-grid.drop-into { border-color: #2563eb; background: rgba(37, 99, 235, 0.05); }
.conn-card {
  position: relative;
  padding: 0;
  overflow: hidden;
  border: 1px solid #d8d8de;
  border-radius: 10px;
  background: #fff;
  text-align: left;
  cursor: pointer;
  /* Cards are drag handles; a text selection starting on one fights the drag. */
  user-select: none;
  transition: border-color 0.15s, box-shadow 0.15s, transform 0.05s;
}
.conn-card:hover { border-color: #b9c6e6; box-shadow: 0 2px 10px rgba(0, 0, 0, 0.07); }
.conn-card:active { transform: translateY(1px); }
.conn-card-thumb {
  aspect-ratio: 16 / 10;
  background: #0b0b0e;
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
}
.conn-card-thumb img { width: 100%; height: 100%; object-fit: cover; display: block; }
.conn-card-thumb-empty { font-size: 30px; opacity: 0.35; }
.conn-card-info {
  padding: 11px 13px;
  display: flex;
  align-items: baseline;
  gap: 8px;
}
.conn-card-name { font-weight: 600; font-size: 15px; flex: 0 0 auto; }
.conn-card-host {
  font-size: 12px;
  color: #666;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* Protocol badge pinned to the top-right corner, over the thumbnail. */
.conn-card-proto {
  position: absolute;
  top: 8px;
  right: 8px;
  z-index: 1;
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.04em;
  color: #fff;
  background: rgba(59, 91, 219, 0.92);
  border-radius: 4px;
  padding: 2px 6px;
  transition: opacity 0.12s ease;
}
.conn-card-proto.vnc { background: rgba(13, 148, 136, 0.92); }
.conn-card-proto.folder { background: rgba(180, 130, 20, 0.92); }

/* Folder cards sit in the same grid as connections and drag the same way. */
.folder-card .conn-card-thumb {
  background: linear-gradient(160deg, #fdf3dc 0%, #f6e4bb 100%);
}
.folder-glyph { font-size: 56px; line-height: 1; }

/* Drop indicators. `into` = drop inside this folder; `before`/`after` = an
   insertion bar on that side of the card. */
.conn-card.drop-into {
  border-color: #2563eb;
  box-shadow: 0 0 0 2px rgba(37, 99, 235, 0.45);
}
/* Drawn on the card's inner edge — the card clips its overflow, so a bar in
   the gutter between cards would be invisible. */
.conn-card.drop-before::before,
.conn-card.drop-after::before {
  content: "";
  position: absolute;
  top: 0;
  bottom: 0;
  width: 4px;
  background: #2563eb;
  z-index: 2;
}
.conn-card.drop-before::before { left: 0; }
.conn-card.drop-after::before { right: 0; }
/* Hover reveals the action buttons in the same corner; hide the badge then. */
.conn-card:hover .conn-card-proto { opacity: 0; }
.conn-card-actions {
  position: absolute;
  top: 8px;
  right: 8px;
  display: flex;
  gap: 2px;
  opacity: 0;
  transition: opacity 0.12s ease;
}
.conn-card:hover .conn-card-actions { opacity: 1; }
.conn-card-actions .icon-btn {
  font-size: 13px;
  padding: 3px 6px;
  background: rgba(255, 255, 255, 0.85);
}
.icon-btn.danger { color: #c81e1e; }

.conn-card.add {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 4px;
  min-height: 180px;
  padding: 20px;
  color: #6b7280;
  border-style: dashed;
}
.conn-card.add:hover { color: #2563eb; border-color: #2563eb; background: #f7f9ff; }
.conn-card.add .plus { font-size: 24px; line-height: 1; }

.form, .details {
  max-width: 480px;
}
.form h2, .details h2 { margin-top: 0; }
.form label {
  display: block;
  margin-bottom: 12px;
  font-size: 13px;
  color: #444;
}
.form input, .form select {
  display: block;
  margin-top: 4px;
  width: 100%;
  padding: 8px 10px;
  border: 1px solid #c5c5cb;
  border-radius: 6px;
  font-size: 14px;
  background: #fff;
}
.form input:focus, .form select:focus {
  outline: none;
  border-color: #3b82f6;
}
/* Checkbox rows read as a row, not as a stacked field like the inputs above. */
.form label.check {
  display: flex;
  align-items: flex-start;
  gap: 8px;
}
.form label.check input {
  display: inline;
  width: auto;
  margin-top: 2px;
}
.form label.check small {
  display: block;
  margin-top: 2px;
  color: #777;
  line-height: 1.4;
}

.details dl {
  display: grid;
  grid-template-columns: 110px 1fr;
  row-gap: 8px;
  margin: 16px 0 24px;
}
.details dt { color: #666; }
.details dd { margin: 0; }

.actions {
  display: flex;
  gap: 8px;
  margin-top: 16px;
}
button {
  padding: 8px 14px;
  font-size: 14px;
  border: 1px solid #c5c5cb;
  border-radius: 6px;
  background: #fff;
  cursor: pointer;
}
button:hover { background: #f0f0f3; }
button.primary {
  background: #2563eb;
  color: white;
  border-color: #2563eb;
}
button.primary:hover { background: #1d4ed8; }
button.danger {
  background: #fff;
  color: #c81e1e;
  border-color: #f5c2c2;
}
button.danger:hover { background: #fef2f2; }

.empty-state {
  color: #888;
  text-align: center;
  margin-top: 80px;
}

/* Drag ghost + the cursor/selection state that applies while dragging. */
.drag-ghost {
  position: fixed;
  z-index: 1000;
  pointer-events: none;
  padding: 5px 10px;
  border-radius: 6px;
  background: rgba(37, 99, 235, 0.95);
  color: #fff;
  font-size: 12px;
  font-weight: 600;
  box-shadow: 0 4px 14px rgba(0, 0, 0, 0.28);
  max-width: 220px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
body.dragging { cursor: grabbing; }
body.dragging * { cursor: grabbing !important; user-select: none; }

.status {
  position: fixed;
  bottom: 16px;
  right: 16px;
  background: #1a1a1a;
  color: white;
  padding: 8px 14px;
  border-radius: 6px;
  font-size: 13px;
}

.session {
  flex: 1;
  min-height: 0;
  position: relative;
  background: #000;
}
/* Thin invisible hover-catch strip at the top-centre of the session. */
.session-reveal {
  position: absolute;
  top: 0;
  left: 50%;
  transform: translateX(-50%);
  z-index: 10;
  width: 240px;
  height: 12px;
  display: flex;
  justify-content: center;
}
/* The controls drop in from above when the strip (or themselves) is hovered. */
/* The catch strip stays narrow, but the controls must not be squeezed to fit
   it — an absolutely positioned box would otherwise shrink-to-fit inside its
   240px containing block and ellipsise the status text. */
.session-controls {
  position: absolute;
  top: 0;
  left: 50%;
  width: max-content;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 8px;
  transform: translateX(-50%) translateY(-130%);
  opacity: 0;
  transition: transform 0.18s ease, opacity 0.18s ease;
}
.session-reveal:hover .session-controls {
  transform: translateX(-50%) translateY(0);
  opacity: 1;
}
/* Small "hover here" affordance; fades out once the controls drop in. */
.session-hint {
  position: absolute;
  top: 0;
  left: 50%;
  transform: translateX(-50%);
  padding: 1px 16px 3px;
  background: rgba(0, 0, 0, 0.35);
  color: rgba(255, 255, 255, 0.55);
  font-size: 14px;
  line-height: 1;
  border-bottom-left-radius: 8px;
  border-bottom-right-radius: 8px;
  pointer-events: none;
  transition: opacity 0.15s ease;
}
.session-reveal:hover .session-hint { opacity: 0; }
.session-status-mini {
  font-size: 12px;
  color: #e4e4e7;
  background: rgba(0, 0, 0, 0.55);
  padding: 4px 9px;
  border-radius: 6px;
  max-width: 40vw;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.session-disconnect {
  border: none;
  border-radius: 6px;
  padding: 5px 11px;
  font-size: 12px;
  color: #fff;
  background: rgba(200, 30, 30, 0.9);
  cursor: pointer;
}
.session-disconnect:hover { background: rgb(220, 40, 40); }
.session-fullscreen {
  border: none;
  border-radius: 6px;
  padding: 5px 11px;
  font-size: 12px;
  color: #fff;
  background: rgba(60, 60, 66, 0.9);
  cursor: pointer;
}
.session-fullscreen:hover { background: rgba(80, 80, 88, 0.95); }
/* Connecting / ended overlay over the session canvas. */
.session-overlay {
  position: absolute;
  inset: 0;
  z-index: 5;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(10, 10, 12, 0.82);
  backdrop-filter: blur(2px);
}
.session-overlay-card {
  width: min(420px, 88vw);
  padding: 26px 28px;
  border-radius: 12px;
  border: 1px solid #33333a;
  background: #1c1c20;
  color: #e4e4e7;
  text-align: center;
}
.session-overlay-icon { font-size: 30px; color: #fbbf24; line-height: 1; }
.session-overlay-name {
  margin-top: 12px;
  font-size: 17px;
  font-weight: 600;
}
.session-overlay-facts {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: 6px 14px;
  margin: 16px 0 0;
  text-align: left;
  font-size: 12.5px;
}
.session-overlay-facts dt { color: #8b8b93; }
.session-overlay-facts dd {
  margin: 0;
  display: flex;
  align-items: center;
  gap: 7px;
  /* Long hostnames wrap instead of stretching the card. */
  overflow-wrap: anywhere;
}
.session-overlay-client { color: #8b8b93; }
.session-overlay-status {
  margin-top: 18px;
  padding-top: 14px;
  border-top: 1px solid #2c2c33;
  font-size: 13px;
  color: #c7c7cf;
  min-height: 19px;
  overflow-wrap: anywhere;
}
.session-overlay-status.failed { color: #fca5a5; }
.session-overlay-actions {
  display: flex;
  justify-content: center;
  gap: 8px;
  margin-top: 18px;
}
.spinner {
  width: 28px;
  height: 28px;
  margin: 0 auto;
  border: 3px solid rgba(255, 255, 255, 0.16);
  border-top-color: #4d8bf5;
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
}
@keyframes spin { to { transform: rotate(360deg); } }
@media (prefers-reduced-motion: reduce) {
  .spinner { animation-duration: 2.4s; }
}

.session-canvas-wrap {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
}
.screen {
  max-width: 100%;
  max-height: 100%;
  outline: none;
  /* Preserve the true pixels of the remote framebuffer. */
  image-rendering: pixelated;
}

@media (prefers-color-scheme: dark) {
  :root { color: #e4e4e7; background: #18181b; }
  .sidebar { background: #1f1f23; border-right-color: #2a2a2e; }
  .sidebar-header { border-bottom-color: #2a2a2e; }
  .conn-list li { border-bottom-color: #27272a; }
  .conn-list li:hover { background: #27272a; }
  .conn-list li.active { background: #1e3a8a; }
  .conn-host { color: #a1a1aa; }
  .tree-row.active { background: #1e3a8a; }
  .tree-indent { border-left-color: rgba(255, 255, 255, 0.16); }
  .tree-empty { color: #71717a; }
  .tree-count { color: #a1a1aa; background: rgba(255, 255, 255, 0.1); }
  .tree-row.drop-into { background: #1e3a8a; outline-color: #60a5fa; }
  .tree-row.drop-before::after,
  .tree-row.drop-after::after { background: #60a5fa; }
  .crumb:hover { background: rgba(255, 255, 255, 0.12); }
  .crumb.drop-into { background: #1e3a8a; border-color: #60a5fa; color: #dbeafe; }
  .crumb-sep { color: #71717a; }
  button.ghost { background: transparent; border-color: #3f3f46; }
  .conn-grid.drop-into { border-color: #60a5fa; background: rgba(96, 165, 250, 0.08); }
  .folder-card .conn-card-thumb {
    background: linear-gradient(160deg, #2a2417 0%, #1f1b12 100%);
  }
  .conn-card.drop-into { border-color: #60a5fa; box-shadow: 0 0 0 2px rgba(96, 165, 250, 0.5); }
  .conn-card.drop-before::before,
  .conn-card.drop-after::before { background: #60a5fa; }
  .sidebar-expand {
    background: #27272a;
    border-color: #3f3f46;
    color: #e4e4e7;
  }
  .sidebar-expand:hover { background: #3f3f46; }
  .icon-btn:hover { background: rgba(255, 255, 255, 0.12); }
  .conn-card { background: #1f1f23; border-color: #2f2f35; }
  .conn-card:hover { border-color: #3b5bdb; box-shadow: 0 2px 12px rgba(0, 0, 0, 0.5); }
  .conn-card-host { color: #a1a1aa; }
  /* Keep the solid colored badge in dark mode (white text reads on both). */
  .conn-card-actions .icon-btn { background: rgba(31, 31, 35, 0.85); }
  .conn-card.add { color: #a1a1aa; }
  .conn-card.add:hover { color: #93b0ff; border-color: #3b5bdb; background: #20232e; }
  .form input, .form select {
    background: #27272a;
    border-color: #3f3f46;
    color: #e4e4e7;
  }
  button { background: #27272a; color: #e4e4e7; border-color: #3f3f46; }
  button:hover { background: #3f3f46; }
  button.danger { background: #27272a; color: #f87171; border-color: #7f1d1d; }
}
</style>
