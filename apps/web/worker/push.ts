/**
 * ClassFlow Web Push 服务。
 *
 * 设计目标：
 * 1. 浏览器只负责登记标准 PushSubscription，不接触 VAPID 私钥。
 * 2. ClassFlow backend 继续使用现有 stage_events outbox，并把高信号事件 POST 到 Worker。
 * 3. Worker 使用现有 ARTIFACTS R2 保存少量 subscription，避免为了单用户通知再引入一套 D1。
 * 4. 失效 endpoint（404/410）会自动从 R2 删除，避免后续持续报错。
 *
 * 当前是单用户自用系统，因此一个很小的 JSON 文档足够；如果未来变成多人产品，再迁移到
 * D1/Queues 会更合适。
 */
import webpush from 'web-push'

import type { R2BucketLike } from './proxy'

const SUBSCRIPTIONS_KEY = '_system/web-push/subscriptions.v1.json'

export interface PushEnv {
  ARTIFACTS?: R2BucketLike
  VAPID_PUBLIC_KEY?: string
  VAPID_PRIVATE_KEY?: string
  VAPID_SUBJECT?: string
  CLASSFLOW_PUBLIC_ORIGIN?: string
}

export interface BrowserPushSubscriptionInput {
  endpoint: string
  expirationTime?: number | null
  keys: {
    p256dh: string
    auth: string
  }
}

interface StoredPushSubscription extends BrowserPushSubscriptionInput {
  id: string
  userAgent: string | null
  createdAt: string
  updatedAt: string
}

interface SubscriptionStore {
  version: 1
  subscriptions: StoredPushSubscription[]
}

export interface StageEventWebhookPayload {
  event_key: string
  source_kind: string
  source_id: string
  stage: string
  outcome: string
  title: string
  message: string
  payload: Record<string, unknown>
  created_at: string
}

interface DeclarativePushPayload {
  web_push: 8030
  app_badge: '1'
  notification: {
    title: string
    body: string
    navigate: string
    silent: false
    tag: string
  }
}

function requireStore(env: PushEnv): R2BucketLike {
  if (!env.ARTIFACTS) throw new Error('ClassFlow Worker 未绑定 ARTIFACTS R2')
  return env.ARTIFACTS
}

function requirePushConfig(env: PushEnv) {
  const publicKey = env.VAPID_PUBLIC_KEY?.trim()
  const privateKey = env.VAPID_PRIVATE_KEY?.trim()
  const subject = env.VAPID_SUBJECT?.trim() || 'https://classflow.zjhstudio.com'
  const publicOrigin = (env.CLASSFLOW_PUBLIC_ORIGIN?.trim() || 'https://classflow.zjhstudio.com')
    .replace(/\/+$/u, '')
  if (!publicKey || !privateKey) throw new Error('ClassFlow Web Push 尚未配置 VAPID')
  return { publicKey, privateKey, subject, publicOrigin }
}

export function getPushPublicConfig(env: PushEnv) {
  return {
    configured: Boolean(env.VAPID_PUBLIC_KEY?.trim() && env.VAPID_PRIVATE_KEY?.trim()),
    publicKey: env.VAPID_PUBLIC_KEY?.trim() ?? null,
  }
}

function assertSubscription(input: BrowserPushSubscriptionInput): void {
  const endpoint = new URL(input.endpoint)
  if (endpoint.protocol !== 'https:') throw new Error('Web Push endpoint 必须使用 HTTPS')
  if (endpoint.username || endpoint.password) throw new Error('Web Push endpoint 不能包含用户凭据')
  if (!input.keys?.p256dh?.trim() || !input.keys?.auth?.trim()) {
    throw new Error('Web Push subscription 缺少 p256dh/auth')
  }
}

async function idForEndpoint(endpoint: string): Promise<string> {
  const digest = new Uint8Array(
    await crypto.subtle.digest('SHA-256', new TextEncoder().encode(endpoint)),
  )
  return `push-${[...digest].map((value) => value.toString(16).padStart(2, '0')).join('')}`
}

async function readStore(env: PushEnv): Promise<SubscriptionStore> {
  const object = await requireStore(env).get(SUBSCRIPTIONS_KEY)
  if (!object) return { version: 1, subscriptions: [] }

  try {
    const raw = new TextDecoder().decode(await object.arrayBuffer())
    const parsed = JSON.parse(raw) as Partial<SubscriptionStore>
    if (parsed.version !== 1 || !Array.isArray(parsed.subscriptions)) {
      return { version: 1, subscriptions: [] }
    }
    return {
      version: 1,
      subscriptions: parsed.subscriptions.filter((item): item is StoredPushSubscription =>
        Boolean(item && typeof item.endpoint === 'string' && item.keys?.p256dh && item.keys?.auth),
      ),
    }
  }
  catch {
    // 损坏的订阅文件不能让整个 ClassFlow Worker 失效；返回空集合后由下一次登记重建。
    return { version: 1, subscriptions: [] }
  }
}

async function writeStore(env: PushEnv, store: SubscriptionStore): Promise<void> {
  const bytes = new TextEncoder().encode(JSON.stringify(store))
  await requireStore(env).put(SUBSCRIPTIONS_KEY, bytes.buffer, {
    httpMetadata: { contentType: 'application/json; charset=utf-8' },
  })
}

