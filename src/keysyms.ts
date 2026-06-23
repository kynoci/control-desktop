// Maps a browser keyboard event to an X11 keysym, the form VNC (RFB `KeyEvent`)
// expects — the VNC counterpart of `scancodes.ts`. See plan-vnc.md §6.
//
// Strategy: printable characters resolve straight from `KeyboardEvent.key`
// (so Shift+2 correctly sends `@`), and everything else (named keys, modifiers,
// the numeric keypad in nav mode) is looked up by `KeyboardEvent.code`.

// Non-character keys, keyed by `KeyboardEvent.code`. Values are X11 keysyms
// (see X11/keysymdef.h — the 0xFFxx "function key" block).
export const KEYSYMS: Record<string, number> = {
  Enter: 0xff0d,
  NumpadEnter: 0xff8d,
  Backspace: 0xff08,
  Tab: 0xff09,
  Escape: 0xff1b,
  Delete: 0xffff,
  Insert: 0xff63,

  Home: 0xff50,
  End: 0xff57,
  PageUp: 0xff55,
  PageDown: 0xff56,
  ArrowLeft: 0xff51,
  ArrowUp: 0xff52,
  ArrowRight: 0xff53,
  ArrowDown: 0xff54,

  ShiftLeft: 0xffe1,
  ShiftRight: 0xffe2,
  ControlLeft: 0xffe3,
  ControlRight: 0xffe4,
  AltLeft: 0xffe9,
  AltRight: 0xffea, // AltGr on many layouts
  MetaLeft: 0xffeb, // Super_L
  MetaRight: 0xffec, // Super_R
  CapsLock: 0xffe5,
  NumLock: 0xff7f,
  ScrollLock: 0xff14,
  ContextMenu: 0xff67, // Menu
  PrintScreen: 0xff61,
  Pause: 0xff13,

  F1: 0xffbe,
  F2: 0xffbf,
  F3: 0xffc0,
  F4: 0xffc1,
  F5: 0xffc2,
  F6: 0xffc3,
  F7: 0xffc4,
  F8: 0xffc5,
  F9: 0xffc6,
  F10: 0xffc7,
  F11: 0xffc8,
  F12: 0xffc9,

  // Numeric keypad with Num Lock off (when on, `e.key` is a digit and the
  // printable path handles it). KP_* keysyms.
  Numpad0: 0xffb0,
  Numpad1: 0xffb1,
  Numpad2: 0xffb2,
  Numpad3: 0xffb3,
  Numpad4: 0xffb4,
  Numpad5: 0xffb5,
  Numpad6: 0xffb6,
  Numpad7: 0xffb7,
  Numpad8: 0xffb8,
  Numpad9: 0xffb9,
  NumpadDivide: 0xffaf,
  NumpadMultiply: 0xffaa,
  NumpadSubtract: 0xffad,
  NumpadAdd: 0xffab,
  NumpadDecimal: 0xffae,
};

// Resolve a keyboard event to an X11 keysym, or `undefined` if we can't map it.
export function keysymFor(e: KeyboardEvent): number | undefined {
  // Single printable character: the keysym is the codepoint for Latin-1, and
  // the Unicode keysym (0x01000000 + codepoint) above that. This naturally
  // covers letters, digits, and shifted symbols via `e.key`.
  if (e.key.length === 1) {
    const cp = e.key.codePointAt(0)!;
    return cp <= 0xff ? cp : 0x01000000 + cp;
  }
  return KEYSYMS[e.code];
}
