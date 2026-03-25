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

import { motion } from 'motion/react'
import { startTransition, useCallback, useEffect, useRef, useState } from 'react'
import {
  deleteTask,
  getTaskArtifactUrl,
  getTaskDetail,
  listTasks,
  retryTask,
  subscribeTaskStream,
} from '../api'
import { downloadWithProgress } from '../downloads'
import { formatBytes, formatEta, formatPercent, formatSpeed, normalizePercent } from '../progress'
import type { TaskDetail, TaskStage, TaskStatus, TaskSummary } from '../types'

interface DownloadButtonState {
  status: 'idle' | 'downloading' | 'succeeded' | 'failed'
  progressPercent: number | null
  receivedBytes: number
  totalBytes: number | null
  speedBytesPerSec: number | null
  errorMessage: string
}

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

/**
 * 把 SSE 推来的最新任务摘要合并进当前详情。
 *
 * 这里故意只覆盖“任务摘要里本来就有”的字段：
 *
 * 1. 进度、速率、阶段、状态这些需要实时刷新。
 * 2. 事件日志、产物路径等详情字段继续保留上一次显式加载结果。
 * 3. 这样既能消掉高频详情轮询，又不会把右侧详情面板整个清空重建。
 */
function mergeTaskSummaryIntoDetail(detail: TaskDetail | null, summary: TaskSummary | undefined): TaskDetail | null {
  if (!detail || !summary || detail.task.id !== summary.id) {
    return detail
  }

  return {
    ...detail,
    task: {
      ...detail.task,
      id: summary.id,
      batch_id: summary.batch_id,
      status: summary.status,
      stage: summary.stage,
      semester: summary.semester,
      course_key: summary.course_key,
      course_name: summary.course_name,
      teacher_name: summary.teacher_name,
      date: summary.date,
      start_time: summary.start_time,
      end_time: summary.end_time,
      last_error: summary.last_error,
      progress_percent: summary.progress_percent,
      transferred_bytes: summary.transferred_bytes,
      total_bytes: summary.total_bytes,
      rate_bytes_per_sec: summary.rate_bytes_per_sec,
      eta_seconds: summary.eta_seconds,
      updated_at: summary.updated_at,
    },
  }
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
  const [isRetrying, setIsRetrying] = useState<boolean>(false)
  const [isDeleting, setIsDeleting] = useState<boolean>(false)
  const [downloadStates, setDownloadStates] = useState<Record<string, DownloadButtonState>>({})
  const [lastSyncedAt, setLastSyncedAt] = useState<string>('')
  const [isPageVisible, setIsPageVisible] = useState<boolean>(document.visibilityState === 'visible')
  const listRequestIdRef = useRef<number>(0)
  const detailRequestIdRef = useRef<number>(0)
  const selectedTaskIdRef = useRef<string>('')
  const pressableProps = {
    whileHover: { y: -2, scale: 1.01 },
    whileTap: { y: 0, scale: 0.985 },
    transition: { type: 'spring' as const, stiffness: 380, damping: 24 },
  }

  useEffect(() => {
    selectedTaskIdRef.current = selectedTaskId
  }, [selectedTaskId])

  useEffect(() => {
    const handleVisibilityChange = () => {
      setIsPageVisible(document.visibilityState === 'visible')
    }

    document.addEventListener('visibilitychange', handleVisibilityChange)
    return () => {
      document.removeEventListener('visibilitychange', handleVisibilityChange)
    }
  }, [])

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
    if (!isPageVisible) {
      return
    }

    return subscribeTaskStream(
      {
        status: statusFilter || undefined,
        date: dateFilter || undefined,
        course_name: courseFilter || undefined,
      },
      (payload) => {
        const nextTasks = payload.tasks
        const nextSelectedTaskId = nextTasks.some((task) => task.id === selectedTaskIdRef.current)
          ? selectedTaskIdRef.current
          : nextTasks[0]?.id ?? ''
        const nextSelectedSummary = nextTasks.find((task) => task.id === nextSelectedTaskId)

        startTransition(() => {
          setTasks(nextTasks)
          setSelectedTaskId(nextSelectedTaskId)
          setSelectedTaskDetail((current) => mergeTaskSummaryIntoDetail(current, nextSelectedSummary))
          setListErrorMessage('')
          setLastSyncedAt(payload.generated_at || new Date().toISOString())
        })
      },
      (message) => {
        setListErrorMessage(message)
      },
    )
  }, [courseFilter, dateFilter, isPageVisible, statusFilter])

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

    try {
      setIsRetrying(true)
      await retryTask(selectedTaskDetail.task.id)
      await loadTasks({
        blocking: false,
        refreshDetail: true,
      })
    } finally {
      setIsRetrying(false)
    }
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
          key: 'task.json',
          label: '下载任务快照',
          href: getTaskArtifactUrl(selectedTaskDetail.task.id, 'task.json'),
          filename: `${selectedTaskDetail.task.course_name}-${selectedTaskDetail.task.date}-task.json`,
        },
        {
          key: 'events.json',
          label: '下载事件日志',
          href: getTaskArtifactUrl(selectedTaskDetail.task.id, 'events.json'),
          filename: `${selectedTaskDetail.task.course_name}-${selectedTaskDetail.task.date}-events.json`,
        },
        selectedTaskDetail.task.segment_markdown_path
          ? {
              key: 'segment.md',
              label: '下载单节 Markdown',
              href: getTaskArtifactUrl(selectedTaskDetail.task.id, 'segment.md'),
              filename: `${selectedTaskDetail.task.course_name}-${selectedTaskDetail.task.date}-segment.md`,
            }
          : null,
        selectedTaskDetail.task.segment_json_path
          ? {
              key: 'segment.json',
              label: '下载单节 JSON',
              href: getTaskArtifactUrl(selectedTaskDetail.task.id, 'segment.json'),
              filename: `${selectedTaskDetail.task.course_name}-${selectedTaskDetail.task.date}-segment.json`,
            }
          : null,
      ].filter(Boolean) as Array<{ key: string; label: string; href: string; filename: string }>
    : []

  const transferPercent = normalizePercent(selectedTaskDetail?.task.progress_percent)
  const shouldShowTransferPanel =
    !!selectedTaskDetail &&
    (selectedTaskDetail.task.stage === 'downloading' ||
      selectedTaskDetail.task.stage === 'uploading_audio' ||
      selectedTaskDetail.task.progress_percent != null ||
      selectedTaskDetail.task.transferred_bytes != null ||
      selectedTaskDetail.task.total_bytes != null)

  const handleTaskDownload = async (action: { key: string; href: string; filename: string }) => {
    setDownloadStates((current) => ({
      ...current,
      [action.key]: {
        status: 'downloading',
        progressPercent: 0,
        receivedBytes: 0,
        totalBytes: null,
        speedBytesPerSec: 0,
        errorMessage: '',
      },
    }))

    try {
      await downloadWithProgress(action.href, action.filename, (snapshot) => {
        setDownloadStates((current) => ({
          ...current,
          [action.key]: {
            status: 'downloading',
            progressPercent: snapshot.progressPercent,
            receivedBytes: snapshot.receivedBytes,
            totalBytes: snapshot.totalBytes,
            speedBytesPerSec: snapshot.speedBytesPerSec,
            errorMessage: '',
          },
        }))
      })

      setDownloadStates((current) => {
        const nextState = current[action.key]
        return {
          ...current,
          [action.key]: {
            ...(nextState ?? {
              progressPercent: 100,
              receivedBytes: 0,
              totalBytes: null,
              speedBytesPerSec: null,
            }),
            status: 'succeeded',
            progressPercent: 100,
            errorMessage: '',
          },
        }
      })
    } catch (error) {
      setDownloadStates((current) => ({
        ...current,
        [action.key]: {
          ...(current[action.key] ?? {
            progressPercent: null,
            receivedBytes: 0,
            totalBytes: null,
            speedBytesPerSec: null,
          }),
          status: 'failed',
          errorMessage: error instanceof Error ? error.message : '下载失败',
        },
      }))
    }
  }

  return (
    <section className="panel">
      <div className="panel__grid">
        <motion.div
          className="card card--padded card--panel"
          layout
          initial={{ opacity: 0, x: -10 }}
          animate={{ opacity: 1, x: 0 }}
          transition={{ duration: 0.24, ease: 'easeOut' }}
        >
          <div className="card__header">
            <div>
              <h2>任务队列</h2>
              <p>任务进度通过 SSE 实时推送；任务详情日志仍以手动刷新和回到前台同步为主。</p>
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
            <motion.div className="field" whileHover={{ y: -1 }}>
              <label htmlFor="task-status">状态</label>
              <select id="task-status" value={statusFilter} onChange={(event) => setStatusFilter(event.target.value)}>
                <option value="">全部</option>
                <option value="pending">等待中</option>
                <option value="running">执行中</option>
                <option value="succeeded">成功</option>
                <option value="failed">失败</option>
              </select>
            </motion.div>
            <motion.div className="field" whileHover={{ y: -1 }}>
              <label htmlFor="task-date">日期</label>
              <input id="task-date" value={dateFilter} onChange={(event) => setDateFilter(event.target.value)} placeholder="2026-03-20" />
            </motion.div>
            <motion.div className="field" whileHover={{ y: -1 }}>
              <label htmlFor="task-course">课程名</label>
              <input id="task-course" value={courseFilter} onChange={(event) => setCourseFilter(event.target.value)} placeholder="病理学" />
            </motion.div>
          </div>

          {listErrorMessage ? <div className="detailNotice detailNotice--error">{listErrorMessage}</div> : null}

          <div className="tableWrap tableWrap--list">
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
                    <td>
                      <div>{stageLabel(task.stage)}</div>
                      {task.status === 'running' && (task.progress_percent != null || task.rate_bytes_per_sec != null) ? (
                        <small>
                          {formatPercent(task.progress_percent)} / {formatSpeed(task.rate_bytes_per_sec)}
                        </small>
                      ) : null}
                    </td>
                  </motion.tr>
                ))}
              </tbody>
            </table>
          </div>

          {isLoading ? <div className="emptyState">正在加载任务列表...</div> : null}
          {!isLoading && tasks.length === 0 ? <div className="emptyState">当前没有任务记录。</div> : null}
        </motion.div>

        <motion.aside
          className="card card--padded detail detail--panel"
          layout
          initial={{ opacity: 0, x: 10 }}
          animate={{ opacity: 1, x: 0 }}
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
                  <motion.button
                    type="button"
                    className="buttonSecondary"
                    onClick={() => void handleRetry()}
                    disabled={isRetrying}
                    {...pressableProps}
                  >
                    {isRetrying ? '重试中...' : '重试任务'}
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
            <div className="detail__body">
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
                <div className="detailNotice detailNotice--error detailNotice--scrollable">
                  {selectedTaskDetail.task.last_error}
                </div>
              ) : (
                <div className="detailNotice detailNotice--info">这里的按钮会直接下载当前任务已有的 JSON、日志和单节 Markdown 备份。</div>
              )}

              {shouldShowTransferPanel ? (
                <div className="transferCard">
                  <div className="transferCard__header">
                    <strong>{selectedTaskDetail.task.stage === 'uploading_audio' ? '上传进度' : '下载进度'}</strong>
                    <span>{formatPercent(transferPercent)}</span>
                  </div>
                  <div className="transferBar">
                    <div
                      className="transferBar__fill"
                      style={{ width: `${transferPercent ?? 0}%` }}
                    />
                  </div>
                  <div className="transferMeta">
                    <span>
                      {formatBytes(selectedTaskDetail.task.transferred_bytes)} / {formatBytes(selectedTaskDetail.task.total_bytes)}
                    </span>
                    <span>速率：{formatSpeed(selectedTaskDetail.task.rate_bytes_per_sec)}</span>
                    <span>ETA：{formatEta(selectedTaskDetail.task.eta_seconds)}</span>
                  </div>
                </div>
              ) : null}

              <div className="detailActionGrid">
                {taskActions.map((action) => (
                  <div key={action.key} className="downloadAction">
                    <motion.button
                      type="button"
                      className={
                        downloadStates[action.key]?.status === 'failed'
                          ? 'artifactPill artifactPill--error'
                          : 'artifactPill'
                      }
                      onClick={() => void handleTaskDownload(action)}
                      disabled={downloadStates[action.key]?.status === 'downloading'}
                      {...pressableProps}
                    >
                      {downloadStates[action.key]?.status === 'downloading'
                        ? `${action.label} · ${formatPercent(downloadStates[action.key]?.progressPercent)}`
                        : action.label}
                    </motion.button>
                    {downloadStates[action.key]?.status === 'downloading' ? (
                      <div className="downloadAction__meta">
                        <div className="miniBar">
                          <div
                            className="miniBar__fill"
                            style={{ width: `${normalizePercent(downloadStates[action.key]?.progressPercent) ?? 6}%` }}
                          />
                        </div>
                        <span>
                          {formatBytes(downloadStates[action.key]?.receivedBytes)} / {formatBytes(downloadStates[action.key]?.totalBytes)}
                        </span>
                        <span>{formatSpeed(downloadStates[action.key]?.speedBytesPerSec)}</span>
                      </div>
                    ) : null}
                    {downloadStates[action.key]?.status === 'failed' ? (
                      <div className="downloadAction__error">{downloadStates[action.key]?.errorMessage}</div>
                    ) : null}
                  </div>
                ))}
              </div>

              <div className="detailSection detailSection--grow">
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
            </div>
          ) : (
            <div className="detail__body">
              <div className="emptyState">{detailMessage || '从左侧选择一个任务查看详情。'}</div>
            </div>
          )}
        </motion.aside>
      </div>
    </section>
  )
}
