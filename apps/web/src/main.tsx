import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App'
import './index.css'
import { clearAppBadge } from './push'

if ('serviceWorker' in navigator) {
  void navigator.serviceWorker.register('/sw.js', { scope: '/' })
}
void clearAppBadge()

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