export async function upsertPushSubscription(
  env: PushEnv,
  input: BrowserPushSubscriptionInput,
  userAgent?: string | null,
) {
  assertSubscription(input)
  const store = await readStore(env)
  const now = new Date().toISOString()
  const existing = store.subscriptions.find((item) => item.endpoint === input.endpoint)
  const id = existing?.id ?? await idForEndpoint(input.endpoint)
  const record: StoredPushSubscription = {
    ...input,
    id,
    userAgent: userAgent?.slice(0, 512) ?? existing?.userAgent ?? null,
    createdAt: existing?.createdAt ?? now,
    updatedAt: now,
  }

  store.subscriptions = [
    ...store.subscriptions.filter((item) => item.endpoint !== input.endpoint),
    record,
  ]
  await writeStore(env, store)
  return { id, subscriptionCount: store.subscriptions.length }
}

export async function deletePushSubscription(env: PushEnv, endpoint: string) {
  const store = await readStore(env)
  const next = store.subscriptions.filter((item) => item.endpoint !== endpoint)
  const deleted = next.length !== store.subscriptions.length
  if (deleted) {
    store.subscriptions = next
    await writeStore(env, store)
  }
  return { deleted, subscriptionCount: next.length }
}

function asWebPushSubscription(record: StoredPushSubscription): webpush.PushSubscription {
  return {
    endpoint: record.endpoint,
    keys: {
      p256dh: record.keys.p256dh,
      auth: record.keys.auth,
    },
  }
}

function trimText(value: string, maxLength: number): string {
  const normalized = value.replace(/\s+/gu, ' ').trim()
  if (normalized.length <= maxLength) return normalized
  return normalized.slice(0, Math.max(0, maxLength - 1)).trimEnd() + '…'
}

export function buildStagePushPayload(
  event: StageEventWebhookPayload,
  publicOrigin: string,
): DeclarativePushPayload {
  const isNotesSuccess =
    event.source_kind === 'notes' && event.stage === 'notes' && event.outcome === 'succeeded'
  const isFailure = event.outcome === 'failed'

  const title = isNotesSuccess
    ? 'ClassFlow · 课堂笔记完成'
    : isFailure
      ? 'ClassFlow · 流水线异常'
      : 'ClassFlow'

  const body = isNotesSuccess
    ? `${event.title} · ${event.message}`
    : `${event.title} · ${trimText(event.message, 150)}`

  const navigate = new URL(publicOrigin + '/')
  navigate.searchParams.set('source', 'push')
  if (event.source_kind === 'task') navigate.searchParams.set('task', event.source_id)

  return {
    web_push: 8030,
    app_badge: '1',
    notification: {
      title,
      body: trimText(body, 180),
      navigate: navigate.toString(),
      silent: false,
      tag: `classflow-${event.event_key}`,
    },
  }
}

function isHighSignalEvent(event: StageEventWebhookPayload): boolean {
  return event.outcome === 'failed'
    || (
      event.source_kind === 'notes'
      && event.stage === 'notes'
      && event.outcome === 'succeeded'
    )
}

async function sendPayload(
  record: StoredPushSubscription,
  payload: DeclarativePushPayload,
  env: PushEnv,
) {
  const config = requirePushConfig(env)
  return webpush.sendNotification(
    asWebPushSubscription(record),
    JSON.stringify(payload),
    {
      TTL: 600,
      urgency: 'high',
      vapidDetails: {
        subject: config.subject,
        publicKey: config.publicKey,
        privateKey: config.privateKey,
      },
    },
  )
}

export async function dispatchStageEvent(env: PushEnv, event: StageEventWebhookPayload) {
  // Backend 也会做相同筛选；这里再守一道，避免未来某个旧 backend 把内部 stage 发到手机。
  if (!isHighSignalEvent(event)) {
    return { accepted: true, suppressed: true, subscriptions: 0, sent: 0, failed: 0, dead: 0 }
  }

  const config = requirePushConfig(env)
  const store = await readStore(env)
  const payload = buildStagePushPayload(event, config.publicOrigin)
  const summary = {
    accepted: true,
    suppressed: false,
    subscriptions: store.subscriptions.length,
    sent: 0,
    failed: 0,
    dead: 0,
  }
  const deadEndpoints = new Set<string>()

  for (const subscription of store.subscriptions) {
    try {
      await sendPayload(subscription, payload, env)
      summary.sent += 1
    }
    catch (error) {
      const statusCode = error instanceof webpush.WebPushError ? error.statusCode : 0
      if (statusCode === 404 || statusCode === 410) {
        deadEndpoints.add(subscription.endpoint)
        summary.dead += 1
      }
      else {
        summary.failed += 1
      }
    }
  }

  if (deadEndpoints.size > 0) {
    store.subscriptions = store.subscriptions.filter(
      (item) => !deadEndpoints.has(item.endpoint),
    )
    await writeStore(env, store)
  }

  // 只要还有活订阅但某台设备发送失败，就向 backend 返回失败，让现有 outbox 退避重试。
  if (summary.failed > 0) {
    throw new Error(`Web Push 临时失败：${summary.failed} 个 subscription`)
  }
  return summary
}

export async function sendTestPush(env: PushEnv, endpoint: string) {
  const store = await readStore(env)
  const subscription = store.subscriptions.find((item) => item.endpoint === endpoint)
  if (!subscription) throw new Error('当前浏览器的 Web Push subscription 尚未登记')
  const config = requirePushConfig(env)
  const now = new Date().toISOString()
  const payload = buildStagePushPayload({
    event_key: `push-test-${now}`,
    source_kind: 'notes',
    source_id: 'push-test',
    stage: 'notes',
    outcome: 'succeeded',
    title: 'ClassFlow 通知测试',
    message: 'iOS Web Push 已连接成功。',
    payload: {},
    created_at: now,
  }, config.publicOrigin)
  await sendPayload(subscription, payload, env)
  return { ok: true }
}
