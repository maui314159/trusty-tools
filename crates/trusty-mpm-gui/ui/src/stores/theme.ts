// Why: Dark/light preference must survive reloads and apply before paint to
// avoid a flash; one store owns the value and the `<html>` class side effect.
// What: A `theme` writable persisted to `localStorage['trusty-mpm.theme']`,
// an `applyTheme()` DOM sync, and a `toggleTheme()` helper for ThemeToggle.
// Test: Call `toggleTheme()` and assert `<html>` gains/loses the `dark` class
// and `localStorage` holds the new value across a simulated reload.

import { writable } from 'svelte/store';

/** Two-state UI theme. */
export type Theme = 'dark' | 'light';

const STORAGE_KEY = 'trusty-mpm.theme';

/** Read the persisted theme, defaulting to dark (the design baseline). */
function initialTheme(): Theme {
  if (typeof localStorage === 'undefined') return 'dark';
  return localStorage.getItem(STORAGE_KEY) === 'light' ? 'light' : 'dark';
}

/** Sync the `<html>` `dark` class to the given theme. */
function applyTheme(value: Theme): void {
  if (typeof document === 'undefined') return;
  document.documentElement.classList.toggle('dark', value === 'dark');
}

/** Current theme; updates persist to localStorage and the DOM. */
export const theme = writable<Theme>(initialTheme());

theme.subscribe((value) => {
  applyTheme(value);
  if (typeof localStorage !== 'undefined') {
    localStorage.setItem(STORAGE_KEY, value);
  }
});

/**
 * Why: ThemeToggle needs a one-call flip without knowing the current value.
 * What: Inverts the `theme` store between `dark` and `light`.
 * Test: Starting from `dark`, call once → `light`; call again → `dark`.
 */
export function toggleTheme(): void {
  theme.update((current) => (current === 'dark' ? 'light' : 'dark'));
}
