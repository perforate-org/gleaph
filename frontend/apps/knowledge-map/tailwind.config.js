/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        surface: "#0b1020",
        panel: "#111827",
        line: "#334155",
        ink: "#e5eefc",
      },
      boxShadow: {
        glow: "0 0 32px rgba(45, 212, 191, 0.25)",
      },
    },
  },
  plugins: [],
};
