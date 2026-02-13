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
          bg: 'rgba(var(--cyber-bg-rgb), <alpha-value>)',
          surface: 'rgba(var(--cyber-surface-rgb), <alpha-value>)',
          border: 'rgba(var(--cyber-border-rgb), <alpha-value>)',
          cyan: 'rgba(var(--cyber-cyan-rgb), <alpha-value>)',
          magenta: 'rgba(var(--cyber-magenta-rgb), <alpha-value>)',
          purple: 'rgba(var(--cyber-purple-rgb), <alpha-value>)',
          lime: 'rgba(var(--cyber-lime-rgb), <alpha-value>)',
          orange: 'rgba(var(--cyber-orange-rgb), <alpha-value>)',
        },
        ui: {
          primary: 'rgba(var(--ui-text-primary-rgb), <alpha-value>)',
          secondary: 'rgba(var(--ui-text-secondary-rgb), <alpha-value>)',
          muted: 'rgba(var(--ui-text-muted-rgb), <alpha-value>)',
        }
      },
      boxShadow: {
        'neon-cyan': '0 0 10px rgba(var(--cyber-cyan-rgb), 0.5)',
        'neon-magenta': '0 0 10px rgba(var(--cyber-magenta-rgb), 0.5)',
        'neon-purple': '0 0 10px rgba(var(--cyber-purple-rgb), 0.5)',
        'neon-lime': '0 0 10px rgba(var(--cyber-lime-rgb), 0.5)',
      },
      backgroundImage: {
        'grid-pattern': 'linear-gradient(rgba(var(--cyber-cyan-rgb), 0.1) 1px, transparent 1px), linear-gradient(90deg, rgba(var(--cyber-cyan-rgb), 0.1) 1px, transparent 1px)',
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
          '0%, 100%': { boxShadow: '0 0 5px rgba(var(--cyber-cyan-rgb), 0.5)' },
          '50%': { boxShadow: '0 0 20px rgba(var(--cyber-cyan-rgb), 0.8)' },
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
