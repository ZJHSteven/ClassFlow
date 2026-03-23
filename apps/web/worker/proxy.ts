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

export interface R2ObjectLike {
  arrayBuffer(): Promise<ArrayBuffer>
  httpMetadata?: {
    contentType?: string
  }
}

export interface R2BucketLike {
  get(key: string): Promise<R2ObjectLike | null>
  put(
    key: string,
    value: ArrayBuffer,
    options?: {
      httpMetadata?: {
        contentType?: string
      }
    },
  ): Promise<unknown>
  delete(key: string): Promise<void>
}

export interface WorkerEnv {
  BACKEND_BASE_URL: string
  BACKEND_TOKEN: string
  ARTIFACT_PROXY_TOKEN?: string
  ARTIFACTS?: R2BucketLike
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

function unauthorizedArtifactResponse() {
  return jsonResponse(401, '未通过 Worker 产物接口鉴权。')
}

function readArtifactKeyFromPath(pathname: string): string {
  const prefix = '/__classflow/artifacts/'
  const rawKey = pathname.slice(prefix.length)
  return rawKey
    .split('/')
    .filter(Boolean)
    .map((segment) => decodeURIComponent(segment))
    .join('/')
}

function isAuthorizedArtifactRequest(request: Request, env: WorkerEnv): boolean {
  const token = request.headers.get('Authorization')?.replace(/^Bearer\s+/i, '').trim() ?? ''
  return Boolean(env.ARTIFACT_PROXY_TOKEN) && token === env.ARTIFACT_PROXY_TOKEN
}

async function handleArtifactRequest(request: Request, env: WorkerEnv): Promise<Response> {
  if (!env.ARTIFACTS || !env.ARTIFACT_PROXY_TOKEN) {
    return jsonResponse(500, 'Worker 尚未配置 ARTIFACTS 或 ARTIFACT_PROXY_TOKEN。')
  }

  if (!isAuthorizedArtifactRequest(request, env)) {
    return unauthorizedArtifactResponse()
  }

  const objectKey = readArtifactKeyFromPath(new URL(request.url).pathname)
  if (!objectKey) {
    return jsonResponse(400, '产物路径不能为空。')
  }

  if (request.method === 'GET') {
    const object = await env.ARTIFACTS.get(objectKey)
    if (!object) {
      return jsonResponse(404, `产物不存在: ${objectKey}`)
    }

    return new Response(await object.arrayBuffer(), {
      status: 200,
      headers: {
        'content-type': object.httpMetadata?.contentType ?? 'application/octet-stream',
      },
    })
  }

  if (request.method === 'PUT') {
    const body = await request.arrayBuffer()
    await env.ARTIFACTS.put(objectKey, body, {
      httpMetadata: {
        contentType: request.headers.get('content-type') ?? 'application/octet-stream',
      },
    })

    return new Response(null, { status: 204 })
  }

  if (request.method === 'DELETE') {
    await env.ARTIFACTS.delete(objectKey)
    return new Response(null, { status: 204 })
  }

  return jsonResponse(405, `不支持的产物方法: ${request.method}`)
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
  if (url.pathname.startsWith('/__classflow/artifacts/')) {
    return handleArtifactRequest(request, env)
  }

  if (url.pathname.startsWith('/api/')) {
    return proxyApiRequest(request, env)
  }

  return env.ASSETS.fetch(request)
}
