/**
 * ClassFlow 朋友分享站的独立 Worker 入口。
 *
 * 安全边界：
 * 1. 这个 Worker 没有 BACKEND_TOKEN、R2 私有写入口、SmartClass 或 Archive 配置。
 * 2. 只提供 public notes 与静态前端资源。
 * 3. 管理 API 与私有产物入口明确 404，避免朋友站意外演化成管理端旁路。
 *
 * 根路径重定向到 /share，让同一份 React bundle 自动进入 SharePage。
 */

import {
  handlePublicNotesRequest,
  type AssetBindingLike,
  type PublicNotesEnv,
} from './proxy'

interface ShareWorkerEnv extends PublicNotesEnv {
  ASSETS: AssetBindingLike
}

function notFound(): Response {
  return new Response('Not Found', {
    status: 404,
    headers: {
      'content-type': 'text/plain; charset=utf-8',
    },
  })
}

export async function handleShareWorkerRequest(
  request: Request,
  env: ShareWorkerEnv,
): Promise<Response> {
  const publicNotesResponse = await handlePublicNotesRequest(request, env)
  if (publicNotesResponse) {
    return publicNotesResponse
  }

  const url = new URL(request.url)
  if (url.pathname.startsWith('/api/') || url.pathname.startsWith('/__classflow/')) {
    return notFound()
  }

  if (url.pathname === '/') {
    return Response.redirect(new URL('/share', request.url).toString(), 302)
  }

  if (url.pathname === '/share') {
    const root = new URL('/', request.url)
    return env.ASSETS.fetch(
      new Request(root.toString(), {
        method: 'GET',
        headers: request.headers,
      }),
    )
  }

  return env.ASSETS.fetch(request)
}

export default {
  async fetch(request: Request, env: ShareWorkerEnv) {
    return handleShareWorkerRequest(request, env)
  },
}
