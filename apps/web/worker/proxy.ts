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

export interface PublicNotesEnv {
  NOTES_PUBLIC_REPO?: string
}

/** Cloudflare Email Service send_email binding 的最小能力面。 */
export interface SendEmailBindingLike {
  send(message: {
    to: string
    from: string
    subject: string
    text: string
    html: string
  }): Promise<{ messageId?: string }>
}

export interface WorkerEnv extends PublicNotesEnv {
  BACKEND_BASE_URL: string
  BACKEND_TOKEN: string
  CF_ACCESS_CLIENT_ID?: string
  CF_ACCESS_CLIENT_SECRET?: string
  ARTIFACT_PROXY_TOKEN?: string
  ARTIFACTS?: R2BucketLike
  SUMMARY_EMAIL?: SendEmailBindingLike
  SUMMARY_EMAIL_TO?: string
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

interface SummaryEmailPayload {
  lecture_key: string
  course_name: string
  teacher_name: string
  date: string
  start_time: string
  end_time: string
  title: string
  summary: string
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === 'string' && value.trim().length > 0
}

function validateSummaryEmailPayload(value: unknown): SummaryEmailPayload | null {
  if (!value || typeof value !== 'object') {
    return null
  }
  const payload = value as Partial<SummaryEmailPayload>
  const keys: Array<keyof SummaryEmailPayload> = [
    'lecture_key',
    'course_name',
    'teacher_name',
    'date',
    'start_time',
    'end_time',
    'title',
    'summary',
  ]
  if (!keys.every((key) => isNonEmptyString(payload[key]))) {
    return null
  }
  if (payload.summary!.length > 8_000) {
    return null
  }
  return payload as SummaryEmailPayload
}

function stripHeaderNewlines(value: string): string {
  return value.replace(/[\r\n]+/g, ' ').trim()
}

function escapeHtml(value: string): string {
  return value
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;')
}

function summaryEmailText(payload: SummaryEmailPayload): string {
  return [
    payload.course_name + '｜' + payload.title,
    payload.date + ' ' + payload.start_time + '-' + payload.end_time + '｜' + payload.teacher_name,
    '',
    payload.summary.trim(),
  ].join('\n')
}

function summaryEmailHtml(payload: SummaryEmailPayload): string {
  const summary = escapeHtml(payload.summary.trim()).replaceAll('\n', '<br>')
  return [
    '<main style="font-family:-apple-system,BlinkMacSystemFont,Segoe UI,sans-serif;line-height:1.65;max-width:760px;margin:auto">',
    '<h2>' + escapeHtml(payload.course_name) + '｜' + escapeHtml(payload.title) + '</h2>',
    '<p style="color:#666">' + escapeHtml(payload.date) + ' ' + escapeHtml(payload.start_time) + '-' + escapeHtml(payload.end_time) + ' · ' + escapeHtml(payload.teacher_name) + '</p>',
    '<div>' + summary + '</div>',
    '</main>',
  ].join('')
}

async function handleSummaryEmailRequest(request: Request, env: WorkerEnv): Promise<Response> {
  if (request.method !== 'POST') {
    return jsonResponse(405, '课堂摘要邮件接口只接受 POST。')
  }
  const token = request.headers.get('Authorization')?.replace(/^Bearer\s+/i, '').trim() ?? ''
  if (!env.BACKEND_TOKEN || token !== env.BACKEND_TOKEN) {
    return jsonResponse(401, '未通过 ClassFlow backend 鉴权。')
  }
  if (!env.SUMMARY_EMAIL || !env.SUMMARY_EMAIL_TO?.trim()) {
    return jsonResponse(500, 'Worker 尚未配置 SUMMARY_EMAIL 或 SUMMARY_EMAIL_TO。')
  }

  const contentLength = Number(request.headers.get('content-length') ?? '0')
  if (Number.isFinite(contentLength) && contentLength > 32_000) {
    return jsonResponse(413, '课堂摘要邮件请求过大。')
  }
  let raw: string
  try {
    raw = await request.text()
  } catch {
    return jsonResponse(400, '无法读取课堂摘要邮件请求。')
  }
  if (raw.length > 32_000) {
    return jsonResponse(413, '课堂摘要邮件请求过大。')
  }

  let parsed: unknown
  try {
    parsed = JSON.parse(raw)
  } catch {
    return jsonResponse(400, '课堂摘要邮件请求不是合法 JSON。')
  }
  const payload = validateSummaryEmailPayload(parsed)
  if (!payload) {
    return jsonResponse(400, '课堂摘要邮件缺少必要字段或内容过长。')
  }

  const subject = stripHeaderNewlines('ClassFlow｜' + payload.course_name + '｜' + payload.title).slice(0, 180)
  try {
    const result = await env.SUMMARY_EMAIL.send({
      to: env.SUMMARY_EMAIL_TO.trim(),
      from: 'classflow@zjhstudio.com',
      subject,
      text: summaryEmailText(payload),
      html: summaryEmailHtml(payload),
    })
    return new Response(JSON.stringify({ ok: true, message_id: result.messageId ?? null }), {
      status: 200,
      headers: { 'content-type': 'application/json; charset=utf-8' },
    })
  } catch (error) {
    console.error('ClassFlow summary email send failed', {
      lecture_key: payload.lecture_key,
      error: error instanceof Error ? error.message : String(error),
    })
    return jsonResponse(502, 'Cloudflare Email Service 投递失败。')
  }
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


interface GithubTreeEntry {
  path?: string
  type?: string
}

interface GithubTreeResponse {
  tree?: GithubTreeEntry[]
}

const DEFAULT_PUBLIC_NOTES_REPO = 'ZJHSteven/med-notes-2026-fall-y3'
const PUBLIC_NOTES_FETCH_ATTEMPTS = 3
const PUBLIC_NOTES_RETRY_DELAYS_MS = [80, 200]

function publicNotesRepo(env: PublicNotesEnv): string {
  return env.NOTES_PUBLIC_REPO?.trim() || DEFAULT_PUBLIC_NOTES_REPO
}

function githubPublicHeaders(): Headers {
  const headers = new Headers()
  headers.set('Accept', 'application/vnd.github+json')
  headers.set('User-Agent', 'ClassFlow-Public-Notes')
  headers.set('X-GitHub-Api-Version', '2022-11-28')
  return headers
}

/**
 * GitHub 公共接口偶尔会从边缘节点返回 429/5xx，公开分享页不能因为一次瞬时失败就直接变成空白。
 * 这里只重试明确属于瞬时故障的状态；404 等确定性结果必须立即返回。
 */
async function fetchPublicGithubWithRetry(
  input: string,
  init: RequestInit,
): Promise<Response | null> {
  let lastTransientResponse: Response | null = null

  for (let attempt = 0; attempt < PUBLIC_NOTES_FETCH_ATTEMPTS; attempt += 1) {
    try {
      const response = await fetch(input, init)
      const transientFailure = response.status === 429 || response.status >= 500
      if (!transientFailure) {
        return response
      }
      lastTransientResponse = response
    } catch {
      // 网络异常没有可复用 Response；继续进入同一套短退避重试。
    }

    const delayMs = PUBLIC_NOTES_RETRY_DELAYS_MS[attempt]
    if (delayMs !== undefined) {
      await new Promise((resolve) => setTimeout(resolve, delayMs))
    }
  }

  return lastTransientResponse
}

/**
 * 显式使用 Cloudflare Cache API 缓存成功的公开 tree/Markdown，减少每次页面打开都回源 GitHub。
 * Vitest 等非 Cloudflare 环境没有 `caches.default` 时自动退化为无缓存。
 */
function publicNotesCache(): Cache | null {
  if (typeof caches === 'undefined') {
    return null
  }
  const cloudflareCaches = caches as CacheStorage & { default?: Cache }
  return cloudflareCaches.default ?? null
}

async function readPublicNotesCache(request: Request): Promise<Response | null> {
  const cache = publicNotesCache()
  if (!cache) {
    return null
  }
  try {
    return (await cache.match(request)) ?? null
  } catch {
    return null
  }
}

async function writePublicNotesCache(request: Request, response: Response): Promise<void> {
  if (response.status !== 200) {
    return
  }
  const cache = publicNotesCache()
  if (!cache) {
    return
  }
  try {
    await cache.put(request, response.clone())
  } catch {
    // 缓存只是可靠性优化；缓存写入失败不能影响用户读取公开资料。
  }
}

export async function handlePublicNotesRequest(request: Request, env: PublicNotesEnv): Promise<Response | null> {
  if (request.method !== 'GET') {
    return null
  }

  const url = new URL(request.url)
  const repo = publicNotesRepo(env)

  if (url.pathname === '/public/notes/tree') {
    const cached = await readPublicNotesCache(request)
    if (cached) {
      return cached
    }

    const response = await fetchPublicGithubWithRetry('https://api.github.com/repos/' + repo + '/git/trees/main?recursive=1', {
      headers: githubPublicHeaders(),
    })
    if (!response?.ok) {
      return jsonResponse(502, '公开笔记仓库目录读取失败。')
    }

    const payload = (await response.json()) as GithubTreeResponse
    const files = (payload.tree || [])
      .filter((entry) => entry.type === 'blob' && typeof entry.path === 'string')
      .map((entry) => entry.path as string)
      .filter((path) => /^[^/]+\/(Audio|Notes)\/[^/]+\.md$/i.test(path))
      .map((path) => {
        const [course, kind, name] = path.split('/')
        return { path, course, kind, name }
      })

    const result = new Response(JSON.stringify({ repo, files }), {
      status: 200,
      headers: {
        'content-type': 'application/json; charset=utf-8',
        'cache-control': 'public, max-age=60',
      },
    })
    await writePublicNotesCache(request, result)
    return result
  }

  if (url.pathname === '/public/notes/file') {
    const path = url.searchParams.get('path') || ''
    if (!/^[^/]+\/(Audio|Notes)\/[^/]+\.md$/i.test(path) || path.includes('..')) {
      return jsonResponse(400, '公开 Markdown 路径不合法。')
    }

    const cached = await readPublicNotesCache(request)
    if (cached) {
      return cached
    }

    const encodedPath = path.split('/').map((part) => encodeURIComponent(part)).join('/')
    const response = await fetchPublicGithubWithRetry('https://raw.githubusercontent.com/' + repo + '/main/' + encodedPath, {
      headers: { 'User-Agent': 'ClassFlow-Public-Notes' },
    })
    if (response?.status === 404) {
      return jsonResponse(404, '共享资料不存在。')
    }
    if (!response?.ok) {
      return jsonResponse(502, '共享 Markdown 读取失败。')
    }

    const result = new Response(await response.text(), {
      status: 200,
      headers: {
        'content-type': 'text/markdown; charset=utf-8',
        'cache-control': 'public, max-age=60',
      },
    })
    await writePublicNotesCache(request, result)
    return result
  }

  return null
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

  const publicNotesResponse = await handlePublicNotesRequest(request, env)
  if (publicNotesResponse) {
    return publicNotesResponse
  }

  if (url.pathname === '/share') {
    const rootUrl = new URL('/', request.url)
    return env.ASSETS.fetch(new Request(rootUrl.toString(), { method: 'GET', headers: request.headers }))
  }
  if (url.pathname.startsWith('/__classflow/artifacts/')) {
    return handleArtifactRequest(request, env)
  }
  if (url.pathname === '/__classflow/email-summary') {
    return handleSummaryEmailRequest(request, env)
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
