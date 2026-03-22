/**
 * Worker 代理的核心逻辑。
 *
 * 约束很明确：
 *
 * 1. 浏览器只能访问 Worker，自身不持有后端密钥。
 * 2. `/api/*` 请求由 Worker 自动追加 Bearer Token 再转发到后端。
 * 3. 非 `/api/*` 请求直接回退到静态资源绑定 `ASSETS.fetch()`。
 */

export interface AssetBindingLike {
  fetch(request: Request): Promise<Response>
}

export interface WorkerEnv {
  BACKEND_BASE_URL: string
  BACKEND_TOKEN: string
  ASSETS: AssetBindingLike
}

function jsonResponse(status: number, error: string) {
  return new Response(JSON.stringify({ error }), {
    status,
    headers: {
      'content-type': 'application/json; charset=utf-8',
    },
  })
}

export async function proxyApiRequest(request: Request, env: WorkerEnv): Promise<Response> {
  if (!env.BACKEND_BASE_URL || !env.BACKEND_TOKEN) {
    return jsonResponse(500, 'Worker 尚未配置 BACKEND_BASE_URL 或 BACKEND_TOKEN。')
  }

  const incomingUrl = new URL(request.url)
  const backendUrl = new URL(`${incomingUrl.pathname}${incomingUrl.search}`, env.BACKEND_BASE_URL)

  const headers = new Headers(request.headers)
  headers.set('Authorization', `Bearer ${env.BACKEND_TOKEN}`)
  headers.delete('host')

  const body =
    request.method === 'GET' || request.method === 'HEAD'
      ? undefined
      : await request.arrayBuffer()

  const response = await fetch(backendUrl.toString(), {
    method: request.method,
    headers,
    body,
    redirect: 'manual',
  })

  if (response.status === 401) {
    return jsonResponse(502, '后端鉴权失败，请检查 Worker 中配置的 BACKEND_TOKEN。')
  }

  if (response.status >= 500) {
    return jsonResponse(502, '后端服务异常，请稍后重试。')
  }

  return response
}

export async function handleWorkerRequest(request: Request, env: WorkerEnv): Promise<Response> {
  const url = new URL(request.url)
  if (url.pathname.startsWith('/api/')) {
    return proxyApiRequest(request, env)
  }

  return env.ASSETS.fetch(request)
}
