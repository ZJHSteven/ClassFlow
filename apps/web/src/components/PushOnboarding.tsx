/**
 * ClassFlow 的“零常驻控件”安装 / 通知引导。
 *
 * 用户不希望在紧张的任务台常驻一个铃铛按钮，所以这里只在确实缺一步时自动出现：
 * - iOS Safari 标签页：提示添加到主屏幕；
 * - 已从主屏幕启动、但还没订阅：提示开启通知；
 * - 已订阅：完全不渲染。
 *
 * 关闭只记在当前 sessionStorage；下一次重新从 Safari 打开仍会提醒。这是因为 iOS 没有
 * 网站可读取的“已安装主屏幕副本列表”，无法可靠做到 Safari 永久知道另一个副本存在。
 */
import { useEffect, useState } from 'react'

import { enablePush, readPushSnapshot, type PushSnapshot } from '../push'

const DISMISS_KEY = 'classflow:onboarding-dismissed'

export function PushOnboarding() {
  const [snapshot, setSnapshot] = useState<PushSnapshot | null>(null)
  const [dismissed, setDismissed] = useState(
    () => sessionStorage.getItem(DISMISS_KEY) === '1',
  )
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  useEffect(() => {
    void readPushSnapshot()
      .then(setSnapshot)
      .catch((value) => setError(value instanceof Error ? value.message : String(value)))
  }, [])

  if (dismissed || !snapshot || snapshot.state === 'subscribed') return null

  // 桌面浏览器不是本轮目标；ClassFlow iOS 主屏幕体验只在 iOS 上主动打扰一次。
  if (!snapshot.isIOS) return null

  const dismiss = () => {
    sessionStorage.setItem(DISMISS_KEY, '1')
    setDismissed(true)
  }

  const enable = async () => {
    setBusy(true)
    setError(undefined)
    try {
      await enablePush(snapshot.config)
      setSnapshot(await readPushSnapshot())
    }
    catch (value) {
      setError(value instanceof Error ? value.message : String(value))
    }
    finally {
      setBusy(false)
    }
  }

  const installRequired = snapshot.state === 'install-required'
  const denied = snapshot.state === 'denied'
  const serverUnconfigured = snapshot.state === 'server-unconfigured'

  return (
    <div className="pushOnboardingBackdrop" role="presentation">
      <section className="pushOnboarding" role="dialog" aria-modal="true" aria-label="ClassFlow 设置">
        <img className="pushOnboarding__logo" src="/app-icon.svg" alt="" />
        <div className="pushOnboarding__copy">
          <p className="pushOnboarding__eyebrow">CLASSFLOW FOR IOS</p>
          <h2>
            {installRequired
              ? '把 ClassFlow 放到主屏幕'
              : denied
                ? 'ClassFlow 通知被系统关闭'
                : serverUnconfigured
                  ? '通知服务正在配置'
                  : '开启 ClassFlow 通知'}
          </h2>
          {installRequired ? (
            <p>
              点 Safari 的<strong>分享</strong>按钮，选择<strong>“添加到主屏幕”</strong>。
              以后直接点 ClassFlow 图标，就能打开任务台和课程库，并启用 iOS Web Push。
            </p>
          ) : denied ? (
            <p>
              需要到 iOS 的 ClassFlow 通知设置里重新允许通知；网页不能绕过系统已经拒绝的权限。
            </p>
          ) : serverUnconfigured ? (
            <p>主屏幕 App 已经就绪，服务器端 Web Push 密钥还未完成配置。</p>
          ) : (
            <p>
              只推送<strong>流水线真正失败</strong>和<strong>整堂课堂笔记完成</strong>，
              不再用每个内部阶段刷屏。开启后会立即收到一条测试通知，用来确认 iOS 链路已经接通。
            </p>
          )}
          {error ? <p className="pushOnboarding__error">{error}</p> : null}
        </div>
        <div className="pushOnboarding__actions">
          {!installRequired && !denied && !serverUnconfigured ? (
            <button type="button" className="pushOnboarding__primary" onClick={enable} disabled={busy}>
              {busy ? '正在开启…' : '开启通知'}
            </button>
          ) : null}
          <button type="button" className="pushOnboarding__secondary" onClick={dismiss}>
            稍后
          </button>
        </div>
      </section>
    </div>
  )
}
