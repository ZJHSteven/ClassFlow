import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App'
import './index.css'
import { clearAppBadge } from './push'

// iOS 的“添加到主屏幕”面板会优先读取当前文档标题；这里和静态 HTML 双保险，
// 避免从很老的 Vite 页面状态恢复时继续沿用模板标题。
document.title = 'ClassFlow'

if ('serviceWorker' in navigator) {
  void navigator.serviceWorker.register('/sw.js', { scope: '/' })
}
void clearAppBadge()

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
