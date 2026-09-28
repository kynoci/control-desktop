// Card previews shared by folders and connections: the editor's settings, how
// they map to the DB columns, the card background they produce, and the image
// downscaling done before anything is stored.

export type PreviewMode = "default" | "color" | "image";
export type PreviewFit = "cover" | "contain";

/** What the preview editor edits. `color` is kept even in Default mode so
 *  switching styles back and forth doesn't lose the user's pick. */
export type PreviewSettings = {
  mode: PreviewMode;
  color: string;
  image: string | null;
  fit: PreviewFit;
};

/** The stored form: the `preview_*` columns. All null = the default card. */
export type PreviewRow = {
  color: string | null;
  image: string | null;
  fit: PreviewFit | null;
};

// Largest stored image. The card thumbnail is always 16:10 and tops out at
// ~420x262 CSS px (Extra large); this is that at 2x, so it stays sharp on a
// high-DPI screen while each row's data URL stays small.
export const PREVIEW_MAX_W = 840;
export const PREVIEW_MAX_H = 525;

// Quick picks for the colour preview; the colour input covers everything else.
export const SWATCHES = [
  "#f6e4bb", "#fecaca", "#fed7aa", "#bbf7d0", "#99f6e4",
  "#bae6fd", "#c7d2fe", "#e9d5ff", "#fbcfe8", "#e5e7eb",
  "#334155", "#1e3a8a",
];

export function settingsFromRow(row: PreviewRow, defaultColor: string): PreviewSettings {
  return {
    mode: row.image ? "image" : row.color ? "color" : "default",
    color: row.color ?? defaultColor,
    image: row.image,
    fit: row.fit ?? "cover",
  };
}

/** The row to store. In Image mode the colour is only kept when the user
 *  actually picked one, so an untouched background stays the default. */
export function rowFromSettings(s: PreviewSettings, defaultColor: string): PreviewRow {
  if (s.mode === "default") return { color: null, image: null, fit: null };
  if (s.mode === "color") return { color: s.color, image: null, fit: null };
  return {
    color: s.color.toLowerCase() !== defaultColor.toLowerCase() ? s.color : null,
    image: s.image,
    fit: s.image ? s.fit : null,
  };
}

/** Thumbnail background for a stored preview. `fallback` is the default card's
 *  background, used when nothing is set and behind an image with no colour. A
 *  soft white sheen over a colour keeps the default cards' feel. */
export function thumbStyle(row: PreviewRow, fallback?: string) {
  const layers: string[] = [];
  if (row.image) layers.push(`url("${row.image}") center / ${row.fit === "contain" ? "contain" : "cover"} no-repeat`);
  if (row.color) layers.push(`linear-gradient(160deg, rgba(255,255,255,0.35), rgba(255,255,255,0) 70%), ${row.color}`);
  else if (fallback) layers.push(fallback);
  return layers.length ? { background: layers.join(", ") } : undefined;
}

/** Read an image file and downscale it to fit PREVIEW_MAX_W x PREVIEW_MAX_H.
 *  Also suggests a fit: photos fill the card, logos/icons sit whole in it. */
export function downscaleImage(file: File): Promise<{ data: string; fit: PreviewFit }> {
  return new Promise((resolve, reject) => {
    if (!file.type.startsWith("image/")) {
      reject(new Error("That file isn't an image."));
      return;
    }
    const url = URL.createObjectURL(file);
    const img = new Image();
    img.onload = () => {
      URL.revokeObjectURL(url);
      const scale = Math.min(1, PREVIEW_MAX_W / img.naturalWidth, PREVIEW_MAX_H / img.naturalHeight);
      const w = Math.max(1, Math.round(img.naturalWidth * scale));
      const h = Math.max(1, Math.round(img.naturalHeight * scale));
      const canvas = document.createElement("canvas");
      canvas.width = w;
      canvas.height = h;
      canvas.getContext("2d")!.drawImage(img, 0, 0, w, h);
      // JPEG only for photos; anything that may carry transparency stays PNG.
      const photo = file.type === "image/jpeg";
      resolve({
        data: photo ? canvas.toDataURL("image/jpeg", 0.85) : canvas.toDataURL("image/png"),
        fit: photo ? "cover" : "contain",
      });
    };
    img.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error("Couldn't read that image."));
    };
    img.src = url;
  });
}

