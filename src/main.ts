import { createApp } from "vue";
import App from "./App.vue";

// Suppress the WebView's browser context menu (Back / Refresh / Print / Inspect),
// which makes the app feel like a web page. Text fields keep it for copy/paste;
// DevTools stays reachable via F12.
document.addEventListener("contextmenu", (e) => {
  const t = e.target as HTMLElement | null;
  if (t?.closest("input, textarea, [contenteditable='true']")) return;
  e.preventDefault();
});

createApp(App).mount("#app");
