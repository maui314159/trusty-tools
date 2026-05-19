/** @type {import('tailwindcss').Config} */
export default {
  // `class` strategy: the theme store toggles `class="dark"` on <html> at
  // runtime, matching isTauri()-style runtime detection used elsewhere.
  darkMode: 'class',
  content: ['./index.html', './web.html', './src/**/*.{svelte,js,ts}'],
  theme: {
    extend: {
      colors: {
        // Brand + surface tokens. Each token has a base (dark-first) value
        // and a `-light` variant consumed via the `dark:` inversion pattern.
        'trusty-primary': '#4f46e5', // indigo-600
        'trusty-surface': '#0f172a', // slate-900 (dark)
        'trusty-surface-light': '#ffffff', // white (light)
        'trusty-border': '#334155', // slate-700 (dark)
        'trusty-border-light': '#e2e8f0', // slate-200 (light)
        'trusty-text': '#f1f5f9', // slate-100 (dark)
        'trusty-text-light': '#0f172a', // slate-900 (light)
        // Session status palette.
        'status-running': '#22c55e', // green-500
        'status-paused': '#f59e0b', // amber-500
        'status-error': '#ef4444', // red-500
        'status-stopped': '#6b7280', // gray-500
      },
    },
  },
  plugins: [],
};