// --- Default connection previews: one icon + tint per host OS ---------------
// Simple original glyphs (not vendor logos), drawn on a 64x64 grid.

export const OS_LOOK: Record<string, { svg: string; bg: string }> = {
  windows: {
    bg: "linear-gradient(160deg, #e3f1ff 0%, #bcdcf7 100%)",
    svg: `<svg viewBox="0 0 64 64" aria-hidden="true"><g fill="#0a6cc2">
      <rect x="8" y="8" width="22" height="22" rx="2"/><rect x="34" y="8" width="22" height="22" rx="2"/>
      <rect x="8" y="34" width="22" height="22" rx="2"/><rect x="34" y="34" width="22" height="22" rx="2"/></g></svg>`,
  },
  linux: {
    bg: "linear-gradient(160deg, #fff7db 0%, #fde6a6 100%)",
    svg: `<svg viewBox="0 0 64 64" aria-hidden="true">
      <ellipse cx="32" cy="37" rx="17" ry="21" fill="#1f2937"/>
      <circle cx="32" cy="19" r="12" fill="#1f2937"/>
      <ellipse cx="32" cy="41" rx="11" ry="15" fill="#f8fafc"/>
      <ellipse cx="27" cy="17" rx="3" ry="3.6" fill="#fff"/><ellipse cx="37" cy="17" rx="3" ry="3.6" fill="#fff"/>
      <circle cx="27.6" cy="17.8" r="1.6" fill="#111827"/><circle cx="36.4" cy="17.8" r="1.6" fill="#111827"/>
      <path d="M27 23h10l-5 5z" fill="#f59e0b"/>
      <ellipse cx="23" cy="58" rx="8" ry="3.5" fill="#f59e0b"/><ellipse cx="41" cy="58" rx="8" ry="3.5" fill="#f59e0b"/></svg>`,
  },
  macos: {
    bg: "linear-gradient(160deg, #f6f6f7 0%, #dcdce1 100%)",
    svg: `<svg viewBox="0 0 64 64" aria-hidden="true" fill="none" stroke="#3f3f46" stroke-width="4.5">
      <path d="M24 24v-6a6 6 0 1 0-6 6h28a6 6 0 1 0-6-6v28a6 6 0 1 0 6-6H18a6 6 0 1 0 6 6z" stroke-linejoin="round"/></svg>`,
  },
  bsd: {
    bg: "linear-gradient(160deg, #ffe8e8 0%, #fbc4c4 100%)",
    svg: `<svg viewBox="0 0 64 64" aria-hidden="true">
      <path d="M17 24C12 15 13 8 18 4c0 7 3 12 8 15z" fill="#991b1b"/>
      <path d="M47 24c5-9 4-16-1-20 0 7-3 12-8 15z" fill="#991b1b"/>
      <circle cx="32" cy="37" r="20" fill="#dc2626"/>
      <ellipse cx="25" cy="29" rx="7" ry="4.5" fill="#fff" opacity=".35"/></svg>`,
  },
  other: {
    bg: "linear-gradient(160deg, #eef2f7 0%, #d3dbe6 100%)",
    svg: `<svg viewBox="0 0 64 64" aria-hidden="true" fill="none" stroke="#475569" stroke-width="4" stroke-linecap="round">
      <rect x="8" y="10" width="48" height="32" rx="3"/><path d="M32 42v10M22 54h20"/></svg>`,
  },
};

export function osLook(os: string | null | undefined) {
  return OS_LOOK[os ?? "windows"] ?? OS_LOOK.other;
}
