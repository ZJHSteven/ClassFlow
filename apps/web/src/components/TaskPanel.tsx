/**
 * 任务台面板。
 *
 * 这个组件把“任务列表 + 任务详情”放到一个视图里：
 *
 * 1. 左侧负责任务筛选和列表。
 * 2. 右侧负责当前任务的详情与重试。
 * 3. 使用轻量轮询，保持状态变化能及时反映到界面上。
 */

import { useEffect, useState } from 'react'
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

export function TaskPanel() {
  const [tasks, setTasks] = useState<TaskSummary[]>([])
  const [selectedTaskId, setSelectedTaskId] = useState<string>('')
  const [selectedTaskDetail, setSelectedTaskDetail] = useState<TaskDetail | null>(null)
  const [statusFilter, setStatusFilter] = useState<string>('')
  const [dateFilter, setDateFilter] = useState<string>('')
  const [courseFilter, setCourseFilter] = useState<string>('')
  const [errorMessage, setErrorMessage] = useState<string>('')
  const [isLoading, setIsLoading] = useState<boolean>(true)

  useEffect(() => {
    let isMounted = true

    const loadTasks = async () => {
      try {
        if (isMounted) {
          setIsLoading(true)
        }
        const nextTasks = await listTasks({
          status: statusFilter || undefined,
          date: dateFilter || undefined,
          course_name: courseFilter || undefined,
        })

        if (!isMounted) {
          return
        }

        setTasks(nextTasks)
        setErrorMessage('')

        if (!selectedTaskId && nextTasks[0]) {
          setSelectedTaskId(nextTasks[0].id)
        }
      } catch (error) {
        if (isMounted) {
          setErrorMessage(error instanceof Error ? error.message : '任务列表加载失败')
        }
      } finally {
        if (isMounted) {
          setIsLoading(false)
        }
      }
    }

    void loadTasks()
    const timer = window.setInterval(() => {
      void loadTasks()
    }, 5000)

    return () => {
      isMounted = false
      window.clearInterval(timer)
    }
  }, [statusFilter, dateFilter, courseFilter, selectedTaskId])

  useEffect(() => {
    if (!selectedTaskId) {
      setSelectedTaskDetail(null)
      return
    }

    let isMounted = true

    const loadTaskDetail = async () => {
      try {
        const detail = await getTaskDetail(selectedTaskId)
        if (isMounted) {
          setSelectedTaskDetail(detail)
        }
      } catch (error) {
        if (isMounted) {
          setErrorMessage(error instanceof Error ? error.message : '任务详情加载失败')
        }
      }
    }

    void loadTaskDetail()

    return () => {
      isMounted = false
    }
  }, [selectedTaskId])

  const handleRetry = async () => {
    if (!selectedTaskDetail) {
      return
    }
    await retryTask(selectedTaskDetail.task.id)
    setSelectedTaskDetail(await getTaskDetail(selectedTaskDetail.task.id))
    setTasks(await listTasks({}))
  }

  return (
    <section className="panel">
      <div className="panel__grid">
        <div className="card card--padded">
          <div className="card__header">
            <div>
              <h2>任务队列</h2>
              <p>实时查看当前任务在哪个阶段卡住，并按课程或日期快速过滤。</p>
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
