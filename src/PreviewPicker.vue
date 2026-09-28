<script setup lang="ts">
// The card-preview controls shared by the folder and connection editors:
// Default / Color / Image, the colour swatches, and the image chooser. The
// sample card is the caller's (slot `sample`), since folders and connections
// draw different cards; slot `top` sits above the controls for extras.
import { ref } from "vue";
import { SWATCHES, PREVIEW_MAX_W, PREVIEW_MAX_H, downscaleImage, type PreviewSettings } from "./preview";

const model = defineModel<PreviewSettings>({ required: true });
const imageError = ref("");

async function onFile(e: Event) {
  const input = e.target as HTMLInputElement;
  const file = input.files?.[0];
  input.value = ""; // so picking the same file again still fires `change`
  if (!file) return;
  imageError.value = "";
  try {
    const { data, fit } = await downscaleImage(file);
    model.value.image = data;
    model.value.mode = "image";
    model.value.fit = fit;
  } catch (err) {
    imageError.value = (err as Error).message;
  }
}
</script>

<template>
  <div class="field">
    <span class="field-label">Preview</span>
    <div class="preview-editor">
      <slot name="sample" />

      <div class="preview-controls">
        <slot name="top" />
        <div class="segmented" role="radiogroup" aria-label="Preview style">
          <button
            v-for="m in (['default', 'color', 'image'] as const)"
            :key="m"
            role="radio"
            :aria-checked="model.mode === m"
            :class="{ on: model.mode === m }"
            @click="model.mode = m"
          >
            {{ m === "default" ? "Default" : m === "color" ? "Color" : "Image" }}
          </button>
        </div>

        <template v-if="model.mode !== 'default'">
          <span class="sub-label">{{ model.mode === "image" ? "Background" : "Color" }}</span>
          <div class="swatches">
            <button
              v-for="c in SWATCHES"
              :key="c"
              class="swatch"
              :class="{ on: model.color.toLowerCase() === c }"
              :style="{ background: c }"
              :title="c"
              :aria-label="`Use ${c}`"
              @click="model.color = c"
            ></button>
            <label class="swatch custom" title="Pick any color">
              <input v-model="model.color" type="color" aria-label="Custom color" />
            </label>
          </div>
        </template>

        <template v-if="model.mode === 'image'">
          <span class="sub-label">Image</span>
          <div class="image-row">
            <label class="file-btn">
              {{ model.image ? "Replace image…" : "Choose image…" }}
              <input type="file" accept="image/png,image/jpeg,image/webp,image/gif,image/svg+xml" @change="onFile" />
            </label>
            <button v-if="model.image" class="ghost" @click="model.image = null">Remove</button>
          </div>
          <div v-if="model.image" class="segmented small" role="radiogroup" aria-label="Image fit">
            <button
              role="radio"
              :aria-checked="model.fit === 'cover'"
              :class="{ on: model.fit === 'cover' }"
              @click="model.fit = 'cover'"
            >
              Fill card
            </button>
            <button
              role="radio"
              :aria-checked="model.fit === 'contain'"
              :class="{ on: model.fit === 'contain' }"
              @click="model.fit = 'contain'"
            >
              Fit inside
            </button>
          </div>
          <small class="hint">
            Recommended: <b>{{ PREVIEW_MAX_W }} × {{ PREVIEW_MAX_H }} px</b> (16:10) — sharp at every card size.
            For <i>Fit inside</i>, a square logo or icon (e.g. 512 × 512, transparent PNG) works well.
            PNG, JPG, WebP, GIF or SVG; larger images are scaled down.
          </small>
          <small v-if="imageError" class="hint error">{{ imageError }}</small>
        </template>
      </div>
    </div>
  </div>
</template>
