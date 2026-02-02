/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        cyber: {
          bg: '#0a0e27',        // Deep dark blue-black
          surface: '#141b3d',   // Slightly lighter surface
          border: '#1e2a5e',    // Dark blue border
          cyan: '#00fff9',      // Neon cyan
          magenta: '#ff00ff',   // Neon magenta
          purple: '#b026ff',    // Neon purple
          lime: '#39ff14',      // Neon lime green
          orange: '#ff6b35',    // Neon orange
        }
      },
      boxShadow: {
        'neon-cyan': '0 0 10px rgba(0, 255, 249, 0.5)',
        'neon-magenta': '0 0 10px rgba(255, 0, 255, 0.5)',
        'neon-purple': '0 0 10px rgba(176, 38, 255, 0.5)',
        'neon-lime': '0 0 10px rgba(57, 255, 20, 0.5)',
      },
      backgroundImage: {
        'grid-pattern': 'linear-gradient(rgba(0, 255, 249, 0.1) 1px, transparent 1px), linear-gradient(90deg, rgba(0, 255, 249, 0.1) 1px, transparent 1px)',
      },
      animation: {
        'glow-pulse': 'glow-pulse 2s ease-in-out infinite',
        'slide-in-right': 'slide-in-right 0.3s ease-out',
        'slide-in-left': 'slide-in-left 0.3s ease-out',
        'slide-down': 'slide-down 0.25s ease-out',
        'slide-up': 'slide-up 0.2s ease-in',
        'fade-in': 'fade-in 0.5s ease-out',
        'fade-in-up': 'fade-in-up 0.5s ease-out',
      },
      keyframes: {
        'glow-pulse': {
          '0%, 100%': { boxShadow: '0 0 5px rgba(0, 255, 249, 0.5)' },
          '50%': { boxShadow: '0 0 20px rgba(0, 255, 249, 0.8)' },
        },
        'slide-in-right': {
          '0%': { transform: 'translateX(100%)' },
          '100%': { transform: 'translateX(0)' },
        },
        'slide-in-left': {
          '0%': { transform: 'translateX(-100%)' },
          '100%': { transform: 'translateX(0)' },
        },
        'slide-down': {
          '0%': { transform: 'translateY(-100%)', opacity: '0' },
          '100%': { transform: 'translateY(0)', opacity: '1' },
        },
        'slide-up': {
          '0%': { transform: 'translateY(0)', opacity: '1' },
          '100%': { transform: 'translateY(-100%)', opacity: '0' },
        },
        'fade-in': {
          '0%': { opacity: '0' },
          '100%': { opacity: '1' },
        },
        'fade-in-up': {
          '0%': { opacity: '0', transform: 'translateY(20px)' },
          '100%': { opacity: '1', transform: 'translateY(0)' },
        },
      },
    },
  },
  plugins: [
    require('@tailwindcss/typography'),
  ],
}
