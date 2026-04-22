import React from 'react'
import ReactDOM from 'react-dom/client'
import { BrowserRouter } from 'react-router-dom'
import { UiClientProvider } from './context/UiClientContext'
import App from './App'
import '@fontsource-variable/inter'
import '@fontsource-variable/jetbrains-mono'
import '@pierre/diffs'
import './index.css'

type AppErrorBoundaryState = {
  hasError: boolean;
}

class AppErrorBoundary extends React.Component<React.PropsWithChildren, AppErrorBoundaryState> {
  state: AppErrorBoundaryState = { hasError: false }

  static getDerivedStateFromError(): AppErrorBoundaryState {
    return { hasError: true }
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    console.error('UI render crash recovered by AppErrorBoundary', error, info)
  }

  render() {
    if (this.state.hasError) {
      return (
        <div className="min-h-screen bg-surface-canvas text-ui-primary flex items-center justify-center px-6">
          <div className="max-w-lg w-full rounded-lg border border-status-warning/40 bg-surface-elevated p-5">
            <h1 className="text-lg font-semibold text-status-warning">UI recovered from a render error</h1>
            <p className="mt-2 text-sm text-ui-secondary">
              A component failed to render. Please refresh the page and re-open the session.
            </p>
          </div>
        </div>
      )
    }

    return this.props.children
  }
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <AppErrorBoundary>
      <BrowserRouter>
        <UiClientProvider>
          <App />
        </UiClientProvider>
      </BrowserRouter>
    </AppErrorBoundary>
  </React.StrictMode>,
)
