import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.dirname(fileURLToPath(import.meta.url));
await build({
  absWorkingDir: root,
  entryPoints: ['gallery.tsx'],
  bundle: true,
  outfile: 'gallery.js',
  jsx: 'automatic',
  alias: { 'thinking-orbs': path.join(root, 'preview-orb.tsx') },
  minify: true,
  define: { 'process.env.NODE_ENV': '"production"' },
  legalComments: 'linked',
});
console.log('Built isolated gallery with the original ThinkingOrb source.');
