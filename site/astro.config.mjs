// @ts-check
import process from 'node:process';
import { defineConfig, fontProviders } from 'astro/config';

// SITE_URL and BASE_PATH say where the page is served from, so one build
// serves a domain's root or a project path such as GitHub Pages'
// /pingpong/. With SITE_URL unset the page has no canonical URL.
export default defineConfig({
  site: process.env.SITE_URL || undefined,
  base: process.env.BASE_PATH || '/',
  // HTML's whitespace rules rather than JSX's (Astro's default), so prose
  // can wrap in the source without losing the space before a link.
  compressHTML: true,
  // Geist from its Fontsource packages (the Latin files, every weight),
  // preloaded, behind a fallback sized to its metrics: the text is laid out
  // once, and does not move when the font arrives.
  fonts: [
    {
      provider: fontProviders.local(),
      name: 'Geist',
      cssVariable: '--font-geist',
      weights: ['100 900'],
      fallbacks: ['system-ui', 'sans-serif'],
      options: {
        variants: [
          {
            src: ['./node_modules/@fontsource-variable/geist/files/geist-latin-wght-normal.woff2'],
            weight: '100 900',
            style: 'normal',
          },
        ],
      },
    },
    {
      provider: fontProviders.local(),
      name: 'Geist Mono',
      cssVariable: '--font-geist-mono',
      weights: ['100 900'],
      fallbacks: ['ui-monospace', 'monospace'],
      options: {
        variants: [
          {
            src: ['./node_modules/@fontsource-variable/geist-mono/files/geist-mono-latin-wght-normal.woff2'],
            weight: '100 900',
            style: 'normal',
          },
        ],
      },
    },
  ],
  vite: {
    server: {
      // The page reads the repository rather than copies of it: the
      // screenshots in docs/images/, the UI's icons in
      // pingpong-ui/assets/icons/ and the release in Casks/ping.rb.
      fs: { allow: ['..'] },
    },
  },
});
