import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

// The design system is a stylesheet artifact, not a JS app.
// The Rust (Axum + Askama + HTMX) server embeds the generated CSS at GET /assets/styles.css,
// so the build must emit ONE self-contained, readable, unlayered-import CSS file.
export default defineConfig({
  output: 'static',
  // Deterministic, inspectable artifact: readable CSS, one chunk, no inlined <style> tags.
  vite: {
    plugins: [tailwindcss()],
    styles: {
      // Keep stylesheets as a real .css asset instead of inlining them into each page.
      inlineStylesheet: false,
    },
    build: {
      // One shared stylesheet for every page (the Go app ships a single styles.css).
      cssCodeSplit: false,
      // Keep the committed artifact human-readable.
      cssMinify: false,
      // Do not base64-inline assets into CSS.
      assetsInlineLimit: 0,
    },
  },
});
