<script setup lang="ts">
import { ref, onMounted, computed, nextTick } from "vue";
import { invoke, Channel } from "@tauri-apps/api/core";
import Database from "@tauri-apps/plugin-sql";
import { SCANCODES } from "./scancodes";

type Connection = {
  id: number;
  name: string;
  host: string;
  port: number;
  username: string;
  protocol: string;
  created_at: number;
};

type Draft = {
  id: number | null;
  name: string;
  host: string;
  port: number;
  username: string;
  protocol: string;
  password: string;
};

const blankDraft = (): Draft => ({
  id: null,
  name: "",
  host: "",
  port: 3389,
  username: "",
  protocol: "rdp",
  password: "",
});

const db = ref<Database | null>(null);
const connections = ref<Connection[]>([]);
const selectedId = ref<number | null>(null);
const draft = ref<Draft>(blankDraft());
const editing = ref(false);
const status = ref<string>("");

const selected = computed(() =>
  connections.value.find((c) => c.id === selectedId.value) ?? null,
);

async function loadAll() {
  if (!db.value) return;
  connections.value = await db.value.select<Connection[]>(
    "SELECT id, name, host, port, username, protocol, created_at FROM connections ORDER BY name",
  );
}

function newConnection() {
  draft.value = blankDraft();
  selectedId.value = null;
  editing.value = true;
}

function editSelected() {
  if (!selected.value) return;
  draft.value = {
    id: selected.value.id,
    name: selected.value.name,
    host: selected.value.host,
    port: selected.value.port,
    username: selected.value.username,
    protocol: selected.value.protocol,
    password: "",
  };
  editing.value = true;
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
        "INSERT INTO connections (name, host, port, username, protocol) VALUES ($1, $2, $3, $4, $5)",
        [d.name, d.host, d.port, d.username, d.protocol],
      );
      id = Number(res.lastInsertId);
    } else {
      id = d.id;
      await db.value.execute(
        "UPDATE connections SET name=$1, host=$2, port=$3, username=$4, protocol=$5 WHERE id=$6",
        [d.name, d.host, d.port, d.username, d.protocol, id],
      );
    }
    if (d.password) {
      await invoke("save_password", { connectionId: id, password: d.password });
    }
    await loadAll();
    selectedId.value = id;
    editing.value = false;
    status.value = "Saved";
  } catch (e) {
    status.value = `Save failed: ${e}`;
  }
}

async function deleteSelected() {
  if (!db.value || !selected.value) return;
  const id = selected.value.id;
  if (!confirm(`Delete "${selected.value.name}"?`)) return;
  try {
    await db.value.execute("DELETE FROM connections WHERE id=$1", [id]);
    await invoke("delete_password", { connectionId: id });
    selectedId.value = null;
    await loadAll();
    status.value = "Deleted";
  } catch (e) {
    status.value = `Delete failed: ${e}`;
  }
}

// Native Windows fallback: hand off to mstsc.exe in its own window.
async function connectMstsc() {
  if (!selected.value) return;
  const c = selected.value;
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

// Reactive metadata shown in the tab strip; one entry per open session.
type SessionMeta = { id: number; name: string; status: string };
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
};
const runtimes = new Map<number, Runtime>();

function setCanvas(id: number, el: Element | null) {
  const rt = runtimes.get(id);
  if (rt) rt.canvas = (el as HTMLCanvasElement) ?? null;
}

function setStatus(id: number, text: string) {
  const s = sessions.value.find((s) => s.id === id);
  if (s) s.status = text;
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
    setStatus(id, "");
  } else if (tag === 1) {
    // strip: [1][y u16][height u16][rgba...]
    if (!rt.ctx) return;
    const y = view.getUint16(1, true);
    const h = view.getUint16(3, true);
    const rgba = new Uint8ClampedArray(data.slice(5));
    if (rgba.length === rt.width * h * 4) {
      rt.ctx.putImageData(new ImageData(rgba, rt.width, h), 0, y);
    }
  } else if (tag === 2) {
    // status: [2][utf-8 message] — a disconnect/close/error notice.
    setStatus(id, new TextDecoder().decode(new Uint8Array(data, 1)));
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
  invoke("rdp_input", { sessionId: id, event }).catch(() => {});
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
function onKey(e: KeyboardEvent, pressed: boolean) {
  if (typeof activeTab.value !== "number") return;
  const scancode = SCANCODES[e.code];
  if (scancode === undefined) return;
  e.preventDefault();
  sendInput(activeTab.value, { kind: "key", scancode, pressed });
}

const keyDown = (e: KeyboardEvent) => onKey(e, true);
const keyUp = (e: KeyboardEvent) => onKey(e, false);

// Desktop size to request: the available session area, clamped to RDP's limits
// (200–8192) with an even width, as the protocol requires.
function currentSize(): { width: number; height: number } {
  const even = (n: number) => Math.floor(n / 2) * 2;
  const clamp = (n: number) => Math.max(200, Math.min(8192, n));
  return {
    width: clamp(even(window.innerWidth)),
    height: clamp(even(window.innerHeight - 38)), // minus the tab bar (toolbar overlays)
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
    invoke("rdp_resize", { sessionId: id, width, height }).catch(() => {});
  }, 250);
}

