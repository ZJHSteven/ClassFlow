/**
 * 任务台面板。
 *
 * 这个组件把“任务列表 + 任务详情”放到一个视图里：
 *
 * 1. 左侧负责任务筛选和列表。
 * 2. 右侧负责当前任务的详情与重试。
 * 3. 默认不做定时轮询，避免界面频繁闪烁，也避免 Cloudflare Worker 无意义计费。
 * 4. 采用“首次加载 + 手动刷新 + 页面重新回到前台时同步一次”的折中策略。
 */

import { startTransition, useCallback, useEffect, useRef, useState } from 'react'
import { getTaskDetail, listTasks, retryTask } from '../api'
import type { TaskDetail, TaskStatus, TaskSummary } from '../types'

function statusLabel(status: TaskStatus) {
  const labelMap: Record<TaskStatus, string> = {
    pending: '等待中',
    running: '执行中',
    succeeded: '成功',
    failed: '失败',
  }
  return labelMap[status]
}

/**
 * 把“最近同步时间”格式化成更适合界面阅读的短时间文本。
 *
 * 输入：
 * - `timestamp`：ISO 时间字符串。
 *
 * 输出：
 * - 返回 `HH:mm:ss` 形式的时间；若为空则返回“尚未同步”。
 */
function formatSyncTime(timestamp: string) {
  if (!timestamp) {
    return '尚未同步'
  }

  return new Date(timestamp).toLocaleTimeString('zh-CN', {
    hour12: false,
  })
}

