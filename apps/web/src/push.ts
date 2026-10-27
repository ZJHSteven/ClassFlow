/**
 * 浏览器侧 ClassFlow Web Push 客户端。
 *
 * iOS 的关键边界：
 * - Safari 标签页不能接收 Home Screen Web Push，所以先判断是否 standalone。
 * - 系统通知权限必须由用户手势触发，因此真正 subscribe 只在“开启通知”按钮点击后执行。
 * - 页面本身无法可靠判断“同一网站是否已经另有一个主屏幕副本”，所以普通 Safari 只能
 *   判断“我现在不是 standalone”，不能把“是否已经安装”当成可查询系统状态。
 */
export interface PushPublicConfig {
  configured: boolean
  publicKey: string | null
}

export type PushState =
  | 'unsupported'
  | 'install-required'
  | 'server-unconfigured'
  | 'denied'
  | 'ready'
  | 'subscribed'

export interface PushSnapshot {
  state: PushState
  installed: boolean
  isIOS: boolean
  permission: NotificationPermission | 'unsupported'
  subscription: PushSubscription | null
  config: PushPublicConfig
}

export function isIOSLike(): boolean {
  const ua = navigator.userAgent
  return /iPad|iPhone|iPod/u.test(ua)
    || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1)
}

export function isStandalone(): boolean {
  const safariStandalone = Boolean((navigator as Navigator & { standalone?: boolean }).standalone)
  return safariStandalone || window.matchMedia('(display-mode: standalone)').matches
}

function supportsWebPush(): boolean {
  return (
    'serviceWorker' in navigator
    && 'PushManager' in window
    && 'Notification' in window
  )
}

function urlBase64ToBytes(value: string): Uint8Array<ArrayBuffer> {
  const padding = '='.repeat((4 - (value.length % 4)) % 4)
  const base64 = (value + padding).replace(/-/gu, '+').replace(/_/gu, '/')
  const raw = atob(base64)
  const bytes = new Uint8Array(raw.length)
  for (let index = 0; index < raw.length; index += 1) bytes[index] = raw.charCodeAt(index)
  return bytes
}

async function readJson<T>(response: Response): Promise<T> {
  const payload = await response.json().catch(() => null) as T | { error?: string } | null
  if (!response.ok) {
    const error = payload && typeof payload === 'object' && 'error' in payload
      ? payload.error
      : undefined
    throw new Error(error || `ClassFlow Push API HTTP ${response.status}`)
  }
  return payload as T
}

async function registration(): Promise<ServiceWorkerRegistration> {
  if (!('serviceWorker' in navigator)) throw new Error('当前浏览器不支持 Service Worker')
  return navigator.serviceWorker.ready
}

export async function readPushSnapshot(): Promise<PushSnapshot> {
  const config = await readJson<PushPublicConfig>(
    await fetch('/api/push/config', { cache: 'no-store' }),
  )
  const isIOS = isIOSLike()
  const installed = isStandalone()

  if (!supportsWebPush()) {
    return {
      state: isIOS && !installed ? 'install-required' : 'unsupported',
      installed,
      isIOS,
      permission: 'unsupported',
      subscription: null,
      config,
    }
  }

  if (isIOS && !installed) {
    return {
      state: 'install-required',
      installed,
      isIOS,
      permission: Notification.permission,
      subscription: null,
      config,
    }
  }

  const sw = await registration()
  const subscription = await sw.pushManager.getSubscription()
  const permission = Notification.permission
  const state: PushState = !config.configured
    ? 'server-unconfigured'
    : subscription
      ? 'subscribed'
      : permission === 'denied'
        ? 'denied'
        : 'ready'

  return { state, installed, isIOS, permission, subscription, config }
}

export async function enablePush(config: PushPublicConfig): Promise<PushSubscription> {
  if (!config.configured || !config.publicKey) throw new Error('服务器尚未配置 Web Push VAPID')

  // Notification.requestPermission 必须直接处于按钮点击链中，尤其是 iOS Web App。
  const permission = await Notification.requestPermission()
  if (permission !== 'granted') throw new Error('系统没有授予 ClassFlow 通知权限')

  const sw = await registration()
  const existing = await sw.pushManager.getSubscription()
  const subscription = existing ?? await sw.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: urlBase64ToBytes(config.publicKey),
  })
  const serialized = subscription.toJSON()
  if (!serialized.endpoint || !serialized.keys?.p256dh || !serialized.keys?.auth) {
    throw new Error('浏览器返回的 PushSubscription 不完整')
  }

  await readJson(await fetch('/api/push/subscriptions', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      subscription: {
        endpoint: serialized.endpoint,
        expirationTime: serialized.expirationTime ?? null,
        keys: {
          p256dh: serialized.keys.p256dh,
          auth: serialized.keys.auth,
        },
      },
    }),
  }))

  // 首次启用后立即做一条端到端测试 Push。测试失败不能撤销已经成功建立的订阅：
  // 后续真实课堂事件仍会继续使用同一 endpoint，且服务端 outbox 自己有独立重试。
  // 这样用户不需要再回到设置页找“测试通知”按钮，也不用另开一轮人工验收。
  try {
    await readJson(await fetch('/api/push/test', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ endpoint: serialized.endpoint }),
    }))
  }
  catch (error) {
    console.warn('ClassFlow Web Push 已订阅，但首次测试通知发送失败', error)
  }

  return subscription
}

export async function clearAppBadge(): Promise<void> {
  const badgeNavigator = navigator as Navigator & { clearAppBadge?: () => Promise<void> }
  if (typeof badgeNavigator.clearAppBadge === 'function') {
    await badgeNavigator.clearAppBadge().catch(() => undefined)
  }
}
