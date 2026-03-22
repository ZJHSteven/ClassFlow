import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import App from './App'

const fetchMock = vi.fn()

const taskListPayload = [
  {
    id: 'task-1',
    batch_id: 'batch-1',
    status: 'running',
    stage: 'transcribing',
    semester: '2025-2026-2',
    course_key: '2025-2026-2|2026-03-20|病理学|王老师',
    course_name: '病理学',
    teacher_name: '王老师',
    date: '2026-03-20',
    start_time: '08:00',
    end_time: '08:45',
    last_error: null,
    created_at: '2026-03-22T00:00:00Z',
    updated_at: '2026-03-22T00:00:00Z',
  },
]

const taskDetailPayload = {
  task: {
    id: 'task-1',
    batch_id: 'batch-1',
    status: 'running',
    stage: 'transcribing',
    semester: '2025-2026-2',
    course_key: '2025-2026-2|2026-03-20|病理学|王老师',
    course_name: '病理学',
    teacher_name: '王老师',
    date: '2026-03-20',
    start_time: '08:00',
    end_time: '08:45',
    new_id: '123',
    page_url: 'https://example.test/page',
    mp4_url: 'https://example.test/video.mp4',
    last_error: null,
    created_at: '2026-03-22T00:00:00Z',
    updated_at: '2026-03-22T00:00:00Z',
  },
  events: [],
}

const courseListPayload = [
  {
    course_key: '2025-2026-2|2026-03-20|病理学|王老师',
    semester: '2025-2026-2',
    course_name: '病理学',
    teacher_name: '王老师',
    date: '2026-03-20',
    received_segment_count: 1,
    successful_segment_count: 1,
    has_failed_segment: false,
    merged_markdown_path: 'course.md',
    manifest_path: 'manifest.json',
    updated_at: '2026-03-22T00:00:00Z',
  },
]

const courseDetailPayload = {
  course_key: '2025-2026-2|2026-03-20|病理学|王老师',
  semester: '2025-2026-2',
  course_name: '病理学',
  teacher_name: '王老师',
  date: '2026-03-20',
  received_segment_count: 1,
  successful_segment_count: 1,
  has_failed_segment: false,
  merged_markdown_path: 'course.md',
  manifest_path: 'manifest.json',
  segments: [],
  updated_at: '2026-03-22T00:00:00Z',
}

/**
 * 按 URL 分发假响应，避免使用“第几次 fetch 返回什么”的脆弱写法。
 *
 * React 组件里存在首次加载、详情加载、轮询等多条请求链。
 * 如果测试只靠 `mockResolvedValueOnce()` 排顺序，一旦请求次数或先后顺序改变，
 * 测试就会把“课程列表”错喂给“课程详情”，进而触发类型错误。
 */
function buildMockResponse(input: RequestInfo | URL): Response {
  const requestUrl = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url

  if (requestUrl.includes('/api/v1/tasks/') && !requestUrl.endsWith('/retry')) {
    return new Response(JSON.stringify(taskDetailPayload), { status: 200 })
  }

  if (requestUrl.includes('/api/v1/tasks')) {
    return new Response(JSON.stringify(taskListPayload), { status: 200 })
  }

  if (requestUrl.includes('/api/v1/courses/') && requestUrl.includes('/artifacts/course.md')) {
    return new Response('# 课程总稿\n\n测试内容', { status: 200 })
  }

  if (requestUrl.includes('/api/v1/courses/')) {
    return new Response(JSON.stringify(courseDetailPayload), { status: 200 })
  }

  if (requestUrl.includes('/api/v1/courses')) {
    return new Response(JSON.stringify(courseListPayload), { status: 200 })
  }

  throw new Error(`测试未覆盖的请求地址：${requestUrl}`)
}

describe('App', () => {
  beforeEach(() => {
    fetchMock.mockReset()
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => buildMockResponse(input))
    vi.stubGlobal('fetch', fetchMock)
  })

  it('应该渲染任务列表并切换到课程库', async () => {
    render(<App />)

    await waitFor(() => {
      expect(screen.getByText('病理学')).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole('button', { name: '课程库' }))

    await waitFor(() => {
      expect(screen.getByText('课程总稿预览')).toBeInTheDocument()
    })
  })
})
