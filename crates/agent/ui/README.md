# QueryMT Agent Dashboard

A themeable web interface for interacting with the QueryMT agent via the Agent Client Protocol (ACP).

## ✨ Features

### Visual Design
- **Base16 Theme System**: Switchable dark and light themes, with QueryMate as the default preset
- **Glow Effects**: Interactive elements use accent-based glow effects that intensify on hover
- **Grid Background**: Subtle accent-grid pattern
- **Smooth Animations**: Fade-in, slide, and pulse animations throughout
- **Responsive Design**: Works seamlessly on desktop and mobile devices

### Session Management
- **Manual Session Creation**: Sessions must be created explicitly via the sidebar (no auto-creation)
- **Sidebar Panel**: Collapsible panel for session management with connection status
- **Session Info Display**: View active session ID, message count, and creation time
- **Clear Session Status**: Visual indicators for session and connection state

### Message Display
- **Markdown Rendering**: Full GitHub Flavored Markdown support with `react-markdown`
- **Syntax Highlighting**: Code blocks and diffs adapt to the active dashboard theme
- **Tool Call Visualization**: Dedicated UI for tool calls and results with status indicators
- **Typing Indicator**: Animated indicator shows when agent is processing
- **Event Timeline**: Virtualized scrolling with react-virtuoso for performance

## 🚀 Quick Start

### Development
```bash
npm install
npm run dev
```

The dashboard will be available at `http://localhost:5173`

### Production Build
```bash
npm run build
npm run preview
```

## 📐 Architecture

### Components

```
src/
├── components/
│   ├── Sidebar.tsx           # Session management panel
│   ├── MessageContent.tsx    # Markdown and code rendering
│   └── TypingIndicator.tsx   # Loading animation
├── hooks/
│   └── useACPClient.ts       # WebSocket connection & ACP protocol
├── acp/
│   └── client.ts             # ACP client implementation
├── utils/
│   └── webSocketStream.ts    # WebSocket stream utilities
├── App.tsx                   # Main application
├── types.ts                  # TypeScript definitions
└── index.css                 # Global styles
```

### Hooks

- **`useACPClient.ts`**: WebSocket connection and ACP protocol handling
  - Auto-connects on mount
  - Manages session state
  - Handles event streaming from agent
  - Provides `newSession()` and `sendPrompt()` methods

### Styling

- **Tailwind CSS**: Utility-first CSS framework with semantic design tokens
- **Custom Theme Tokens**: Surface/accent/status colors, glows, and animations in `tailwind.config.js`
- **Global Styles**: Base styles and utilities in `index.css`
- **Typography Plugin**: Better prose styling with `@tailwindcss/typography`

## 🎨 Theme Customization

### Built-in Dashboard Themes

- Use the header theme picker (palette icon) to switch between built-in Base16 themes (dark + light) and the QueryMate default theme.
- Theme choice is persisted in local storage (`dashboardTheme`) via the UI store.
- Theme variables are applied at runtime using CSS custom properties so existing Tailwind classes update automatically.
- Code highlighting and diffs follow the selected dashboard theme.

### Color Palette

```javascript
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
  }
}
```

### Custom Utilities

- `.glow-border-primary`: Primary accent border with glow effect
- `.glow-border-secondary`: Secondary accent border with glow
- `.glow-border-tertiary`: Tertiary accent border with glow
- `.glow-text-primary`: Primary accent text with glow
- `.glow-text-secondary`: Secondary accent text with glow
- `.glow-text-success`: Success text with glow
- `.grid-background`: Accent grid pattern overlay

### Animations

- `animate-accent-pulse`: Pulsing glow effect
- `animate-fade-in`: Fade in animation
- `animate-fade-in-up`: Fade in with upward motion
- `animate-slide-in-left`: Slide in from left
- `animate-slide-in-right`: Slide in from right

## 📦 Dependencies

### Core
- **React 18**: UI library
- **TypeScript**: Type safety
- **Vite**: Fast build tool
- **Tailwind CSS**: Utility-first styling

### ACP Integration
- **@agentclientprotocol/sdk**: Official ACP TypeScript SDK
- **WebSocket**: Real-time bidirectional communication

### UI/UX
- **react-virtuoso**: Virtualized scrolling for performance
- **lucide-react**: Beautiful icon library
- **react-markdown**: Markdown rendering
- **remark-gfm**: GitHub Flavored Markdown support
- **@tailwindcss/typography**: Enhanced prose styling
- **@pierre/diffs**: Code diff rendering (installed, ready for integration)

## 🔄 Usage Flow

1. **Connect**: Application automatically connects to WebSocket on load
2. **Create Session**: Open sidebar (menu icon) and click "Create New Session"
3. **Chat**: Enter prompts in the input area at the bottom
4. **View Events**: Messages, tool calls, and results appear in the timeline
5. **Session Info**: Check session details anytime in the sidebar

## 🛠️ Troubleshooting

### Connection Issues
- Ensure the agent backend is running on `ws://127.0.0.1:3030/acp/ws`
- Check browser console for WebSocket connection errors
- Verify firewall settings allow WebSocket connections

### Session Creation Fails
- Verify backend is ready to accept session creation requests
- Check that the working directory path in `useACPClient.ts` is valid
- Review backend logs for error messages

### Build Errors
```bash
# Clear cache and reinstall
rm -rf node_modules dist
npm install
npm run build
```

### Styling Issues
```bash
# Rebuild Tailwind CSS
npm run build
```

## 💡 Development Tips

### Hot Reload
Changes to components, styles, and hooks are automatically reflected in the browser during development.

### TypeScript Type Checking
Run type checking separately:
```bash
npx tsc --noEmit
```

### Tailwind Autocomplete
Install the Tailwind CSS IntelliSense extension for your editor to get autocomplete for utility classes.

### Debugging WebSocket
Open browser DevTools → Network tab → WS filter to see WebSocket messages in real-time.

## 🚧 Future Enhancements

- [ ] Full @pierre/diffs integration for rich code diffs with line-by-line comparison
- [ ] Session history and ability to switch between multiple sessions
- [ ] Inline code review with annotations and comments
- [ ] Message reactions and threading
- [ ] Export conversation history to markdown/JSON
- [ ] Keyboard shortcuts for common actions
- [ ] Search and filter messages in timeline
- [ ] Voice input support
- [ ] File upload/attachment support
- [ ] Multi-agent conversation support

## 📋 Architecture Notes

### WebSocket vs HTTP
The dashboard uses WebSocket for bidirectional, real-time communication with the agent backend. This is more efficient than HTTP polling for streaming events.

### ACP Protocol
The Agent Client Protocol (ACP) defines the message format for client-agent communication:
- **Initialization**: Handshake with protocol version
- **Session Management**: Create and manage agent sessions
- **Prompts**: Send user input to agent
- **Notifications**: Receive streaming updates from agent

### Event Types
- `user_message_chunk`: User input echo
- `agent_message_chunk`: Agent response text
- `tool_call`: Tool execution started
- `tool_call_update`: Tool execution completed/failed

## 📄 License

Part of the QueryMT project.
