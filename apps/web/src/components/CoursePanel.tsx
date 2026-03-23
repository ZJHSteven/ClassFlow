/**
 * 课程库面板。
 *
 * 这个组件负责展示“课程层面”的聚合结果：
 *
 * 1. 左侧按课程维度浏览。
 * 2. 右侧展示课程总稿 Markdown 预览。
 * 3. 默认不做后台轮询，避免列表定时闪烁，也避免 Worker 代理产生无意义请求。
 * 4. 采用“首次加载 + 手动刷新 + 页面回到前台时同步”的策略。
 */

import { startTransition, useCallback, useEffect, useRef, useState } from 'react'
import { getCourseDetail, getCourseMarkdown, listCourses } from '../api'
import type { CourseDetail, CourseSummary } from '../types'

/**
 * 把最近同步时间格式化成短时间。
 */
function formatSyncTime(timestamp: string) {
  if (!timestamp) {
    return '尚未同步'
  }

  return new Date(timestamp).toLocaleTimeString('zh-CN', {
    hour12: false,
  })
}

export function CoursePanel() {
  const [courses, setCourses] = useState<CourseSummary[]>([])
  const [selectedCourseKey, setSelectedCourseKey] = useState<string>('')
  const [selectedCourseDetail, setSelectedCourseDetail] = useState<CourseDetail | null>(null)
  const [markdownPreview, setMarkdownPreview] = useState<string>('')
  const [semesterFilter, setSemesterFilter] = useState<string>('')
  const [dateFilter, setDateFilter] = useState<string>('')
  const [courseFilter, setCourseFilter] = useState<string>('')
  const [errorMessage, setErrorMessage] = useState<string>('')
  const [isLoading, setIsLoading] = useState<boolean>(true)
  const [isRefreshing, setIsRefreshing] = useState<boolean>(false)
  const [lastSyncedAt, setLastSyncedAt] = useState<string>('')
  const listRequestIdRef = useRef<number>(0)
  const detailRequestIdRef = useRef<number>(0)
  const selectedCourseKeyRef = useRef<string>('')

  useEffect(() => {
    selectedCourseKeyRef.current = selectedCourseKey
  }, [selectedCourseKey])

  /**
   * 刷新单个课程详情与总稿。
   *
   * 课程详情页需要同时取详情 JSON 和 Markdown 正文，
   * 所以这里并行拉取两条请求，减少等待时间。
   */
  const loadCourseDetail = useCallback(async (courseKey: string) => {
    const requestId = detailRequestIdRef.current + 1
    detailRequestIdRef.current = requestId

    try {
      const [detail, markdown] = await Promise.all([
        getCourseDetail(courseKey),
        getCourseMarkdown(courseKey),
      ])

      if (detailRequestIdRef.current !== requestId) {
        return
      }

      startTransition(() => {
        setSelectedCourseDetail(detail)
        setMarkdownPreview(markdown)
      })
    } catch (error) {
      if (detailRequestIdRef.current === requestId) {
        setErrorMessage(error instanceof Error ? error.message : '课程详情加载失败')
      }
    }
  }, [])

  /**
   * 刷新课程列表；必要时顺带刷新当前课程详情。
   */
  const loadCourses = useCallback(async (options?: { blocking?: boolean; refreshDetail?: boolean }) => {
    const { blocking = false, refreshDetail = false } = options ?? {}
    const requestId = listRequestIdRef.current + 1
    listRequestIdRef.current = requestId

    try {
      if (blocking) {
        setIsLoading(true)
      } else {
        setIsRefreshing(true)
      }

      const nextCourses = await listCourses({
        semester: semesterFilter || undefined,
        date: dateFilter || undefined,
        course_name: courseFilter || undefined,
      })

      if (listRequestIdRef.current !== requestId) {
        return
      }

      const nextSelectedCourseKey = nextCourses.some((course) => course.course_key === selectedCourseKeyRef.current)
        ? selectedCourseKeyRef.current
        : nextCourses[0]?.course_key ?? ''

      startTransition(() => {
        setCourses(nextCourses)
        setSelectedCourseKey(nextSelectedCourseKey)
        setErrorMessage('')
        setLastSyncedAt(new Date().toISOString())
      })

      if (refreshDetail) {
        if (nextSelectedCourseKey) {
          await loadCourseDetail(nextSelectedCourseKey)
        } else {
          startTransition(() => {
            setSelectedCourseDetail(null)
            setMarkdownPreview('')
          })
        }
      }
    } catch (error) {
      if (listRequestIdRef.current === requestId) {
        setErrorMessage(error instanceof Error ? error.message : '课程库加载失败')
      }
    } finally {
      if (listRequestIdRef.current === requestId) {
        setIsLoading(false)
        setIsRefreshing(false)
      }
    }
  }, [courseFilter, dateFilter, loadCourseDetail, semesterFilter])

  useEffect(() => {
    void loadCourses({
      blocking: true,
      refreshDetail: true,
    })
  }, [loadCourses])

  useEffect(() => {
    const syncWhenVisibleAgain = () => {
      if (document.visibilityState !== 'visible') {
        return
      }

      void loadCourses({
        blocking: false,
        refreshDetail: true,
      })
    }

    window.addEventListener('focus', syncWhenVisibleAgain)
    document.addEventListener('visibilitychange', syncWhenVisibleAgain)

    return () => {
      window.removeEventListener('focus', syncWhenVisibleAgain)
      document.removeEventListener('visibilitychange', syncWhenVisibleAgain)
    }
  }, [loadCourses])

  useEffect(() => {
    if (!selectedCourseKey) {
      setSelectedCourseDetail(null)
      setMarkdownPreview('')
      return
    }

    void loadCourseDetail(selectedCourseKey)
  }, [loadCourseDetail, selectedCourseKey])

  return (
    <section className="panel">
      <div className="panel__grid">
        <div className="card card--padded">
          <div className="card__header">
            <div>
              <h2>课程库</h2>
              <p>以“课程”而不是“单个任务”的视角审查最终交付物，并避免后台定时轮询。</p>
            </div>
            <div className="card__actions">
              <div className="syncHint">最近同步：{formatSyncTime(lastSyncedAt)}</div>
              <button
                type="button"
                className="buttonSecondary"
                onClick={() =>
                  void loadCourses({
                    blocking: false,
                    refreshDetail: true,
                  })
                }
                disabled={isRefreshing}
              >
                {isRefreshing ? '同步中...' : '刷新课程库'}
              </button>
            </div>
          </div>

          <div className="filters filters--compact">
            <div className="field">
              <label htmlFor="course-semester">学期</label>
              <input id="course-semester" value={semesterFilter} onChange={(event) => setSemesterFilter(event.target.value)} placeholder="2025-2026-2" />
            </div>
            <div className="field">
              <label htmlFor="course-date">日期</label>
              <input id="course-date" value={dateFilter} onChange={(event) => setDateFilter(event.target.value)} placeholder="2026-03-20" />
            </div>
            <div className="field">
              <label htmlFor="course-name">课程名</label>
              <input id="course-name" value={courseFilter} onChange={(event) => setCourseFilter(event.target.value)} placeholder="病理学" />
            </div>
          </div>

          {errorMessage ? <div className="emptyState">{errorMessage}</div> : null}

          <div className="tableWrap">
            <table className="table">
              <thead>
                <tr>
                  <th>课程</th>
                  <th>日期</th>
                  <th>片段</th>
                  <th>失败</th>
                </tr>
              </thead>
              <tbody>
                {courses.map((course) => (
                  <tr key={course.course_key} onClick={() => setSelectedCourseKey(course.course_key)}>
                    <td>
                      <strong>{course.course_name}</strong>
                      <div>{course.teacher_name}</div>
                    </td>
                    <td>
                      <div>{course.semester}</div>
                      <div>{course.date}</div>
                    </td>
                    <td>
                      {course.successful_segment_count} / {course.received_segment_count}
                    </td>
                    <td>{course.has_failed_segment ? '是' : '否'}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {isLoading ? <div className="emptyState">正在加载课程列表...</div> : null}
          {!isLoading && courses.length === 0 ? <div className="emptyState">当前没有课程总稿可展示。</div> : null}
        </div>

        <aside className="card card--padded detail">
          <div className="card__header">
            <div>
              <h3>课程总稿预览</h3>
              <p>预览 Worker 代理从后端取回的 Markdown 成品。</p>
            </div>
          </div>

          {selectedCourseDetail ? (
            <>
              <div className="detail__meta">
                <div>
                  <strong>{selectedCourseDetail.course_name}</strong> / {selectedCourseDetail.teacher_name}
                </div>
                <div>
                  {selectedCourseDetail.semester} / {selectedCourseDetail.date}
                </div>
                <div>
                  片段：{selectedCourseDetail.successful_segment_count} / {selectedCourseDetail.received_segment_count}
                </div>
                <div>失败片段：{selectedCourseDetail.has_failed_segment ? '有' : '无'}</div>
              </div>
              <pre className="markdownPreview">{markdownPreview || '课程总稿尚未生成。'}</pre>
            </>
          ) : (
            <div className="emptyState">从左侧选择一个课程查看总稿。</div>
          )}
        </aside>
      </div>
    </section>
  )
}
