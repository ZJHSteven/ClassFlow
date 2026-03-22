import { describe, expect, it, vi } from 'vitest'
import { handleWorkerRequest, proxyApiRequest, type WorkerEnv } from './proxy'

const defaultEnv: WorkerEnv = {
  BACKEND_BASE_URL: 'https://backend.example.com',
  BACKEND_TOKEN: 'secret-token',
  ASSETS: {
    fetch: vi.fn(async () => new Response('asset', { status: 200 })),
  },
}

describe('worker proxy', () => {
  it('应该把 /api/* 请求转发到后端并追加 Authorization', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ ok: true }), { status: 200 }))
    vi.stubGlobal('fetch', fetchMock)

    const response = await proxyApiRequest(new Request('https://worker.example.com/api/v1/tasks'), defaultEnv)
    expect(response.status).toBe(200)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    const firstCall = fetchMock.mock.calls[0]
    const targetUrl = String(firstCall?.[0] ?? '')
    const init = (firstCall?.[1] ?? {}) as RequestInit
    expect(targetUrl).toBe('https://backend.example.com/api/v1/tasks')
    expect((init.headers as Headers).get('Authorization')).toBe('Bearer secret-token')
  })

  it('应该在非 API 请求时回退到静态资源绑定', async () => {
    const response = await handleWorkerRequest(new Request('https://worker.example.com/'), defaultEnv)
    expect(response.status).toBe(200)
    expect(defaultEnv.ASSETS.fetch).toHaveBeenCalled()
  })

  it('应该把后端 401 掩码成 Worker 层错误', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('unauthorized', { status: 401 })))

    const response = await proxyApiRequest(new Request('https://worker.example.com/api/v1/tasks'), defaultEnv)
    expect(response.status).toBe(502)
    const body = await response.json()
    expect(body.error).toContain('BACKEND_TOKEN')
  })
})
