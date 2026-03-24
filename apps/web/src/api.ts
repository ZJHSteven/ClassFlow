/**
 * 这个文件统一封装前端到 Worker `/api/*` 的调用。
 *
 * 设计目标：
 *
 * 1. 所有请求都走同一个错误处理入口，避免页面里反复写 `if (!response.ok)`。
 * 2. 页面层只关心“我要什么数据”，不关心 URL 拼接细节。
 * 3. 以后如果 Worker 代理规则调整，只需要改这里。
 */

import type { CourseDetail, CourseSummary, TaskDetail, TaskSummary } from './types'

export type CourseArtifactName = 'course.md' | 'manifest.json'
export type TaskArtifactName = 'segment.md' | 'segment.json' | 'events.json' | 'task.json'

async function readErrorMessage(response: Response): Promise<string> {
  let message = `请求失败，HTTP ${response.status}`
  try {
    const body = (await response.json()) as { error?: string }
    if (body.error) {
      message = body.error
    }
  } catch {
    // 这里故意静默忽略 JSON 解析错误，因为错误消息已经有兜底文本。
  }
  return message
}

async function readJson<T>(input: RequestInfo | URL, init?: RequestInit): Promise<T> {
  const response = await fetch(input, init)
  if (!response.ok) {
    throw new Error(await readErrorMessage(response))
  }

  return (await response.json()) as T
}

async function readText(input: RequestInfo | URL, init?: RequestInit): Promise<string> {
  const response = await fetch(input, init)
  if (!response.ok) {
    throw new Error(await readErrorMessage(response))
  }
  return response.text()
}

export interface TaskFilters {
  status?: string
  date?: string
  course_name?: string
}

export interface TaskStreamSnapshotPayload {
  tasks: TaskSummary[]
  generated_at: string
}

export interface CourseFilters {
  semester?: string
  date?: string
  course_name?: string
}

function buildQuery(entries: Array<[string, string | undefined]>): string {
  const params = new URLSearchParams()
  for (const [key, value] of entries) {
    if (!value) {
      continue
    }
    params.set(key, value)
  }
  const query = params.toString()
  return query ? `?${query}` : ''
}

export function listTasks(filters: TaskFilters): Promise<TaskSummary[]> {
  return readJson<TaskSummary[]>(
    `/api/v1/tasks${buildQuery([
      ['status', filters.status],
      ['date', filters.date],
      ['course_name', filters.course_name],
    ])}`,
  )
}

/**
 * 订阅任务摘要 SSE。
 *
 * 这里特意只推“任务摘要列表”，不把详情日志一起塞进流里，原因是：
 *
 * 1. 用户当前最在意的是运行中任务的阶段、进度和速率。
 * 2. 详情日志更新频率更低，继续走按需请求更省实现复杂度。
 * 3. 这样能先把最费 Worker 请求数的短轮询拿掉，而不把接口面一次性改得太重。
 */
export function subscribeTaskStream(
  filters: TaskFilters,
  onSnapshot: (payload: TaskStreamSnapshotPayload) => void,
  onStreamError: (message: string) => void,
): () => void {
  if (typeof EventSource === 'undefined') {
    onStreamError('当前环境不支持 SSE，已回退到手动刷新。')
    return () => {}
  }

  const eventSource = new EventSource(
    `/api/v1/tasks/stream${buildQuery([
      ['status', filters.status],
      ['date', filters.date],
      ['course_name', filters.course_name],
    ])}`,
  )

  const handleSnapshot = (event: Event) => {
    try {
      const messageEvent = event as MessageEvent<string>
      onSnapshot(JSON.parse(messageEvent.data) as TaskStreamSnapshotPayload)
    } catch {
      onStreamError('任务流数据解析失败，已回退到手动刷新。')
    }
  }

  const handleErrorMessage = (event: Event) => {
    try {
      const messageEvent = event as MessageEvent<string>
      const payload = JSON.parse(messageEvent.data) as { error?: string }
      onStreamError(payload.error || '任务流推送失败。')
    } catch {
      onStreamError('任务流推送失败。')
    }
  }

  eventSource.addEventListener('tasks_snapshot', handleSnapshot)
  eventSource.addEventListener('tasks_error', handleErrorMessage)

  return () => {
    eventSource.removeEventListener('tasks_snapshot', handleSnapshot)
    eventSource.removeEventListener('tasks_error', handleErrorMessage)
    eventSource.close()
  }
}

export function getTaskDetail(taskId: string): Promise<TaskDetail> {
  return readJson<TaskDetail>(`/api/v1/tasks/${encodeURIComponent(taskId)}`)
}

export async function retryTask(taskId: string): Promise<void> {
  await readJson(`/api/v1/tasks/${encodeURIComponent(taskId)}/retry`, {
    method: 'POST',
  })
}

export async function deleteTask(taskId: string): Promise<void> {
  await readJson(`/api/v1/tasks/${encodeURIComponent(taskId)}`, {
    method: 'DELETE',
  })
}

export function listCourses(filters: CourseFilters): Promise<CourseSummary[]> {
  return readJson<CourseSummary[]>(
    `/api/v1/courses${buildQuery([
      ['semester', filters.semester],
      ['date', filters.date],
      ['course_name', filters.course_name],
    ])}`,
  )
}

export function getCourseDetail(courseKey: string): Promise<CourseDetail> {
  return readJson<CourseDetail>(`/api/v1/courses/${encodeURIComponent(courseKey)}`)
}

export async function getCourseMarkdown(courseKey: string): Promise<string> {
  return readText(getCourseArtifactUrl(courseKey, 'course.md'))
}

/**
 * 统一生成课程产物地址，供“预览请求”和“下载按钮”共用。
 */
export function getCourseArtifactUrl(courseKey: string, artifactName: CourseArtifactName): string {
  return `/api/v1/courses/${encodeURIComponent(courseKey)}/artifacts/${artifactName}`
}

export function getTaskArtifactUrl(taskId: string, artifactName: TaskArtifactName): string {
  return `/api/v1/tasks/${encodeURIComponent(taskId)}/artifacts/${artifactName}`
}
