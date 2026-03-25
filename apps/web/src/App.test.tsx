import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import App from './App'

const fetchMock = vi.fn()
const setIntervalSpy = vi.spyOn(window, 'setInterval')
const anchorClickSpy = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
const createObjectUrlSpy = vi.fn(() => 'blob:test')
const revokeObjectUrlSpy = vi.fn()

class MockEventSource {
  static instances: MockEventSource[] = []

  readonly url: string
  readonly listeners = new Map<string, Set<(event: Event) => void>>()

  constructor(url: string | URL) {
    this.url = String(url)
    MockEventSource.instances.push(this)
  }

  addEventListener(type: string, listener: (event: Event) => void) {
    const listeners = this.listeners.get(type) ?? new Set<(event: Event) => void>()
    listeners.add(listener)
    this.listeners.set(type, listeners)
  }

  removeEventListener(type: string, listener: (event: Event) => void) {
    this.listeners.get(type)?.delete(listener)
  }

  close() {
    // 测试里不需要额外行为，保留空实现即可。
  }
}

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
    segment_markdown_path: 'segments/task-1.md',
    segment_json_path: 'segments/task-1.json',
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
    setIntervalSpy.mockClear()
    anchorClickSpy.mockClear()
    createObjectUrlSpy.mockClear()
    revokeObjectUrlSpy.mockClear()
    MockEventSource.instances.length = 0
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => buildMockResponse(input))
    vi.stubGlobal('fetch', fetchMock)
    vi.stubGlobal('EventSource', MockEventSource)
    Object.defineProperty(URL, 'createObjectURL', {
      configurable: true,
      writable: true,
      value: createObjectUrlSpy,
    })
    Object.defineProperty(URL, 'revokeObjectURL', {
      configurable: true,
      writable: true,
      value: revokeObjectUrlSpy,
    })
  })

  afterEach(() => {
    cleanup()
    vi.unstubAllGlobals()
  })

  it('应该渲染任务列表并切换到课程库', async () => {
    render(<App />)

    await waitFor(() => {
      expect(screen.getAllByText('病理学').length).toBeGreaterThan(0)
    })

    fireEvent.click(screen.getAllByRole('button', { name: '课程库' })[0])

    await waitFor(() => {
      expect(screen.getByText('课程总稿与交付物')).toBeInTheDocument()
    })
  })

  it('不应该使用定时轮询，而应该提供手动刷新入口', async () => {
    render(<App />)

    await waitFor(() => {
      expect(screen.getAllByRole('button', { name: '刷新列表' }).length).toBeGreaterThan(0)
    })

    expect(setIntervalSpy).not.toHaveBeenCalledWith(expect.any(Function), 5000)
    expect(setIntervalSpy).not.toHaveBeenCalledWith(expect.any(Function), 8000)
    expect(MockEventSource.instances[0]?.url).toContain('/api/v1/tasks/stream')

    fireEvent.click(screen.getAllByRole('button', { name: '课程库' })[0])

    await waitFor(() => {
      expect(screen.getByRole('button', { name: '刷新课程库' })).toBeInTheDocument()
    })

    expect(setIntervalSpy).not.toHaveBeenCalledWith(expect.any(Function), 5000)
    expect(setIntervalSpy).not.toHaveBeenCalledWith(expect.any(Function), 8000)
  })

  it('课程库应该提供总稿与清单下载按钮', async () => {
    render(<App />)

    fireEvent.click(screen.getAllByRole('button', { name: '课程库' })[0])

    await waitFor(() => {
      expect(screen.getByRole('button', { name: '下载课程总稿' })).toBeInTheDocument()
      expect(screen.getByRole('button', { name: '下载课程清单' })).toBeInTheDocument()
    })
  })

  it('任务台应该提供任务级下载按钮', async () => {
    render(<App />)

    await waitFor(() => {
      expect(screen.getByRole('button', { name: '下载任务快照' })).toBeInTheDocument()
      expect(screen.getByRole('button', { name: '下载事件日志' })).toBeInTheDocument()
    })
  })

  it('课程总稿已经预览后，再下载不应该重复请求同一份 Markdown', async () => {
    render(<App />)

    fireEvent.click(screen.getAllByRole('button', { name: '课程库' })[0])

    await waitFor(() => {
      expect(screen.getByText('已加载')).toBeInTheDocument()
    })

    fetchMock.mockClear()

    fireEvent.click(screen.getByRole('button', { name: '下载课程总稿' }))

    await waitFor(() => {
      expect(anchorClickSpy).toHaveBeenCalled()
    })

    expect(fetchMock).not.toHaveBeenCalled()
  })
})