async function connectBuiltin() {
  if (!selected.value) return;
  const c = selected.value;
  const id = nextSessionId++;
  runtimes.set(id, { canvas: null, ctx: null, width: 0, height: 0, lastX: -1, lastY: -1 });
  sessions.value.push({ id, name: c.name, status: `Connecting to ${c.name}...` });
  activeTab.value = id;
  await nextTick();

  // Request a desktop size matching the available window.
  const { width, height } = currentSize();

  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (data) => handleFrame(id, data);

  runtimes.get(id)?.canvas?.focus();

  try {
    await invoke("rdp_connect", {
      sessionId: id,
      req: {
        id: c.id,
        host: c.host,
        port: c.port,
        username: c.username,
        protocol: c.protocol,
        client: "ironrdp",
      },
      width,
      height,
      onFrame: channel,
    });
  } catch (e) {
    setStatus(id, `Connect failed: ${e}`);
  }
}

async function closeTab(id: number) {
  const idx = sessions.value.findIndex((s) => s.id === id);
  try {
    await invoke("rdp_disconnect", { sessionId: id });
  } catch {
    // ignore — session may already be gone
  }
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
  draft.value = blankDraft();
  status.value = "";
}

onMounted(async () => {
  // Global key capture; handlers no-op unless a session tab is active.
  window.addEventListener("keydown", keyDown, true);
  window.addEventListener("keyup", keyUp, true);
  window.addEventListener("resize", onWindowResize);
  try {
    db.value = await Database.load("sqlite:remote.db");
    await loadAll();
  } catch (e) {
    status.value = `DB load failed: ${e}`;
  }
});
</script>

<template>
  <div class="root">
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

  <div class="app" v-show="activeTab === 'home'">
    <aside class="sidebar">
      <div class="sidebar-header">
        <h2>Connections</h2>
        <button class="primary" @click="newConnection">+ New</button>
      </div>
      <ul class="conn-list">
        <li
          v-for="c in connections"
          :key="c.id"
          :class="{ active: c.id === selectedId }"
          @click="selectedId = c.id; editing = false"
        >
          <div class="conn-name">{{ c.name }}</div>
          <div class="conn-host">{{ c.username }}@{{ c.host }}:{{ c.port }}</div>
        </li>
        <li v-if="connections.length === 0" class="empty">
          No saved connections yet
        </li>
      </ul>
    </aside>

    <main class="main">
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
          </select>
        </label>
        <div class="actions">
          <button class="primary" @click="saveDraft">Save</button>
          <button @click="cancelEdit">Cancel</button>
        </div>
      </section>

      <section v-else-if="selected" class="details">
        <h2>{{ selected.name }}</h2>
        <dl>
          <dt>Host</dt><dd>{{ selected.host }}:{{ selected.port }}</dd>
          <dt>Username</dt><dd>{{ selected.username }}</dd>
          <dt>Protocol</dt><dd>{{ selected.protocol.toUpperCase() }}</dd>
        </dl>
        <div class="actions">
          <button class="primary" @click="connectBuiltin">Connect (Built-in)</button>
          <button @click="connectMstsc">Connect (Windows RDP)</button>
          <button @click="editSelected">Edit</button>
          <button class="danger" @click="deleteSelected">Delete</button>
        </div>
      </section>

      <section v-else class="empty-state">
        <p>Select a connection on the left, or click <strong>+ New</strong> to add one.</p>
      </section>

      <div v-if="status" class="status">{{ status }}</div>
    </main>
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
          <span v-if="s.status" class="session-status-mini">{{ s.status }}</span>
          <button class="session-disconnect" title="Disconnect" @click="closeTab(s.id)">
            ⏻ Disconnect
          </button>
        </div>
      </div>
      <div class="session-canvas-wrap">
        <canvas
          :ref="(el) => setCanvas(s.id, el as Element | null)"
          tabindex="0"
          class="screen"
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

.tabs {
  display: flex;
  align-items: stretch;
  flex: 0 0 38px;
  background: #1f1f23;
  border-bottom: 1px solid #2a2a2e;
  /* Scroll horizontally only; never show a vertical scrollbar. */
  overflow-x: auto;
  overflow-y: hidden;
  /* Always sits above the session panes. */
  position: relative;
  z-index: 100;
}
.tabs::-webkit-scrollbar { height: 6px; }
.tabs::-webkit-scrollbar-thumb { background: #3f3f46; border-radius: 3px; }
.tabs::-webkit-scrollbar-track { background: transparent; }
.tab {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 14px;
  height: 38px;
  border: none;
  border-right: 1px solid #2a2a2e;
  border-radius: 0;
  background: transparent;
  color: #c5c5cb;
  font-size: 13px;
  white-space: nowrap;
  cursor: pointer;
}
.tab:hover { background: #27272a; }
.tab.active { background: #f5f5f7; color: #1a1a1a; }
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
}

.sidebar {
  width: 280px;
  border-right: 1px solid #d0d0d5;
  background: #fafafa;
  display: flex;
  flex-direction: column;
}
.sidebar-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 14px;
  border-bottom: 1px solid #d0d0d5;
}
.sidebar-header h2 { margin: 0; font-size: 15px; }

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

.main {
  flex: 1;
  padding: 24px 32px;
  overflow-y: auto;
}

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
.session-controls {
  position: absolute;
  top: 0;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 8px;
  transform: translateY(-130%);
  opacity: 0;
  transition: transform 0.18s ease, opacity 0.18s ease;
}
.session-reveal:hover .session-controls {
  transform: translateY(0);
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
