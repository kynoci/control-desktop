<script setup lang="ts">
import { ref, onMounted, nextTick, watch } from "vue";
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
async function refreshMaximized() {
  isMaximized.value = await appWindow.isMaximized();
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
  connections.value = await db.value.select<Connection[]>(
    "SELECT id, name, host, port, username, protocol, created_at FROM connections ORDER BY name",
  );
  loadThumbnails();
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

function newConnection() {
  draft.value = blankDraft();
  editing.value = true;
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
  connId: number; // which saved connection this session belongs to
  protocol: string; // "rdp" | "vnc" — routes input/disconnect/resize commands
  lastThumb: number; // timestamp of the last thumbnail capture
};
const runtimes = new Map<number, Runtime>();

function setCanvas(id: number, el: Element | null) {
  const rt = runtimes.get(id);
  if (rt) rt.canvas = (el as HTMLCanvasElement) ?? null;
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
    rt.lastThumb = Date.now();
  } catch {
    /* canvas not ready / storage full — skip this capture */
  }
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
    // Refresh the connection thumbnail at most every few seconds.
    if (Date.now() - rt.lastThumb > 4000) captureThumbnail(id);
  } else if (tag === 3) {
    // rect: [3][x u16][y u16][w u16][h u16][rgba...] — VNC dirty rectangle.
    if (!rt.ctx) return;
    const x = view.getUint16(1, true);
    const y = view.getUint16(3, true);
    const w = view.getUint16(5, true);
    const h = view.getUint16(7, true);
    const rgba = new Uint8ClampedArray(data.slice(9));
    if (rgba.length === w * h * 4) {
      rt.ctx.putImageData(new ImageData(rgba, w, h), x, y);
    }
    if (Date.now() - rt.lastThumb > 4000) captureThumbnail(id);
  } else if (tag === 2) {
    // status: [2][utf-8 message] — a disconnect/close/error notice.
    setStatus(id, new TextDecoder().decode(new Uint8Array(data, 1)));
  } else if (tag === 3) {
    // clipboard: [3][utf-8 text] — remote copied text; mirror to local clipboard.
    const text = new TextDecoder().decode(new Uint8Array(data, 1));
    navigator.clipboard.writeText(text).catch(() => {
      /* clipboard write blocked (no permission / not focused) — ignore */
    });
  } else if (tag === 4) {
    console.log(`[audio] tag 4 received, bytes=${data.byteLength}`);
    // audio: [4][pcm] — remote sound, interleaved 16-bit PCM in the negotiated format.
    playAudio(id, data);
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
    const ctx = rt.audioCtx;
    console.log(`[audio] unlocked, state=${ctx?.state}`);

    // Test local WebAudio output.
    // If you hear this short beep, frontend/Tauri audio output is OK.
    if (ctx) {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      gain.gain.value = 0.4; // loud and unmistakable
      osc.frequency.value = 660;
      osc.connect(gain);
      gain.connect(ctx.destination);
      osc.start();
      osc.stop(ctx.currentTime + 1.2); // long so it can't be missed
      console.log(`[audio] LOUD test beep playing for 1.2s, destination=${ctx.destination.maxChannelCount}ch`);
    }
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
  
  sessions.value.push({ id, name: c.name, status: `Connecting to ${c.name}...` });
  activeTab.value = id;
  await nextTick();

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
  draft.value = blankDraft();
  status.value = "";
}

onMounted(async () => {
  // Global key capture; handlers no-op unless a session tab is active.
  window.addEventListener("keydown", keyDown, true);
  window.addEventListener("keyup", keyUp, true);
  window.addEventListener("resize", onWindowResize);
  // When the app regains focus (e.g. after copying in another app), prime the
  // active session's remote clipboard with the local clipboard contents.
  window.addEventListener("focus", () => {
    if (typeof activeTab.value === "number") offerLocalClipboard(activeTab.value);
  });
  // Keep the maximize/restore button in sync with the actual window state.
  refreshMaximized();
  appWindow.onResized(() => refreshMaximized());
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
        <button class="primary" @click="newConnection">+ New</button>
      </div>
      <ul class="conn-list">
        <li
          v-for="c in connections"
          :key="c.id"
          :title="`Connect to ${c.name}`"
          @click="connectBuiltin(c)"
        >
          <div class="conn-name">{{ c.name }}</div>
          <div class="conn-host">{{ c.username }}@{{ c.host }}:{{ c.port }}</div>
        </li>
        <li v-if="connections.length === 0" class="empty">
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
        <div class="actions">
          <button class="primary" @click="saveDraft">Save</button>
          <button @click="cancelEdit">Cancel</button>
        </div>
      </section>

      <div v-else class="conn-grid">
        <div
          v-for="c in connections"
          :key="c.id"
          class="conn-card"
          :title="`Connect to ${c.name}`"
          @click="connectBuiltin(c)"
        >
          <div class="conn-card-actions">
            <button v-if="c.protocol === 'rdp'" class="icon-btn" title="Open with Windows RDP" @click.stop="connectMstsc(c)">🖥</button>
            <button class="icon-btn" title="Edit" @click.stop="editConn(c)">✎</button>
            <button class="icon-btn danger" title="Delete" @click.stop="deleteConn(c)">🗑</button>
          </div>
          <div class="conn-card-thumb">
            <img v-if="thumbnails[c.id]" :src="thumbnails[c.id]" alt="" />
            <span v-else class="conn-card-thumb-empty">🖥</span>
          </div>
          <div class="conn-card-info">
            <div class="conn-card-name">{{ c.name }}</div>
            <div class="conn-card-host">{{ c.username }}@{{ c.host }}:{{ c.port }}</div>
            <span class="conn-card-proto">{{ c.protocol.toUpperCase() }}</span>
          </div>
        </div>

        <button class="conn-card add" @click="newConnection">
          <span class="plus">＋</span>
          <span>New connection</span>
        </button>
      </div>

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
  align-items: stretch;
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

.main {
  flex: 1;
  padding: 24px 32px;
  overflow-y: auto;
}
/* Leave room for the floating expand button when the sidebar is hidden. */
.main.shifted { padding-left: 56px; }

/* Connection cards — click to connect (built-in IronRDP). */
.conn-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: 14px;
  align-content: start;
}
.conn-card {
  position: relative;
  padding: 0;
  overflow: hidden;
  border: 1px solid #d8d8de;
  border-radius: 10px;
  background: #fff;
  text-align: left;
  cursor: pointer;
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
.conn-card-info { padding: 11px 13px; }
.conn-card-name { font-weight: 600; font-size: 15px; }
.conn-card-host { font-size: 12px; color: #666; margin-top: 3px; word-break: break-all; }
.conn-card-proto {
  display: inline-block;
  margin-top: 10px;
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.04em;
  color: #3b5bdb;
  background: #eaf0ff;
  border-radius: 4px;
  padding: 2px 6px;
}
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
  .conn-card-proto { color: #9db4ff; background: #1e2a55; }
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
