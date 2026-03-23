/**
 * 任务台面板。
 *
 * 这个组件负责“任务列表 + 任务详情 + 任务级下载动作”三件事：
 *
 * 1. 左侧按任务维度浏览和筛选。
 * 2. 右侧集中展示当前任务的阶段、错误、日志和下载入口。
 * 3. 对失败任务提供“重试”和“彻底删除”两个明确动作。
 *
 * 这里特意把“下载按钮”放在详情区的显眼位置，
 * 而不是塞进一堆小字里，目的是让用户一眼知道：
 * “这个任务到底能导出什么，点哪里拿。”
 */

import { motion, useReducedMotion } from 'motion/react'
import { startTransition, useCallback, useEffect, useRef, useState } from 'react'
import {
  deleteTask,
  getTaskArtifactUrl,
  getTaskDetail,
  listTasks,
  retryTask,
} from '../api'
import type { TaskDetail, TaskStage, TaskStatus, TaskSummary } from '../types'

function statusLabel(status: TaskStatus) {
  const labelMap: Record<TaskStatus, string> = {
    pending: '等待中',
    running: '执行中',
    succeeded: '成功',
    failed: '失败',
  }
  return labelMap[status]
}

function stageLabel(stage: TaskStage) {
  const labelMap: Record<TaskStage, string> = {
    queued: '排队中',
    downloading: '下载中',
    extracting_audio: '抽音频',
    uploading_audio: '上传音频',
    transcribing: '转写中',
    storing_artifacts: '写入产物',
    merging_course: '合并课程',
    cleanup: '清理中',
    done: '已完成',
  }
  return labelMap[stage]
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
  const [listErrorMessage, setListErrorMessage] = useState<string>('')
  const [detailMessage, setDetailMessage] = useState<string>('')
  const [isLoading, setIsLoading] = useState<boolean>(true)
  const [isRefreshing, setIsRefreshing] = useState<boolean>(false)
  const [isDeleting, setIsDeleting] = useState<boolean>(false)
  const [lastSyncedAt, setLastSyncedAt] = useState<string>('')
  const listRequestIdRef = useRef<number>(0)
  const detailRequestIdRef = useRef<number>(0)
  const selectedTaskIdRef = useRef<string>('')
  const shouldReduceMotion = useReducedMotion()

  const pressableProps = shouldReduceMotion
    ? {}
    : {
        whileHover: { y: -2, scale: 1.01 },
        whileTap: { y: 0, scale: 0.985 },
        transition: { type: 'spring' as const, stiffness: 380, damping: 24 },
      }

  useEffect(() => {
    selectedTaskIdRef.current = selectedTaskId
  }, [selectedTaskId])

  /**
   * 单独刷新某个任务的详情。
   *
   * 这里用 `requestId` 丢弃过期响应，
   * 目的是避免“用户快速切换两条任务，慢请求覆盖快请求”。
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
        setDetailMessage('')
      })
    } catch (error) {
      if (detailRequestIdRef.current === requestId) {
        startTransition(() => {
          setSelectedTaskDetail(null)
          setDetailMessage(error instanceof Error ? error.message : '任务详情加载失败')
        })
      }
    }
  }, [])

  /**
   * 刷新任务列表，并在必要时同步当前选中任务的详情。
   *
   * 设计细节：
   *
   * 1. 首次进入页面走阻塞加载，后续刷新只显示轻量“同步中”。
   * 2. 当选中项没变时，刷新后会顺手更新右侧详情。
   * 3. 当选中项变化时，让下面的 `useEffect` 接管详情刷新，避免重复请求。
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
        setListErrorMessage('')
        setLastSyncedAt(new Date().toISOString())
      })

      if (refreshDetail) {
        if (nextSelectedTaskId && nextSelectedTaskId === selectedTaskIdRef.current) {
          await loadTaskDetail(nextSelectedTaskId)
        } else {
          startTransition(() => {
            setSelectedTaskDetail(null)
            setDetailMessage('')
          })
        }
      }
    } catch (error) {
      if (listRequestIdRef.current === requestId) {
        setListErrorMessage(error instanceof Error ? error.message : '任务列表加载失败')
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
      setDetailMessage('')
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

  const handleDelete = async () => {
    if (!selectedTaskDetail) {
      return
    }

    const shouldDelete = window.confirm(
      '这会删除该失败任务的本地工作目录、列表记录，以及它已有的任务级备份。确定继续吗？',
    )
    if (!shouldDelete) {
      return
    }

    try {
      setIsDeleting(true)
      await deleteTask(selectedTaskDetail.task.id)
      await loadTasks({
        blocking: false,
        refreshDetail: true,
      })
    } finally {
      setIsDeleting(false)
    }
  }

  const taskActions = selectedTaskDetail
    ? [
        {
          label: '下载任务快照',
          href: getTaskArtifactUrl(selectedTaskDetail.task.id, 'task.json'),
        },
        {
          label: '下载事件日志',
          href: getTaskArtifactUrl(selectedTaskDetail.task.id, 'events.json'),
        },
        selectedTaskDetail.task.segment_markdown_path
          ? {
              label: '下载单节 Markdown',
              href: getTaskArtifactUrl(selectedTaskDetail.task.id, 'segment.md'),
            }
          : null,
        selectedTaskDetail.task.segment_json_path
          ? {
              label: '下载单节 JSON',
              href: getTaskArtifactUrl(selectedTaskDetail.task.id, 'segment.json'),
            }
          : null,
      ].filter(Boolean) as Array<{ label: string; href: string }>
    : []

  return (
    <section className="panel">
      <div className="panel__grid">
        <motion.div
          className="card card--padded"
          layout
          initial={shouldReduceMotion ? false : { opacity: 0, x: -10 }}
          animate={shouldReduceMotion ? undefined : { opacity: 1, x: 0 }}
          transition={{ duration: 0.24, ease: 'easeOut' }}
        >
          <div className="card__header">
            <div>
              <h2>任务队列</h2>
              <p>默认不做定时轮询；手动刷新，或切回页面时自动同步一次。</p>
            </div>
            <div className="card__actions">
              <div className="syncHint">最近同步：{formatSyncTime(lastSyncedAt)}</div>
              <motion.button
                type="button"
                className="buttonSecondary"
                onClick={() =>
                  void loadTasks({
                    blocking: false,
                    refreshDetail: true,
                  })
                }
                disabled={isRefreshing}
                {...pressableProps}
              >
                {isRefreshing ? '同步中...' : '刷新列表'}
              </motion.button>
            </div>
          </div>

          <div className="filters">
            <motion.div className="field" whileHover={shouldReduceMotion ? undefined : { y: -1 }}>
              <label htmlFor="task-status">状态</label>
              <select id="task-status" value={statusFilter} onChange={(event) => setStatusFilter(event.target.value)}>
                <option value="">全部</option>
                <option value="pending">等待中</option>
                <option value="running">执行中</option>
                <option value="succeeded">成功</option>
                <option value="failed">失败</option>
              </select>
            </motion.div>
            <motion.div className="field" whileHover={shouldReduceMotion ? undefined : { y: -1 }}>
              <label htmlFor="task-date">日期</label>
              <input id="task-date" value={dateFilter} onChange={(event) => setDateFilter(event.target.value)} placeholder="2026-03-20" />
            </motion.div>
            <motion.div className="field" whileHover={shouldReduceMotion ? undefined : { y: -1 }}>
              <label htmlFor="task-course">课程名</label>
              <input id="task-course" value={courseFilter} onChange={(event) => setCourseFilter(event.target.value)} placeholder="病理学" />
            </motion.div>
          </div>

          {listErrorMessage ? <div className="detailNotice detailNotice--error">{listErrorMessage}</div> : null}

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
                  <motion.tr
                    key={task.id}
                    layout
                    className={
                      task.id === selectedTaskId
                        ? 'tableRow tableRow--interactive is-selected'
                        : 'tableRow tableRow--interactive'
                    }
                    onClick={() => setSelectedTaskId(task.id)}
                    {...pressableProps}
                  >
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
                    <td>{stageLabel(task.stage)}</td>
                  </motion.tr>
                ))}
              </tbody>
            </table>
          </div>

          {isLoading ? <div className="emptyState">正在加载任务列表...</div> : null}
          {!isLoading && tasks.length === 0 ? <div className="emptyState">当前没有任务记录。</div> : null}
        </motion.div>

        <motion.aside
          className="card card--padded detail"
          layout
          initial={shouldReduceMotion ? false : { opacity: 0, x: 10 }}
          animate={shouldReduceMotion ? undefined : { opacity: 1, x: 0 }}
          transition={{ duration: 0.24, ease: 'easeOut' }}
        >
          <div className="card__header">
            <div>
              <h3>任务详情</h3>
              <p>右侧给出当前任务的原始下载入口、阶段日志，以及失败后的重试/删除动作。</p>
            </div>
            <div className="card__actions card__actions--inline">
              {selectedTaskDetail?.task.status === 'failed' ? (
                <>
                  <motion.button type="button" className="buttonSecondary" onClick={() => void handleRetry()} {...pressableProps}>
                    重试任务
                  </motion.button>
                  <motion.button
                    type="button"
                    className="buttonDanger"
                    onClick={() => void handleDelete()}
                    disabled={isDeleting}
                    {...pressableProps}
                  >
                    {isDeleting ? '删除中...' : '放弃并删除'}
                  </motion.button>
                </>
              ) : null}
            </div>
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
                <div>阶段：{stageLabel(selectedTaskDetail.task.stage)}</div>
              </div>

              {selectedTaskDetail.task.last_error ? (
                <div className="detailNotice detailNotice--error">{selectedTaskDetail.task.last_error}</div>
              ) : (
                <div className="detailNotice detailNotice--info">这里的按钮会直接下载当前任务已有的 JSON、日志和单节 Markdown 备份。</div>
              )}

              <div className="detailActionGrid">
                {taskActions.map((action) => (
                  <motion.a key={action.label} className="artifactPill" href={action.href} download {...pressableProps}>
                    {action.label}
                  </motion.a>
                ))}
              </div>

              <div className="detailSection">
                <h4>阶段日志</h4>
                <div className="detail__events">
                  {selectedTaskDetail.events.map((event) => (
                    <motion.div key={event.id} className="detail__event" layout>
                      <small>
                        {event.created_at} / {event.stage} / {event.level}
                      </small>
                      <div>{event.message}</div>
                    </motion.div>
                  ))}
                </div>
              </div>
            </>
          ) : (
            <div className="emptyState">{detailMessage || '从左侧选择一个任务查看详情。'}</div>
          )}
        </motion.aside>
      </div>
    </section>
  )
}
