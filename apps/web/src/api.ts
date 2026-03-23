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

async function readJson<T>(input: RequestInfo | URL, init?: RequestInit): Promise<T> {
  const response = await fetch(input, init)
  if (!response.ok) {
    let message = `请求失败，HTTP ${response.status}`
    try {
      const body = (await response.json()) as { error?: string }
      if (body.error) {
        message = body.error
      }
    } catch {
      // 这里故意静默忽略 JSON 解析错误，因为错误消息已经有兜底文本。
    }
    throw new Error(message)
  }

  return (await response.json()) as T
}

export interface TaskFilters {
  status?: string
  date?: string
  course_name?: string
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

export function getTaskDetail(taskId: string): Promise<TaskDetail> {
  return readJson<TaskDetail>(`/api/v1/tasks/${encodeURIComponent(taskId)}`)
}

export async function retryTask(taskId: string): Promise<void> {
  await readJson(`/api/v1/tasks/${encodeURIComponent(taskId)}/retry`, {
    method: 'POST',
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
  const response = await fetch(getCourseArtifactUrl(courseKey, 'course.md'))
  if (!response.ok) {
    throw new Error(`课程总稿读取失败，HTTP ${response.status}`)
  }
  return response.text()
}

/**
 * 统一生成课程产物地址，供“预览请求”和“下载按钮”共用。
 */
export function getCourseArtifactUrl(courseKey: string, artifactName: CourseArtifactName): string {
  return `/api/v1/courses/${encodeURIComponent(courseKey)}/artifacts/${artifactName}`
}
