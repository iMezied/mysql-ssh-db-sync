/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        env: {
          prod: "#dc2626",
          staging: "#d97706",
          dev: "#059669",
        },
      },
    },
  },
  plugins: [],
};
