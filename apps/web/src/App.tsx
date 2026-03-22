/**
 * 这个文件只负责页面级编排：
 *
 * 1. 顶部展示项目定位和当前模式。
 * 2. 负责“任务台 / 课程库”两个主视图切换。
 * 3. 不直接处理复杂的数据抓取，让数据逻辑下沉到各自面板组件里。
 *
 * 这样做的好处是：
 *
 * 1. 页面结构稳定，样式更容易统一维护。
 * 2. 后续如果再增加“系统设置”页，不需要重写现有逻辑。
 */

import { useState } from 'react'
import { CoursePanel } from './components/CoursePanel'
import { TaskPanel } from './components/TaskPanel'
import './App.css'

type ViewMode = 'tasks' | 'courses'

function App() {
  const [viewMode, setViewMode] = useState<ViewMode>('tasks')

  return (
    <div className="shell">
      <header className="hero">
        <div className="hero__eyebrow">ClassFlow / Worker Console</div>
        <div className="hero__content">
          <div>
            <h1>智慧课堂转写任务台</h1>
            <p>
              统一查看油猴脚本推送进来的课程片段，追踪后台转写状态，并在课程库中直接预览合并稿。
            </p>
          </div>
          <div className="hero__badgeList">
            <span>Rust 后端</span>
            <span>Cloudflare Worker 代理</span>
            <span>React 管理台</span>
          </div>
        </div>
      </header>

      <nav className="tabBar" aria-label="主视图切换">
        <button
          type="button"
          className={viewMode === 'tasks' ? 'tabBar__button is-active' : 'tabBar__button'}
          onClick={() => setViewMode('tasks')}
        >
          任务台
        </button>
        <button
          type="button"
          className={viewMode === 'courses' ? 'tabBar__button is-active' : 'tabBar__button'}
          onClick={() => setViewMode('courses')}
        >
          课程库
        </button>
      </nav>

      <main className="content">
        {viewMode === 'tasks' ? <TaskPanel /> : <CoursePanel />}
      </main>
    </div>
  )
}

export default App
