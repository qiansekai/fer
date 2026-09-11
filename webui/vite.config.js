import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import { viteSingleFile } from 'vite-plugin-singlefile'

// Everything is inlined into one self-contained index.html so the Rust side can
// embed it with a single `include_str!` and `fer serve` stays a lone binary with
// no asset directory to ship alongside it.
export default defineConfig({
  plugins: [vue(), viteSingleFile()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2020',
    // The whole bundle lands inline in the HTML; keep it readable in the binary.
    minify: 'esbuild',
    cssCodeSplit: false,
  },
  server: {
    // `npm run dev` talks to a locally running `fer serve` on the standard port.
    proxy: {
      '/api': 'http://127.0.0.1:19876',
    },
  },
})
