/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        surface: {
          canvas: 'rgba(var(--surface-canvas-rgb), <alpha-value>)',
          elevated: 'rgba(var(--surface-elevated-rgb), <alpha-value>)',
          border: 'rgba(var(--surface-border-rgb), <alpha-value>)',
        },
        accent: {
          primary: 'rgba(var(--accent-primary-rgb), <alpha-value>)',
          secondary: 'rgba(var(--accent-secondary-rgb), <alpha-value>)',
          tertiary: 'rgba(var(--accent-tertiary-rgb), <alpha-value>)',
        },
        status: {
          success: 'rgba(var(--status-success-rgb), <alpha-value>)',
          warning: 'rgba(var(--status-warning-rgb), <alpha-value>)',
        },
        ui: {
          primary: 'rgba(var(--ui-text-primary-rgb), <alpha-value>)',
          secondary: 'rgba(var(--ui-text-secondary-rgb), <alpha-value>)',
          muted: 'rgba(var(--ui-text-muted-rgb), <alpha-value>)',
        }
      },
      boxShadow: {
        'glow-primary': '0 0 10px rgba(var(--accent-primary-rgb), 0.5)',
        'glow-secondary': '0 0 10px rgba(var(--accent-secondary-rgb), 0.5)',
        'glow-tertiary': '0 0 10px rgba(var(--accent-tertiary-rgb), 0.5)',
        'glow-success': '0 0 10px rgba(var(--status-success-rgb), 0.5)',
      },
      backgroundImage: {
        'grid-pattern': 'linear-gradient(rgba(var(--accent-primary-rgb), 0.1) 1px, transparent 1px), linear-gradient(90deg, rgba(var(--accent-primary-rgb), 0.1) 1px, transparent 1px)',
      },
      animation: {
        'accent-pulse': 'accent-pulse 2s ease-in-out infinite',
        'slide-in-right': 'slide-in-right 0.3s ease-out',
        'slide-in-left': 'slide-in-left 0.3s ease-out',
        'slide-down': 'slide-down 0.25s ease-out',
        'slide-up': 'slide-up 0.2s ease-in',
        'fade-in': 'fade-in 0.5s ease-out',
        'fade-in-up': 'fade-in-up 0.5s ease-out',
      },
      keyframes: {
        'accent-pulse': {
          '0%, 100%': { boxShadow: '0 0 5px rgba(var(--accent-primary-rgb), 0.5)' },
          '50%': { boxShadow: '0 0 20px rgba(var(--accent-primary-rgb), 0.8)' },
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
