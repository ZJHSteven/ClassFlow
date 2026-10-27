/*
 * ClassFlow 根作用域 Service Worker。
 *
 * 当前只承担 Web Push 与点击回跳，不做离线缓存。控制台的数据高度实时，贸然缓存 HTML/API
 * 反而容易把“旧任务状态”伪装成在线结果，因此这里刻意保持很薄。
 */
self.addEventListener('push', (event) => {
  let payload = null
  try {
    payload = event.data ? event.data.json() : null
  }
  catch {
    payload = null
  }

  const notification = payload && typeof payload === 'object' ? payload.notification : null
  const title = notification && typeof notification.title === 'string'
    ? notification.title
    : 'ClassFlow'
  const navigate = notification && typeof notification.navigate === 'string'
    ? notification.navigate
    : self.registration.scope

  const tasks = [
    self.registration.showNotification(title, {
      body: notification && typeof notification.body === 'string'
        ? notification.body
        : 'ClassFlow 有新的课堂流水线状态',
      tag: notification && typeof notification.tag === 'string'
        ? notification.tag
        : undefined,
      icon: '/app-icon-192.png?v=3',
      badge: '/app-icon-192.png?v=3',
      data: { navigate },
    }),
  ]

  // iOS Home Screen Web App 支持 Badging 时，先用 1 表示“有新状态”。
  // 打开 ClassFlow 后 main.tsx 会清掉 badge。
  if (self.navigator && typeof self.navigator.setAppBadge === 'function') {
    tasks.push(self.navigator.setAppBadge(1))
  }

  event.waitUntil(Promise.all(tasks))
})

self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  const navigate = event.notification.data && typeof event.notification.data.navigate === 'string'
    ? event.notification.data.navigate
    : self.registration.scope

  event.waitUntil((async () => {
    const targetUrl = new URL(navigate, self.registration.scope).toString()
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true })

    for (const client of windows) {
      if ('focus' in client) {
        if ('navigate' in client) await client.navigate(targetUrl)
        return client.focus()
      }
    }
    return self.clients.openWindow(targetUrl)
  })())
})