export function TaskPanel() {
  const [tasks, setTasks] = useState<TaskSummary[]>([])
  const [selectedTaskId, setSelectedTaskId] = useState<string>('')
  const [selectedTaskDetail, setSelectedTaskDetail] = useState<TaskDetail | null>(null)
  const [statusFilter, setStatusFilter] = useState<string>('')
  const [dateFilter, setDateFilter] = useState<string>('')
  const [courseFilter, setCourseFilter] = useState<string>('')
  const [errorMessage, setErrorMessage] = useState<string>('')
  const [isLoading, setIsLoading] = useState<boolean>(true)
  const [isRefreshing, setIsRefreshing] = useState<boolean>(false)
  const [lastSyncedAt, setLastSyncedAt] = useState<string>('')
  const listRequestIdRef = useRef<number>(0)
  const detailRequestIdRef = useRef<number>(0)
  const selectedTaskIdRef = useRef<string>('')

  useEffect(() => {
    selectedTaskIdRef.current = selectedTaskId
  }, [selectedTaskId])

  /**
   * 单独刷新某个任务的详情。
   *
   * 这里把列表刷新和详情刷新拆开，是为了避免“切换当前选中任务”时还要重新拉整个列表。
   * 另外使用 `requestId` 丢弃过期响应，防止前一次慢请求把新状态覆盖掉。
   */
  const loadTaskDetail = useCallback(async (taskId: string) => {
    const requestId = detailRequestIdRef.current + 1
    detailRequestIdRef.current = requestId

    try {
      const detail = await getTaskDetail(taskId)
      if (detailRequestIdRef.current !== requestId) {
        return
      }

      startTransition(() => {
        setSelectedTaskDetail(detail)
      })
    } catch (error) {
      if (detailRequestIdRef.current === requestId) {
        setErrorMessage(error instanceof Error ? error.message : '任务详情加载失败')
      }
    }
  }, [])

  /**
   * 刷新任务列表，并在必要时顺手同步当前选中任务的详情。
   *
   * 输入：
   * - `blocking`：是否把这次刷新视为“阻塞式首次加载”。
   * - `refreshDetail`：刷新完列表后，是否顺便刷新当前选中任务详情。
   *
   * 核心取舍：
   * - 首次进入页面时显示完整加载态。
   * - 后续刷新只显示轻量“同步中”状态，不再让整块内容闪一下。
   * - 使用 `startTransition()` 把大块列表替换标记为非紧急更新，减少界面顿挫。
   */
  const loadTasks = useCallback(async (options?: { blocking?: boolean; refreshDetail?: boolean }) => {
    const { blocking = false, refreshDetail = false } = options ?? {}
    const requestId = listRequestIdRef.current + 1
    listRequestIdRef.current = requestId

    try {
      if (blocking) {
        setIsLoading(true)
      } else {
        setIsRefreshing(true)
      }

      const nextTasks = await listTasks({
        status: statusFilter || undefined,
        date: dateFilter || undefined,
        course_name: courseFilter || undefined,
      })

      if (listRequestIdRef.current !== requestId) {
        return
      }

      const nextSelectedTaskId = nextTasks.some((task) => task.id === selectedTaskIdRef.current)
        ? selectedTaskIdRef.current
        : nextTasks[0]?.id ?? ''

      startTransition(() => {
        setTasks(nextTasks)
        setSelectedTaskId(nextSelectedTaskId)
        setErrorMessage('')
        setLastSyncedAt(new Date().toISOString())
      })

      if (refreshDetail) {
        if (nextSelectedTaskId) {
          await loadTaskDetail(nextSelectedTaskId)
        } else {
          startTransition(() => {
            setSelectedTaskDetail(null)
          })
        }
      }
    } catch (error) {
      if (listRequestIdRef.current === requestId) {
        setErrorMessage(error instanceof Error ? error.message : '任务列表加载失败')
      }
    } finally {
      if (listRequestIdRef.current === requestId) {
        setIsLoading(false)
        setIsRefreshing(false)
      }
    }
  }, [courseFilter, dateFilter, loadTaskDetail, statusFilter])

  useEffect(() => {
    void loadTasks({
      blocking: true,
      refreshDetail: true,
    })
  }, [loadTasks])

  useEffect(() => {
    /**
     * 只在“窗口重新聚焦”或“标签页重新可见”时同步一次。
     *
     * 这样既能在用户切回页面时看到比较新的状态，
     * 又不会像轮询那样持续消耗 Worker 请求次数。
     */
    const syncWhenVisibleAgain = () => {
      if (document.visibilityState !== 'visible') {
        return
      }

      void loadTasks({
        blocking: false,
        refreshDetail: true,
      })
    }

    window.addEventListener('focus', syncWhenVisibleAgain)
    document.addEventListener('visibilitychange', syncWhenVisibleAgain)

    return () => {
      window.removeEventListener('focus', syncWhenVisibleAgain)
      document.removeEventListener('visibilitychange', syncWhenVisibleAgain)
    }
  }, [loadTasks])

  useEffect(() => {
    if (!selectedTaskId) {
      setSelectedTaskDetail(null)
      return
    }

    void loadTaskDetail(selectedTaskId)
  }, [loadTaskDetail, selectedTaskId])

  const handleRetry = async () => {
    if (!selectedTaskDetail) {
      return
    }
    await retryTask(selectedTaskDetail.task.id)
    await loadTasks({
      blocking: false,
      refreshDetail: true,
    })
  }

  return (
    <section className="panel">
      <div className="panel__grid">
        <div className="card card--padded">
          <div className="card__header">
            <div>
              <h2>任务队列</h2>
              <p>默认不做定时轮询；手动刷新，或切回页面时自动同步一次。</p>
            </div>
            <div className="card__actions">
              <div className="syncHint">最近同步：{formatSyncTime(lastSyncedAt)}</div>
              <button
                type="button"
                className="buttonSecondary"
                onClick={() =>
                  void loadTasks({
                    blocking: false,
                    refreshDetail: true,
                  })
                }
                disabled={isRefreshing}
              >
                {isRefreshing ? '同步中...' : '刷新列表'}
              </button>
            </div>
          </div>

          <div className="filters">
            <div className="field">
              <label htmlFor="task-status">状态</label>
              <select id="task-status" value={statusFilter} onChange={(event) => setStatusFilter(event.target.value)}>
                <option value="">全部</option>
                <option value="pending">等待中</option>
                <option value="running">执行中</option>
                <option value="succeeded">成功</option>
                <option value="failed">失败</option>
              </select>
            </div>
            <div className="field">
              <label htmlFor="task-date">日期</label>
              <input id="task-date" value={dateFilter} onChange={(event) => setDateFilter(event.target.value)} placeholder="2026-03-20" />
            </div>
            <div className="field">
              <label htmlFor="task-course">课程名</label>
              <input id="task-course" value={courseFilter} onChange={(event) => setCourseFilter(event.target.value)} placeholder="病理学" />
            </div>
          </div>

          {errorMessage ? <div className="emptyState">{errorMessage}</div> : null}

          <div className="tableWrap">
            <table className="table">
              <thead>
                <tr>
                  <th>课程</th>
                  <th>时间</th>
                  <th>状态</th>
                  <th>阶段</th>
                </tr>
              </thead>
              <tbody>
                {tasks.map((task) => (
                  <tr key={task.id} onClick={() => setSelectedTaskId(task.id)}>
                    <td>
                      <strong>{task.course_name}</strong>
                      <div>{task.teacher_name}</div>
                    </td>
                    <td>
                      <div>{task.date}</div>
                      <div>
                        {task.start_time} - {task.end_time}
                      </div>
                    </td>
                    <td>
                      <span className={`statusPill statusPill--${task.status}`}>{statusLabel(task.status)}</span>
                    </td>
                    <td>{task.stage}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {isLoading ? <div className="emptyState">正在加载任务列表...</div> : null}
          {!isLoading && tasks.length === 0 ? <div className="emptyState">当前没有任务记录。</div> : null}
        </div>

        <aside className="card card--padded detail">
          <div className="card__header">
            <div>
              <h3>任务详情</h3>
              <p>查看当前任务完整阶段日志，并对失败任务执行重试。</p>
            </div>
            {selectedTaskDetail?.task.status === 'failed' ? (
              <button type="button" onClick={() => void handleRetry()}>
                重试任务
              </button>
            ) : null}
          </div>

          {selectedTaskDetail ? (
            <>
              <div className="detail__meta">
                <div>
                  <strong>{selectedTaskDetail.task.course_name}</strong> / {selectedTaskDetail.task.teacher_name}
                </div>
                <div>
                  {selectedTaskDetail.task.date} {selectedTaskDetail.task.start_time} - {selectedTaskDetail.task.end_time}
                </div>
                <div>状态：{statusLabel(selectedTaskDetail.task.status)}</div>
                <div>阶段：{selectedTaskDetail.task.stage}</div>
                {selectedTaskDetail.task.last_error ? <div>错误：{selectedTaskDetail.task.last_error}</div> : null}
              </div>

              <div className="detail__events">
                {selectedTaskDetail.events.map((event) => (
                  <div key={event.id} className="detail__event">
                    <small>
                      {event.created_at} / {event.stage} / {event.level}
                    </small>
                    <div>{event.message}</div>
                  </div>
                ))}
              </div>
            </>
          ) : (
            <div className="emptyState">从左侧选择一个任务查看详情。</div>
          )}
        </aside>
      </div>
    </section>
  )
}
