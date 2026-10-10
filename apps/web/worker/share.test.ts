import { afterEach, describe, expect, it, vi } from 'vitest'
import { handleShareWorkerRequest } from './share'

afterEach(() => {
  vi.unstubAllGlobals()
})

const env = {
  ASSETS: {
    fetch: vi.fn(async () => new Response('<html>share</html>', { status: 200 })),
  },
}

describe('public share worker', () => {
  it('根路径应该重定向到 /share', async () => {
    const response = await handleShareWorkerRequest(
      new Request('https://share.example.com/'),
      env,
    )
    expect(response.status).toBe(302)
    expect(response.headers.get('location')).toBe('https://share.example.com/share')
  })

  it('分享站必须拒绝管理 API 与私有产物入口', async () => {
    const apiResponse = await handleShareWorkerRequest(
      new Request('https://share.example.com/api/v1/tasks'),
      env,
    )
    const privateResponse = await handleShareWorkerRequest(
      new Request('https://share.example.com/__classflow/artifacts/demo'),
      env,
    )

    expect(apiResponse.status).toBe(404)
    expect(privateResponse.status).toBe(404)
  })

  it('/share 应该回退到 SPA 根资源', async () => {
    const response = await handleShareWorkerRequest(
      new Request('https://share.example.com/share'),
      env,
    )
    expect(response.status).toBe(200)
    expect(await response.text()).toContain('share')
  })
})
