import { defineConfig } from 'tsdown'

// Output: lib/index.js (ESM). The dsh kernel packages and ws are all excluded from bundling;
// the dsh installation supplies kernel peer packages through the $DSH_HOME/profiles/node_modules fallback directory.
// Inlining these packages would create a second cordis instance in the process.
export default defineConfig({
  entry: ['src/index.ts'],
  outDir: 'lib',
  format: ['esm'],
  platform: 'node',
  target: 'es2024',
  dts: false,
  clean: false,
  // package.json declares type: module; use .js rather than tsdown's default .mjs.
  fixedExtension: false,
  deps: {
    neverBundle: [/^@deepseek-ai\//, 'ws'],
  },
})
