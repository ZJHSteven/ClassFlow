/**
 * 窄屏 Master-Detail 导航辅助。
 *
 * ClassFlow 的任务台与课程库在桌面端都采用“左列表 + 右详情”。原先 CSS 在窄屏下只是
 * 把两栏纵向堆叠，导致用户点击列表后还必须滚到整张列表底部才能看到详情。
 *
 * 这里把窄屏行为改成真正的两层页面：
 *
 * 1. 点击列表项时 push 一条浏览器 history entry，并切换到详情页。
 * 2. iPhone 左滑返回、浏览器返回按钮都会触发 popstate，统一回到列表页。
 * 3. 进入详情前保存列表滚动位置；返回时恢复，避免用户重新找刚才那一行。
 * 4. 桌面端不创建 history entry，仍保持原来的同屏选中行为。
 */

import { useCallback, useEffect, useRef, useState } from 'react'

const COMPACT_DETAIL_QUERY = '(max-width: 1100px)'

function matchesCompactLayout() {
  return typeof window.matchMedia === 'function' && window.matchMedia(COMPACT_DETAIL_QUERY).matches
}

interface ClassFlowDetailHistoryState {
  scope: string
  entityId: string
}

export function useResponsiveDetailNavigation(scope: string) {
  const [isCompactLayout, setIsCompactLayout] = useState<boolean>(() => matchesCompactLayout())
  const [isDetailPageOpen, setIsDetailPageOpen] = useState<boolean>(false)
  const panelRef = useRef<HTMLElement | null>(null)
  const previousScrollYRef = useRef<number>(0)
  const detailOpenRef = useRef<boolean>(false)
  const ownsHistoryEntryRef = useRef<boolean>(false)

  useEffect(() => {
    detailOpenRef.current = isDetailPageOpen
  }, [isDetailPageOpen])

  useEffect(() => {
    if (typeof window.matchMedia !== 'function') {
      return
    }

    const mediaQuery = window.matchMedia(COMPACT_DETAIL_QUERY)
    const syncLayout = () => {
      const nextCompact = mediaQuery.matches
      setIsCompactLayout(nextCompact)

      // 横屏/窗口放大回桌面布局时，不应该继续隐藏左侧列表。
      if (!nextCompact) {
        detailOpenRef.current = false
        ownsHistoryEntryRef.current = false
        setIsDetailPageOpen(false)
      }
    }

    syncLayout()
    mediaQuery.addEventListener?.('change', syncLayout)
    return () => mediaQuery.removeEventListener?.('change', syncLayout)
  }, [])

  const restoreListScroll = useCallback(() => {
    window.requestAnimationFrame(() => {
      window.scrollTo({
        top: previousScrollYRef.current,
        behavior: 'auto',
      })
    })
  }, [])

  useEffect(() => {
    const handlePopState = () => {
      if (!detailOpenRef.current) {
        return
      }

      detailOpenRef.current = false
      ownsHistoryEntryRef.current = false
      setIsDetailPageOpen(false)
      restoreListScroll()
    }

    window.addEventListener('popstate', handlePopState)
    return () => window.removeEventListener('popstate', handlePopState)
  }, [restoreListScroll])

  const openDetailPage = useCallback((entityId: string) => {
    if (!isCompactLayout) {
      return
    }

    previousScrollYRef.current = window.scrollY
    const currentState = window.history.state
    const nextState = currentState && typeof currentState === 'object'
      ? { ...currentState }
      : {}
    const classflowDetail: ClassFlowDetailHistoryState = {
      scope,
      entityId,
    }

    window.history.pushState(
      {
        ...nextState,
        classflowDetail,
      },
      '',
      window.location.href,
    )
    ownsHistoryEntryRef.current = true
    detailOpenRef.current = true
    setIsDetailPageOpen(true)

    // 详情替换列表后，把视口带回当前 panel 顶部，而不是停在原列表深处的 scrollY。
    window.requestAnimationFrame(() => {
      panelRef.current?.scrollIntoView?.({
        block: 'start',
        behavior: 'auto',
      })
    })
  }, [isCompactLayout, scope])

  const closeDetailPage = useCallback(() => {
    if (!detailOpenRef.current) {
      return
    }

    const state = window.history.state as { classflowDetail?: ClassFlowDetailHistoryState } | null
    if (
      ownsHistoryEntryRef.current
      && state?.classflowDetail?.scope === scope
    ) {
      // 与 iOS 左滑返回走同一条 popstate 路径，避免“按钮返回”和“系统返回”出现两套状态。
      window.history.back()
      return
    }

    detailOpenRef.current = false
    ownsHistoryEntryRef.current = false
    setIsDetailPageOpen(false)
    restoreListScroll()
  }, [restoreListScroll, scope])

  return {
    panelRef,
    isCompactLayout,
    isDetailPageOpen,
    openDetailPage,
    closeDetailPage,
  }
}

