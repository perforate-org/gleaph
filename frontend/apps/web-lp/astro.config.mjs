import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  vite: {
    plugins: [tailwindcss()],
    server: {
      fs: {
        // Allow importing the shared design tokens outside the app root.
        allow: ['../../..'],
      },
    },
  },
});
