import { describe, expect, it, vi } from 'vitest'
import { handleWorkerRequest, proxyApiRequest, type WorkerEnv } from './proxy'

class MemoryR2Object {
  constructor(
    private readonly bytes: Uint8Array,
    public readonly httpMetadata?: { contentType?: string },
  ) {}

  async arrayBuffer() {
    return this.bytes.buffer.slice(
      this.bytes.byteOffset,
      this.bytes.byteOffset + this.bytes.byteLength,
    )
  }
}

class MemoryR2Bucket {
  private readonly objects = new Map<string, MemoryR2Object>()

  async get(key: string) {
    return this.objects.get(key) ?? null
  }

  async put(key: string, value: ArrayBuffer, options?: { httpMetadata?: { contentType?: string } }) {
    this.objects.set(key, new MemoryR2Object(new Uint8Array(value), options?.httpMetadata))
  }

  async delete(key: string) {
    this.objects.delete(key)
  }
}

const defaultEnv: WorkerEnv = {
  BACKEND_BASE_URL: 'https://backend.example.com',
  BACKEND_TOKEN: 'secret-token',
  ARTIFACT_PROXY_TOKEN: 'artifact-token',
  ARTIFACTS: new MemoryR2Bucket(),
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

  it('应该允许后端通过 Worker 私有接口写入并读取 R2 产物', async () => {
    const putResponse = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/artifacts/segments/demo.md', {
        method: 'PUT',
        headers: {
          Authorization: 'Bearer artifact-token',
          'Content-Type': 'text/markdown; charset=utf-8',
        },
        body: '# demo',
      }),
      defaultEnv,
    )
    expect(putResponse.status).toBe(204)

    const getResponse = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/artifacts/segments/demo.md', {
        headers: {
          Authorization: 'Bearer artifact-token',
        },
      }),
      defaultEnv,
    )
    expect(getResponse.status).toBe(200)
    expect(getResponse.headers.get('content-type')).toContain('text/markdown')
    expect(await getResponse.text()).toBe('# demo')
  })

  it('应该拒绝未携带正确私有 token 的产物请求', async () => {
    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/artifacts/segments/demo.md'),
      defaultEnv,
    )
    expect(response.status).toBe(401)
  })

  it('应该直接通过 Worker 绑定的 R2 返回课程总稿，避免再次回打私有 Worker 接口', async () => {
    await defaultEnv.ARTIFACTS?.put(
      'courses/demo/course.md',
      new TextEncoder().encode('# cached course').buffer,
      {
        httpMetadata: {
          contentType: 'text/markdown; charset=utf-8',
        },
      },
    )

    const fetchMock = vi.fn(async () =>
      new Response(
        JSON.stringify({
          merged_markdown_path: 'courses/demo/course.md',
          manifest_path: 'courses/demo/manifest.json',
        }),
        {
          status: 200,
          headers: {
            'content-type': 'application/json; charset=utf-8',
          },
        },
      ),
    )
    vi.stubGlobal('fetch', fetchMock)

    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/api/v1/courses/demo%7Ccourse/artifacts/course.md'),
      defaultEnv,
    )

    expect(response.status).toBe(200)
    expect(await response.text()).toBe('# cached course')
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect(String(fetchMock.mock.calls[0]?.[0] ?? '')).toBe('https://backend.example.com/api/v1/courses/demo%7Ccourse')
  })
})
