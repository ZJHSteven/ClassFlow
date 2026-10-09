/**
 * SmartClass 会话状态与人工恢复卡。
 *
 * 长期采集完全运行在 N1 的纯 HTTP Session Manager 中；这个组件只承担两件事：
 *
 * 1. 告诉用户当前 Session 是健康、缺失还是已经过期。
 * 2. 真正需要人工介入时，提供“获取验证码 -> 输入凭据 -> 登录”的一次性入口。
 *
 * 安全约束：用户名、密码、验证码都只放 React 内存状态，不写任何 Web Storage。
 */

import { FormEvent, useCallback, useEffect, useState } from 'react'
import {
  getSmartClassAuthStatus,
  getSmartClassCaptcha,
  loginSmartClass,
  type SmartClassAuthStatus,
  type SmartClassCaptcha,
} from '../api'

export function SmartClassSessionCard() {
  const [status, setStatus] = useState<SmartClassAuthStatus | null>(null)
  const [captcha, setCaptcha] = useState<SmartClassCaptcha | null>(null)
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [verifyCode, setVerifyCode] = useState('')
  const [message, setMessage] = useState('')
  const [isRefreshing, setIsRefreshing] = useState(false)
  const [isLoadingCaptcha, setIsLoadingCaptcha] = useState(false)
  const [isLoggingIn, setIsLoggingIn] = useState(false)

  const refreshStatus = useCallback(async (showSpinner = true) => {
    if (showSpinner) {
      setIsRefreshing(true)
    }
    try {
      const nextStatus = await getSmartClassAuthStatus()
      setStatus(nextStatus)
      setMessage('')
      if (nextStatus.authenticated) {
        setCaptcha(null)
        setPassword('')
        setVerifyCode('')
      }
    } catch (error) {
      setMessage(error instanceof Error ? error.message : 'SmartClass 会话状态读取失败')
    } finally {
      if (showSpinner) {
        setIsRefreshing(false)
      }
    }
  }, [])

  useEffect(() => {
    void refreshStatus()
    // 这里刻意不做任何 interval 轮询。
    //
    // ClassFlow 前端本身部署在 Worker 后面，如果页面常驻时每 30 秒主动 GET 一次
    // `/smartclass/auth`，会无意义增加 Worker 请求数，也会把“状态同步”重新退化成轮询。
    // 但页面从后台切回前台 / 窗口重新获得焦点时做一次 GET 属于用户驱动的低频刷新，
    // 这个行为本来就合理，也能让用户从 SmartClass 登录页切回来后立刻看到真实状态。
    const refreshWhenVisible = () => {
      if (document.visibilityState === 'visible') {
        void refreshStatus(false)
      }
    }
    window.addEventListener('focus', refreshWhenVisible)
    document.addEventListener('visibilitychange', refreshWhenVisible)
    return () => {
      window.removeEventListener('focus', refreshWhenVisible)
      document.removeEventListener('visibilitychange', refreshWhenVisible)
    }
  }, [refreshStatus])

  const refreshCaptcha = async () => {
    setIsLoadingCaptcha(true)
    setMessage('')
    try {
      const nextCaptcha = await getSmartClassCaptcha()
      setCaptcha(nextCaptcha)
      setVerifyCode('')
    } catch (error) {
      setMessage(error instanceof Error ? error.message : '验证码获取失败')
    } finally {
      setIsLoadingCaptcha(false)
    }
  }

  const submitLogin = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!captcha) {
      setMessage('请先获取图形验证码。')
      return
    }
    if (!username.trim() || !password || !verifyCode.trim()) {
      setMessage('用户名、密码和验证码都需要填写。')
      return
    }

    setIsLoggingIn(true)
    setMessage('')
    try {
      const result = await loginSmartClass({
        username: username.trim(),
        password,
        verify_code: verifyCode.trim(),
        verify_code_id: captcha.verify_code_id,
      })
      if (!result.authenticated) {
        throw new Error('SmartClass 未确认登录成功。')
      }
      // 密码和验证码在请求结束后立即从 React 状态清除；用户名保留在当前页面内，便于
      // 验证码输错时重新提交，但页面刷新后也不会持久化。
      setPassword('')
      setVerifyCode('')
      setCaptcha(null)
      await refreshStatus()
    } catch (error) {
      setMessage(error instanceof Error ? error.message : 'SmartClass 登录失败')
      // 登录失败后验证码通常已经作废，主动重新获取，避免用户反复提交旧验证码。
      try {
        setCaptcha(await getSmartClassCaptcha())
        setVerifyCode('')
      } catch {
        setCaptcha(null)
      }
    } finally {
      setIsLoggingIn(false)
    }
  }

  const isAuthenticated = status?.authenticated === true

  return (
    <section className="smartClassSession" aria-label="SmartClass 自动采集会话">
      <div className="smartClassSession__summary">
        <div>
          <div className="smartClassSession__eyebrow">SmartClass / 自动发现</div>
          <strong>{isAuthenticated ? 'SmartClass 已登录' : 'SmartClass 需要登录'}</strong>
          <p>
            {isAuthenticated
              ? 'N1 会每 10 分钟自动检查新录播，无需再打开 Windows 浏览器桥。'
              : status?.reason || '正在读取 N1 会话状态…'}
          </p>
        </div>
        <div className="smartClassSession__actions">
          <span
            className={
              isAuthenticated
                ? 'statusPill statusPill--succeeded'
                : 'statusPill statusPill--pending'
            }
          >
            {status === null ? '检查中' : isAuthenticated ? '自动采集可用' : '等待人工恢复'}
          </span>
          <button
            type="button"
            className="buttonSecondary"
            disabled={isRefreshing}
            onClick={() => void refreshStatus()}
          >
            {isRefreshing ? '检查中…' : '刷新状态'}
          </button>
        </div>
      </div>

      {!isAuthenticated && status !== null ? (
        <form className="smartClassSession__login" onSubmit={submitLogin}>
          <div className="smartClassSession__fields">
            <div className="field">
              <label htmlFor="smartclass-username">SmartClass 用户名</label>
              <input
                id="smartclass-username"
                autoComplete="username"
                value={username}
                onChange={(event) => setUsername(event.target.value)}
              />
            </div>
            <div className="field">
              <label htmlFor="smartclass-password">密码</label>
              <input
                id="smartclass-password"
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(event) => setPassword(event.target.value)}
              />
            </div>
            <div className="field">
              <label htmlFor="smartclass-verify-code">验证码</label>
              <input
                id="smartclass-verify-code"
                inputMode="text"
                autoComplete="off"
                value={verifyCode}
                onChange={(event) => setVerifyCode(event.target.value)}
              />
            </div>
          </div>

          <div className="smartClassSession__captchaRow">
            <div className="smartClassSession__captcha">
              {captcha ? (
                <img src={captcha.image_data_url} alt="SmartClass 图形验证码" />
              ) : (
                <span>验证码尚未加载</span>
              )}
            </div>
            <button
              type="button"
              className="buttonSecondary"
              disabled={isLoadingCaptcha || isLoggingIn}
              onClick={() => void refreshCaptcha()}
            >
              {isLoadingCaptcha ? '获取中…' : captcha ? '换一张验证码' : '获取验证码'}
            </button>
            <button
              type="submit"
              className="smartClassSession__submit"
              disabled={isLoggingIn || isLoadingCaptcha}
            >
              {isLoggingIn ? '正在登录…' : '登录并恢复自动采集'}
            </button>
          </div>

          <small className="smartClassSession__privacy">
            凭据只用于本次 HTTPS 登录请求；浏览器不会保存密码，N1 只持久化登录 Session。
          </small>
        </form>
      ) : null}

      {message ? <div className="detailNotice detailNotice--error">{message}</div> : null}
    </section>
  )
}
