import { createRequire } from 'node:module';
import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

// Astro 7 / Vite 8 externalise `cookie` into the prerender entry instead of
// bundling it. Under pnpm's strict layout the app has no `cookie` link of its
// own, so Node resolves whatever happens to sit above the workspace. Point the
// specifier at the copy Astro itself uses so the prerender step is deterministic.
const require = createRequire(import.meta.url);
const astroRequire = createRequire(require.resolve('astro/package.json'));
const cookieEntry = astroRequire.resolve('cookie');

export default defineConfig({
  // Absolute URLs for canonical and social cards. Override at deploy time with
  // PUBLIC_SITE_URL if the landing page is served from another host.
  site: process.env.PUBLIC_SITE_URL ?? 'https://gleaph.com',
  devToolbar: { enabled: false },
  vite: {
    plugins: [tailwindcss()],
    resolve: {
      alias: { cookie: cookieEntry },
    },
    server: {
      fs: {
        // Allow importing the shared design tokens outside the app root.
        allow: ['../../..'],
      },
    },
  },
});
