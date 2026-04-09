/**
 * Worker 代理的核心逻辑。
 *
 * 约束很明确：
 *
 * 1. 浏览器只能访问 Worker，自身不持有后端密钥。
 * 2. `/api/*` 请求由 Worker 自动追加 Bearer Token；若后端 Tunnel 还接入了
 *    Cloudflare Access，则再额外追加 Service Token 请求头后转发到后端。
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
  CF_ACCESS_CLIENT_ID?: string
  CF_ACCESS_CLIENT_SECRET?: string
  ARTIFACT_PROXY_TOKEN?: string
  ARTIFACTS?: R2BucketLike
  ASSETS: AssetBindingLike
}

interface TaskArtifactLookupResponse {
  task: {
    segment_markdown_path?: string | null
    segment_json_path?: string | null
  }
}

interface CourseArtifactLookupResponse {
  merged_markdown_path?: string | null
  manifest_path?: string | null
}

function jsonResponse(status: number, error: string) {
  return new Response(JSON.stringify({ error }), {
    status,
    headers: {
      'content-type': 'application/json; charset=utf-8',
    },
  })
}

function textResponse(status: number, contentType: string, body: string) {
  return new Response(body, {
    status,
    headers: {
      'content-type': contentType,
    },
  })
}

function unauthorizedArtifactResponse() {
  return jsonResponse(401, '未通过 Worker 产物接口鉴权。')
}

/**
 * 统一组装“Worker -> 后端”请求的鉴权头。
 *
 * 这里把两层鉴权放在一起处理：
 *
 * 1. `Authorization` 负责应用自己的 Bearer Token。
 * 2. `CF-Access-*` 负责通过 Cloudflare Access 保护后的 Tunnel 域名。
 * 3. Access 的 `Client ID / Secret` 必须成对出现，防止半配置造成难排查的 401。
 */
function buildBackendAuthHeaders(env: WorkerEnv, baseHeaders?: HeadersInit): Headers | Response {
  if (!env.BACKEND_BASE_URL || !env.BACKEND_TOKEN) {
    return jsonResponse(500, 'Worker 尚未配置 BACKEND_BASE_URL 或 BACKEND_TOKEN。')
  }

  const headers = new Headers(baseHeaders)
  headers.set('Authorization', `Bearer ${env.BACKEND_TOKEN}`)
  headers.delete('host')

  const hasAccessClientId = Boolean(env.CF_ACCESS_CLIENT_ID?.trim())
  const hasAccessClientSecret = Boolean(env.CF_ACCESS_CLIENT_SECRET?.trim())
  if (hasAccessClientId !== hasAccessClientSecret) {
    return jsonResponse(
      500,
      'Worker 的 Cloudflare Access Service Token 配置不完整，CF_ACCESS_CLIENT_ID 与 CF_ACCESS_CLIENT_SECRET 必须同时存在。',
    )
  }

  if (hasAccessClientId && hasAccessClientSecret) {
    headers.set('CF-Access-Client-Id', env.CF_ACCESS_CLIENT_ID!.trim())
    headers.set('CF-Access-Client-Secret', env.CF_ACCESS_CLIENT_SECRET!.trim())
  }

  return headers
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

async function fetchBackendJson<T>(pathnameWithQuery: string, env: WorkerEnv): Promise<T | Response> {
  const backendUrl = new URL(pathnameWithQuery, env.BACKEND_BASE_URL)
  const headers = buildBackendAuthHeaders(env)
  if (headers instanceof Response) {
    return headers
  }

  const response = await fetch(backendUrl.toString(), {
    method: 'GET',
    headers,
    redirect: 'manual',
  })

  if (response.status === 401) {
    return jsonResponse(502, '后端鉴权失败，请检查 Worker 中配置的 BACKEND_TOKEN。')
  }

  if (response.status >= 500) {
    return jsonResponse(502, '后端服务异常，请稍后重试。')
  }

  if (!response.ok) {
    return textResponse(
      response.status,
      response.headers.get('content-type') ?? 'application/json; charset=utf-8',
      await response.text(),
    )
  }

  return (await response.json()) as T
}

async function buildStoredObjectResponse(object: R2ObjectLike): Promise<Response> {
  return new Response(await object.arrayBuffer(), {
    status: 200,
    headers: {
      'content-type': object.httpMetadata?.contentType ?? 'application/octet-stream',
    },
  })
}

async function tryHandlePublicArtifactRequest(request: Request, env: WorkerEnv): Promise<Response | null> {
  if (request.method !== 'GET' || !env.ARTIFACTS) {
    return null
  }

  const { pathname } = new URL(request.url)
  const taskArtifactMatch = pathname.match(/^\/api\/v1\/tasks\/([^/]+)\/artifacts\/([^/]+)$/)
  if (taskArtifactMatch) {
    const [, taskId, artifactName] = taskArtifactMatch
    if (artifactName !== 'segment.md' && artifactName !== 'segment.json') {
      return null
    }

    const lookup = await fetchBackendJson<TaskArtifactLookupResponse>(`/api/v1/tasks/${taskId}`, env)
    if (lookup instanceof Response) {
      return lookup
    }

    const objectKey =
      artifactName === 'segment.md' ? lookup.task.segment_markdown_path : lookup.task.segment_json_path
    if (!objectKey) {
      return jsonResponse(404, `任务产物尚未生成: ${artifactName}`)
    }

    const object = await env.ARTIFACTS.get(objectKey)
    if (!object) {
      return jsonResponse(404, `产物不存在: ${objectKey}`)
    }

    return buildStoredObjectResponse(object)
  }

  const courseArtifactMatch = pathname.match(/^\/api\/v1\/courses\/([^/]+)\/artifacts\/([^/]+)$/)
  if (!courseArtifactMatch) {
    return null
  }

  const [, courseKey, artifactName] = courseArtifactMatch
  if (artifactName !== 'course.md' && artifactName !== 'manifest.json') {
    return null
  }

  const lookup = await fetchBackendJson<CourseArtifactLookupResponse>(`/api/v1/courses/${courseKey}`, env)
  if (lookup instanceof Response) {
    return lookup
  }

  const objectKey = artifactName === 'course.md' ? lookup.merged_markdown_path : lookup.manifest_path
  if (!objectKey) {
    return jsonResponse(404, `课程产物尚未生成: ${artifactName}`)
  }

  const object = await env.ARTIFACTS.get(objectKey)
  if (!object) {
    return jsonResponse(404, `产物不存在: ${objectKey}`)
  }

  return buildStoredObjectResponse(object)
}

export async function proxyApiRequest(request: Request, env: WorkerEnv): Promise<Response> {
  const incomingUrl = new URL(request.url)
  const backendUrl = new URL(`${incomingUrl.pathname}${incomingUrl.search}`, env.BACKEND_BASE_URL)

  const headers = buildBackendAuthHeaders(env, request.headers)
  if (headers instanceof Response) {
    return headers
  }

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

  const publicArtifactResponse = await tryHandlePublicArtifactRequest(request, env)
  if (publicArtifactResponse) {
    return publicArtifactResponse
  }

  if (url.pathname.startsWith('/api/')) {
    return proxyApiRequest(request, env)
  }

  return env.ASSETS.fetch(request)
}
