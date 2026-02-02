import React from 'react'
import ReactDOM from 'react-dom/client'
import { BrowserRouter } from 'react-router-dom'
import { UiClientProvider } from './context/UiClientContext'
import App from './App'
import './index.css'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <BrowserRouter>
      <UiClientProvider>
        <App />
      </UiClientProvider>
    </BrowserRouter>
  </React.StrictMode>,
)
