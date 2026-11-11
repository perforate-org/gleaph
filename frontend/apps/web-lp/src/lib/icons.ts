// Inline icon set for the landing page. Every icon is a 24x24 stroke drawing
// that inherits currentColor, so it follows the theme without extra variants.
const stroke = 'fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"';

export const icons = {
  copy: `<svg viewBox="0 0 24 24" ${stroke}><rect x="9" y="9" width="11" height="11" rx="2.5"/><path d="M5 15V6a2 2 0 0 1 2-2h9"/></svg>`,
  check: `<svg viewBox="0 0 24 24" ${stroke}><path d="M20 6 9 17l-5-5"/></svg>`,
  sun: `<svg viewBox="0 0 24 24" ${stroke}><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>`,
  moon: `<svg viewBox="0 0 24 24" ${stroke}><path d="M20 14.5A8.5 8.5 0 0 1 9.5 4 8.5 8.5 0 1 0 20 14.5Z"/></svg>`,
  graph: `<svg viewBox="0 0 24 24" ${stroke}><circle cx="6" cy="17" r="2.2"/><circle cx="17.5" cy="6.5" r="2.2"/><circle cx="18" cy="17" r="2.2"/><path d="M7.9 15.4 15.7 8M8.2 17h7.6"/></svg>`,
  text: `<svg viewBox="0 0 24 24" ${stroke}><path d="M4 6h16M4 10.5h10M4 15h16M4 19.5h7"/></svg>`,
  vector: `<svg viewBox="0 0 24 24" ${stroke}><rect x="3.5" y="3.5" width="6" height="6" rx="1.4"/><rect x="14.5" y="3.5" width="6" height="6" rx="1.4"/><rect x="3.5" y="14.5" width="6" height="6" rx="1.4"/><path d="M14.5 17.5h6M17.5 14.5v6"/></svg>`,
  shield: `<svg viewBox="0 0 24 24" ${stroke}><path d="M12 3 5 6v5.4c0 4.7 2.9 7.9 7 9.6 4.1-1.7 7-4.9 7-9.6V6l-7-3Z"/><path d="m9 12 2.2 2.2L15.5 10"/></svg>`,
  key: `<svg viewBox="0 0 24 24" ${stroke}><circle cx="8" cy="15" r="3.6"/><path d="m10.6 12.4 8.6-8.6M16.2 3.9l3.4 3.4M13.6 6.5l3.4 3.4"/></svg>`,
  shards: `<svg viewBox="0 0 24 24" ${stroke}><rect x="3.5" y="3.5" width="7" height="7" rx="1.4"/><rect x="13.5" y="3.5" width="7" height="7" rx="1.4"/><rect x="3.5" y="13.5" width="7" height="7" rx="1.4"/><rect x="13.5" y="13.5" width="7" height="7" rx="1.4"/></svg>`,
  terminal: `<svg viewBox="0 0 24 24" ${stroke}><rect x="3" y="4.5" width="18" height="15" rx="2.5"/><path d="m7 9.5 3 2.5-3 2.5M12.5 15H17"/></svg>`,
  book: `<svg viewBox="0 0 24 24" ${stroke}><path d="M4 5.5A2.5 2.5 0 0 1 6.5 3H20v15H6.5A2.5 2.5 0 0 0 4 20.5V5.5Z"/><path d="M4 20.5A2.5 2.5 0 0 1 6.5 18H20M8 7h8M8 11h5"/></svg>`,
  github: `<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 2C6.48 2 2 6.58 2 12.25c0 4.53 2.87 8.37 6.84 9.73.5.09.68-.22.68-.49 0-.24-.01-1.04-.01-1.89-2.78.62-3.37-1.21-3.37-1.21-.45-1.18-1.11-1.49-1.11-1.49-.91-.64.07-.62.07-.62 1 .07 1.53 1.05 1.53 1.05.89 1.56 2.34 1.11 2.91.85.09-.66.35-1.11.63-1.37-2.22-.26-4.56-1.14-4.56-5.07 0-1.12.39-2.03 1.03-2.75-.1-.26-.45-1.3.1-2.71 0 0 .84-.27 2.75 1.05a9.4 9.4 0 0 1 2.5-.34c.85 0 1.71.12 2.5.34 1.91-1.32 2.75-1.05 2.75-1.05.55 1.41.2 2.45.1 2.71.64.72 1.03 1.63 1.03 2.75 0 3.94-2.34 4.8-4.57 5.06.36.32.68.94.68 1.9 0 1.37-.01 2.47-.01 2.81 0 .27.18.59.69.49A10.04 10.04 0 0 0 22 12.25C22 6.58 17.52 2 12 2Z"/></svg>`,
} as const;

export type IconName = keyof typeof icons;
