/**
 * 课程库面板。
 *
 * 这里把“课程级交付物”集中到右侧：
 *
 * 1. 课程总稿下载。
 * 2. 课程 manifest 下载。
 * 3. 固定高度、内部滚动的课程总稿预览区。
 *
 * 设计目的很直接：
 *
 * 1. 不让一大段 Markdown 把整页高度拖得失控。
 * 2. 下载按钮必须足够显眼，不能藏在小角落里。
 * 3. 课程总稿不存在时，只在右侧详情区说明，不把 404 错误抛回左侧列表区误导用户。
 */

import { AnimatePresence, motion } from 'motion/react'
import { startTransition, useCallback, useEffect, useRef, useState } from 'react'
import {
  getCourseArtifactUrl,
  getCourseDetail,
  getCourseMarkdown,
  getLectureArtifactUrl,
  getLectureMarkdown,
  listCourses,
  recoverLectureNotes,
  retryLectureNotes,
} from '../api'
import type { LectureNotesRecoveryAction } from '../api'
import { buildCourseMarkdownDownloadFilename } from '../courseDownloadFilename'
import { downloadBlob, downloadWithProgress } from '../downloads'
import { formatBytes, formatPercent, formatSpeed, normalizePercent } from '../progress'
import { useResponsiveDetailNavigation } from '../responsiveDetailNavigation'
import type { CourseDetail, CourseSummary } from '../types'

interface DownloadButtonState {
  status: 'idle' | 'downloading' | 'succeeded' | 'failed'
  progressPercent: number | null
  receivedBytes: number
  totalBytes: number | null
  speedBytesPerSec: number | null
  errorMessage: string
}

interface CourseMarkdownCacheEntry {
  markdown: string
  updatedAt: string
}

type PreviewMode = 'asr' | 'notes'

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
  const [selectedLectureKey, setSelectedLectureKey] = useState<string>('')
  const [previewMode, setPreviewMode] = useState<PreviewMode>('asr')
  const [markdownPreview, setMarkdownPreview] = useState<string>('')
  const [previewTitle, setPreviewTitle] = useState<string>('课程 ASR')
  const [previewMessage, setPreviewMessage] = useState<string>('')
  const [isPreviewLoading, setIsPreviewLoading] = useState<boolean>(false)
  const [semesterFilter, setSemesterFilter] = useState<string>('')
  const [dateFilter, setDateFilter] = useState<string>('')
  const [courseFilter, setCourseFilter] = useState<string>('')
  const [listErrorMessage, setListErrorMessage] = useState<string>('')
  const [isLoading, setIsLoading] = useState<boolean>(true)
  const [isRefreshing, setIsRefreshing] = useState<boolean>(false)
  const [downloadStates, setDownloadStates] = useState<Record<string, DownloadButtonState>>({})
  const [lastSyncedAt, setLastSyncedAt] = useState<string>('')
  const listRequestIdRef = useRef<number>(0)
  const detailRequestIdRef = useRef<number>(0)
  const selectedCourseKeyRef = useRef<string>('')
  const selectedLectureKeyRef = useRef<string>('')
  const markdownCacheRef = useRef<Record<string, CourseMarkdownCacheEntry>>({})
  const pressableProps = {
    whileHover: { y: -2, scale: 1.01 },
    whileTap: { y: 0, scale: 0.985 },
    transition: { type: 'spring' as const, stiffness: 380, damping: 24 },
  }
  const detailNavigation = useResponsiveDetailNavigation('course')

  useEffect(() => {
    selectedCourseKeyRef.current = selectedCourseKey
  }, [selectedCourseKey])

  useEffect(() => {
    selectedLectureKeyRef.current = selectedLectureKey
  }, [selectedLectureKey])

  /**
   * 刷新单个课程详情与总稿。
   *
   * 这里先取详情，再决定是否读取 `course.md`：
   *
   * 1. 课程还没产出总稿时，直接给出“尚未生成”的说明。
   * 2. 只有详情明确存在 `merged_markdown_path` 时，才去请求 Markdown 正文。
   */
  const loadCourseDetail = useCallback(async (courseKey: string) => {
    const requestId = detailRequestIdRef.current + 1
    detailRequestIdRef.current = requestId

    try {
      startTransition(() => {
        setIsPreviewLoading(true)
        setPreviewMessage('')
      })

      const detail = await getCourseDetail(courseKey)

      if (detailRequestIdRef.current !== requestId) {
        return
      }

      let nextMarkdownPreview = ''
      let nextPreviewMessage = ''

      if (detail.merged_markdown_path) {
        const cachedEntry = markdownCacheRef.current[courseKey]
        if (cachedEntry && cachedEntry.updatedAt === detail.updated_at) {
          nextMarkdownPreview = cachedEntry.markdown
        } else {
          try {
            nextMarkdownPreview = await getCourseMarkdown(courseKey)
            markdownCacheRef.current[courseKey] = {
              markdown: nextMarkdownPreview,
              updatedAt: detail.updated_at,
            }
          } catch (error) {
            nextPreviewMessage = error instanceof Error ? error.message : '课程总稿加载失败'
          }
        }
      } else {
        delete markdownCacheRef.current[courseKey]
        nextPreviewMessage = '课程总稿尚未生成。当前课程可能仍在下载、上传、转写或合并中。'
      }

      if (detailRequestIdRef.current !== requestId) {
        return
      }

      const nextLectureKey = detail.lectures.some(
        (lecture) => lecture.lecture_key === selectedLectureKeyRef.current,
      )
        ? selectedLectureKeyRef.current
        : detail.lectures[0]?.lecture_key ?? ''

      startTransition(() => {
        setSelectedCourseDetail(detail)
        setSelectedLectureKey(nextLectureKey)
        setPreviewMode('asr')
        setMarkdownPreview(nextMarkdownPreview)
        setPreviewTitle('课程 ASR')
        setPreviewMessage(nextPreviewMessage)
        setIsPreviewLoading(false)
      })
    } catch (error) {
      if (detailRequestIdRef.current === requestId) {
        startTransition(() => {
          setSelectedCourseDetail(null)
          setSelectedLectureKey('')
          setPreviewMode('asr')
          setMarkdownPreview('')
          setPreviewTitle('课程 ASR')
          setPreviewMessage(error instanceof Error ? error.message : '课程详情加载失败')
          setIsPreviewLoading(false)
        })
      }
    }
  }, [])

  /**
   * 刷新课程列表；必要时同步当前课程详情。
   *
   * 和任务台一样，这里也避免在“选中项变化时”重复触发详情请求。
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
        setListErrorMessage('')
        setLastSyncedAt(new Date().toISOString())
      })

      if (refreshDetail) {
        if (nextSelectedCourseKey && nextSelectedCourseKey === selectedCourseKeyRef.current) {
          await loadCourseDetail(nextSelectedCourseKey)
        } else {
          startTransition(() => {
            setSelectedCourseDetail(null)
            setSelectedLectureKey('')
            setPreviewMode('asr')
            setMarkdownPreview('')
            setPreviewTitle('课程 ASR')
            setPreviewMessage('')
            setIsPreviewLoading(false)
          })
        }
      }
    } catch (error) {
      if (listRequestIdRef.current === requestId) {
        setListErrorMessage(error instanceof Error ? error.message : '课程库加载失败')
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
      setSelectedLectureKey('')
      setPreviewMode('asr')
      setMarkdownPreview('')
      setPreviewTitle('课程 ASR')
      setPreviewMessage('')
      setIsPreviewLoading(false)
      return
    }

    void loadCourseDetail(selectedCourseKey)
  }, [loadCourseDetail, selectedCourseKey])

  const courseMarkdownDownloadUrl = selectedCourseDetail?.merged_markdown_path
    ? getCourseArtifactUrl(selectedCourseDetail.course_key, 'course.md')
    : ''
  const manifestDownloadUrl = selectedCourseDetail?.manifest_path
    ? getCourseArtifactUrl(selectedCourseDetail.course_key, 'manifest.json')
    : ''
  const selectedLecture = selectedCourseDetail?.lectures.find(
    (lecture) => lecture.lecture_key === selectedLectureKey,
  ) ?? selectedCourseDetail?.lectures[0]
  const selectedLectureIndex = selectedCourseDetail && selectedLecture
    ? selectedCourseDetail.lectures.findIndex((lecture) => lecture.lecture_key === selectedLecture.lecture_key)
    : -1

  const buildLectureNotesFilename = (lecture: NonNullable<CourseDetail['lectures']>[number], index: number) => {
    const title = lecture.notes_title || `${selectedCourseDetail?.course_name ?? '课程'}-课堂笔记`
    return `${index + 1}-${title}.md`
  }

  const courseActions = selectedCourseDetail
    ? [
        courseMarkdownDownloadUrl
          ? {
              key: 'course.md',
              label: '下载课程 ASR',
              href: courseMarkdownDownloadUrl,
              filename: buildCourseMarkdownDownloadFilename({
                date: selectedCourseDetail.date,
                courseName: selectedCourseDetail.course_name,
                teacherName: selectedCourseDetail.teacher_name,
              }),
              primary: true,
            }
          : null,
        manifestDownloadUrl
          ? {
              key: 'manifest.json',
              label: '下载课程清单',
              href: manifestDownloadUrl,
              filename: `${selectedCourseDetail.course_name}-${selectedCourseDetail.date}-manifest.json`,
              primary: false,
            }
          : null,
        selectedLecture?.notes_ready
          ? {
              key: `lecture:${selectedLecture.lecture_key}:notes.md`,
              label: '下载 Notes',
              href: getLectureArtifactUrl(selectedCourseDetail.course_key, selectedLecture.lecture_key, 'notes.md'),
              filename: buildLectureNotesFilename(selectedLecture, Math.max(selectedLectureIndex, 0)),
              primary: true,
            }
          : null,
      ].filter(Boolean) as Array<{ key: string; label: string; href: string; filename: string; primary: boolean }>
    : []

  const handleCourseDownload = async (action: { key: string; href: string; filename: string }) => {
    const cachedMarkdown =
      action.key === 'course.md' && selectedCourseDetail
        ? markdownCacheRef.current[selectedCourseDetail.course_key]?.markdown || markdownPreview
        : ''

    if (action.key === 'course.md' && cachedMarkdown) {
      const markdownBlob = new Blob([cachedMarkdown], { type: 'text/markdown; charset=utf-8' })
      downloadBlob(markdownBlob, action.filename)
      setDownloadStates((current) => ({
        ...current,
        [action.key]: {
          status: 'succeeded',
          progressPercent: 100,
          receivedBytes: markdownBlob.size,
          totalBytes: markdownBlob.size,
          speedBytesPerSec: null,
          errorMessage: '',
        },
      }))
      return
    }

    setDownloadStates((current) => ({
      ...current,
      [action.key]: {
        status: 'downloading',
        progressPercent: 0,
        receivedBytes: 0,
        totalBytes: null,
        speedBytesPerSec: 0,
        errorMessage: '',
      },
    }))

    try {
      await downloadWithProgress(action.href, action.filename, (snapshot) => {
        setDownloadStates((current) => ({
          ...current,
          [action.key]: {
            status: 'downloading',
            progressPercent: snapshot.progressPercent,
            receivedBytes: snapshot.receivedBytes,
            totalBytes: snapshot.totalBytes,
            speedBytesPerSec: snapshot.speedBytesPerSec,
            errorMessage: '',
          },
        }))
      })

      setDownloadStates((current) => ({
        ...current,
        [action.key]: {
          ...(current[action.key] ?? {
            progressPercent: 100,
            receivedBytes: 0,
            totalBytes: null,
            speedBytesPerSec: null,
          }),
          status: 'succeeded',
          progressPercent: 100,
          errorMessage: '',
        },
      }))
    } catch (error) {
      setDownloadStates((current) => ({
        ...current,
        [action.key]: {
          ...(current[action.key] ?? {
            progressPercent: null,
            receivedBytes: 0,
            totalBytes: null,
            speedBytesPerSec: null,
          }),
          status: 'failed',
          errorMessage: error instanceof Error ? error.message : '下载失败',
        },
      }))
    }
  }

  const handlePreviewModeChange = async (mode: PreviewMode) => {
    if (!selectedCourseDetail) {
      return
    }
    setPreviewMode(mode)
    setIsPreviewLoading(true)
    setPreviewMessage('')

    if (mode === 'asr') {
      try {
        const cachedEntry = markdownCacheRef.current[selectedCourseDetail.course_key]
        const markdown = cachedEntry?.markdown || await getCourseMarkdown(selectedCourseDetail.course_key)
        markdownCacheRef.current[selectedCourseDetail.course_key] = {
          markdown,
          updatedAt: selectedCourseDetail.updated_at,
        }
        setMarkdownPreview(markdown)
        setPreviewTitle('课程 ASR')
      } catch (error) {
        setMarkdownPreview('')
        setPreviewMessage(error instanceof Error ? error.message : '课程 ASR 加载失败')
      } finally {
        setIsPreviewLoading(false)
      }
      return
    }

    if (!selectedLecture?.notes_ready) {
      setMarkdownPreview('')
      setPreviewTitle('Notes')
      setPreviewMessage(
        selectedLecture?.notes_status === 'failed'
          ? selectedLecture.notes_last_error || 'Notes 生成失败，可在上方重试。'
          : 'Notes 尚未生成完成。',
      )
      setIsPreviewLoading(false)
      return
    }

    try {
      const markdown = await getLectureMarkdown(
        selectedCourseDetail.course_key,
        selectedLecture.lecture_key,
        'notes.md',
      )
      setMarkdownPreview(markdown)
      setPreviewTitle(selectedLecture.notes_title || 'Notes')
    } catch (error) {
      setMarkdownPreview('')
      setPreviewMessage(error instanceof Error ? error.message : 'Notes 加载失败')
    } finally {
      setIsPreviewLoading(false)
    }
  }

  const handleNotesRetry = async (lecture: NonNullable<CourseDetail['lectures']>[number]) => {
    if (!selectedCourseDetail) return
    try {
      await retryLectureNotes(selectedCourseDetail.course_key, lecture.lecture_key)
      await loadCourseDetail(selectedCourseDetail.course_key)
    } catch (error) {
      setPreviewMessage(error instanceof Error ? error.message : 'Notes 任务提交失败')
    }
  }

  const handleNotesRecovery = async (
    lecture: NonNullable<CourseDetail['lectures']>[number],
    action: LectureNotesRecoveryAction,
  ) => {
    if (!selectedCourseDetail) return
    if (
      action === 'force_resubmit'
      && !window.confirm('只有在已经人工确认 Archive 中不存在对应 conversation 时，才应重建。确定继续吗？')
    ) {
      return
    }
    try {
      await recoverLectureNotes(selectedCourseDetail.course_key, lecture.lecture_key, action)
      await loadCourseDetail(selectedCourseDetail.course_key)
    } catch (error) {
      setPreviewMessage(error instanceof Error ? error.message : 'Notes 恢复操作失败')
    }
  }

  const handleCourseSelect = (courseKey: string) => {
    setSelectedCourseKey(courseKey)
    detailNavigation.openDetailPage(courseKey)
  }

  return (
    <section className="panel" ref={detailNavigation.panelRef}>
      <div className={`panel__grid ${detailNavigation.isDetailPageOpen ? 'panel__grid--detail-page' : 'panel__grid--list-page'}`}>
        <motion.div
          className="card card--padded card--panel masterPane"
          layout
          initial={{ opacity: 0, x: -10 }}
          animate={{ opacity: 1, x: 0 }}
          transition={{ duration: 0.24, ease: 'easeOut' }}
        >
          <div className="card__header">
            <div>
              <h2>课程库</h2>
              <p>左侧负责课程检索，右侧集中展示课程级交付物和总稿预览。</p>
            </div>
            <div className="card__actions">
              <div className="syncHint">最近同步：{formatSyncTime(lastSyncedAt)}</div>
              <motion.button
                type="button"
                className="buttonSecondary"
                onClick={() =>
                  void loadCourses({
                    blocking: false,
                    refreshDetail: true,
                  })
                }
                disabled={isRefreshing}
                {...pressableProps}
              >
                {isRefreshing ? '同步中...' : '刷新课程库'}
              </motion.button>
            </div>
          </div>

          <div className="filters filters--compact">
            <motion.div className="field" whileHover={{ y: -1 }}>
              <label htmlFor="course-semester">学期</label>
              <input id="course-semester" value={semesterFilter} onChange={(event) => setSemesterFilter(event.target.value)} placeholder="2025-2026-2" />
            </motion.div>
            <motion.div className="field" whileHover={{ y: -1 }}>
              <label htmlFor="course-date">日期</label>
              <input id="course-date" value={dateFilter} onChange={(event) => setDateFilter(event.target.value)} placeholder="2026-03-20" />
            </motion.div>
            <motion.div className="field" whileHover={{ y: -1 }}>
              <label htmlFor="course-name">课程名</label>
              <input id="course-name" value={courseFilter} onChange={(event) => setCourseFilter(event.target.value)} placeholder="病理学" />
            </motion.div>
          </div>

          {listErrorMessage ? <div className="detailNotice detailNotice--error">{listErrorMessage}</div> : null}

          <div className="tableWrap tableWrap--list">
            <table className="table">
              <thead>
                <tr>
                  <th>课程</th>
                  <th>日期</th>
                  <th>片段</th>
                  <th>失败</th>
                  <th>总稿</th>
                </tr>
              </thead>
              <tbody>
                {courses.map((course) => (
                  <motion.tr
                    key={course.course_key}
                    layout
                    className={
                      course.course_key === selectedCourseKey
                        ? 'tableRow tableRow--interactive is-selected'
                        : 'tableRow tableRow--interactive'
                    }
                    onClick={() => handleCourseSelect(course.course_key)}
                    {...pressableProps}
                  >
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
                    <td>{course.merged_markdown_path ? '已生成' : '未生成'}</td>
                  </motion.tr>
                ))}
              </tbody>
            </table>
          </div>

          {isLoading ? <div className="emptyState">正在加载课程列表...</div> : null}
          {!isLoading && courses.length === 0 ? <div className="emptyState">当前没有课程总稿可展示。</div> : null}
        </motion.div>

        <motion.aside
          className="card card--padded detail detail--panel detailPane"
          layout
          initial={{ opacity: 0, x: 10 }}
          animate={{ opacity: 1, x: 0 }}
          transition={{ duration: 0.24, ease: 'easeOut' }}
        >
          <motion.button
            type="button"
            className="mobileDetailBack"
            onClick={detailNavigation.closeDetailPage}
            {...pressableProps}
          >
            <span aria-hidden="true">←</span>
            返回课程库
          </motion.button>
          <div className="card__header">
            <div>
              <h3>课程 ASR 与 Notes</h3>
              <p>一次连续授课只展示为一堂完整课堂；45 分钟录像切片仅作为后台聚合信息，不再暴露成独立课程操作。</p>
            </div>
          </div>

          {selectedCourseDetail ? (
            <div className="detail__body">
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

              <div className="detailActionGrid">
                {courseActions.map((action) => (
                  <div key={action.key} className="downloadAction">
                    <motion.button
                      type="button"
                      className={
                        downloadStates[action.key]?.status === 'failed'
                          ? 'artifactPill artifactPill--error'
                          : action.primary
                            ? 'artifactPill artifactPill--primary'
                            : 'artifactPill'
                      }
                      onClick={() => void handleCourseDownload(action)}
                      disabled={downloadStates[action.key]?.status === 'downloading'}
                      {...pressableProps}
                    >
                      {downloadStates[action.key]?.status === 'downloading'
                        ? `${action.label} · ${formatPercent(downloadStates[action.key]?.progressPercent)}`
                        : action.label}
                    </motion.button>
                    {downloadStates[action.key]?.status === 'downloading' ? (
                      <div className="downloadAction__meta">
                        <div className="miniBar">
                          <div
                            className="miniBar__fill"
                            style={{ width: `${normalizePercent(downloadStates[action.key]?.progressPercent) ?? 6}%` }}
                          />
                        </div>
                        <span>
                          {formatBytes(downloadStates[action.key]?.receivedBytes)} / {formatBytes(downloadStates[action.key]?.totalBytes)}
                        </span>
                        <span>{formatSpeed(downloadStates[action.key]?.speedBytesPerSec)}</span>
                      </div>
                    ) : null}
                    {downloadStates[action.key]?.status === 'failed' ? (
                      <div className="downloadAction__error">{downloadStates[action.key]?.errorMessage}</div>
                    ) : null}
                  </div>
                ))}
              </div>

              {selectedCourseDetail.lectures.length > 1 ? (
                <label className="lectureSelector">
                  <span>完整课堂</span>
                  <select
                    value={selectedLecture?.lecture_key ?? ''}
                    onChange={(event) => {
                      setSelectedLectureKey(event.target.value)
                      setPreviewMode('asr')
                      void handlePreviewModeChange('asr')
                    }}
                  >
                    {selectedCourseDetail.lectures.map((lecture, index) => (
                      <option key={lecture.lecture_key} value={lecture.lecture_key}>
                        {index + 1}. {lecture.start_time} - {lecture.end_time}
                      </option>
                    ))}
                  </select>
                </label>
              ) : null}

              {selectedLecture ? (
                <div className="lectureSummary">
                  <div>
                    <strong>{selectedLecture.start_time} - {selectedLecture.end_time}</strong>
                    <span>{selectedLecture.segment_count} 个录播切片已合并为 1 堂完整课堂</span>
                  </div>
                  <div className="detailActionGrid">
                    {selectedLecture.notes_recovery_kind === 'submission_unknown' ? (
                      <>
                        <motion.button
                          type="button"
                          className="artifactPill artifactPill--primary"
                          onClick={() => void handleNotesRecovery(selectedLecture, 'reconcile')}
                          {...pressableProps}
                        >
                          核对已有会话
                        </motion.button>
                        <motion.button
                          type="button"
                          className="artifactPill artifactPill--error"
                          onClick={() => void handleNotesRecovery(selectedLecture, 'force_resubmit')}
                          {...pressableProps}
                        >
                          确认不存在后重建
                        </motion.button>
                      </>
                    ) : selectedLecture.notes_recovery_kind === 'safe_resubmit' ? (
                      <motion.button
                        type="button"
                        className="artifactPill artifactPill--error"
                        onClick={() => void handleNotesRecovery(selectedLecture, 'safe_resubmit')}
                        {...pressableProps}
                      >
                        安全重新提交
                      </motion.button>
                    ) : selectedLecture.notes_recovery_kind === 'manual_review'
                      && selectedLecture.notes_conversation_id ? (
                        <motion.button
                          type="button"
                          className="artifactPill artifactPill--pending"
                          onClick={() => void handleNotesRecovery(selectedLecture, 'resume_existing')}
                          {...pressableProps}
                        >
                          从已有会话继续
                        </motion.button>
                      ) : selectedLecture.notes_status === 'failed' ? (
                        <span className="artifactPill artifactPill--error">需人工核对</span>
                    ) : selectedLecture.notes_status === 'not_started' ? (
                      <motion.button
                        type="button"
                        className="artifactPill artifactPill--pending"
                        disabled={!selectedLecture.transcript_ready}
                        onClick={() => void handleNotesRetry(selectedLecture)}
                        {...pressableProps}
                      >
                        {selectedLecture.transcript_ready ? '生成 Notes' : '等待 ASR'}
                      </motion.button>
                    ) : !selectedLecture.notes_ready ? (
                      <span className="artifactPill artifactPill--pending">
                        {selectedLecture.notes_recovery_kind === 'submission_unknown'
                          ? '提交结果待核对'
                          : selectedLecture.notes_status === 'pending'
                          ? 'Notes 等待提交'
                          : selectedLecture.notes_status === 'generating'
                            ? 'Notes 生成中'
                            : selectedLecture.notes_status === 'publishing'
                              ? 'Notes 同步 GitHub'
                              : 'Notes 处理中'}
                      </span>
                    ) : null}
                  </div>
                </div>
              ) : (
                <div className="detailNotice detailNotice--info">当前还没有可识别的完整课堂。</div>
              )}

              {selectedLecture?.notes_last_error ? (
                <div className="detailNotice detailNotice--warning detailNotice--scrollable">
                  {selectedLecture.notes_last_error}
                </div>
              ) : null}

              {previewMessage ? <div className="detailNotice detailNotice--warning detailNotice--scrollable">{previewMessage}</div> : null}

              <div className="previewShell">
                <div className="artifactSwitch" role="tablist" aria-label="课程内容预览">
                  <motion.button
                    type="button"
                    role="tab"
                    aria-selected={previewMode === 'asr'}
                    className={previewMode === 'asr' ? 'artifactSwitch__button is-active' : 'artifactSwitch__button'}
                    onClick={() => void handlePreviewModeChange('asr')}
                    {...pressableProps}
                  >
                    课程 ASR
                  </motion.button>
                  <motion.button
                    type="button"
                    role="tab"
                    aria-selected={previewMode === 'notes'}
                    className={previewMode === 'notes' ? 'artifactSwitch__button is-active' : 'artifactSwitch__button'}
                    onClick={() => void handlePreviewModeChange('notes')}
                    disabled={!selectedLecture}
                    {...pressableProps}
                  >
                    Notes
                  </motion.button>
                </div>
                <div className="previewShell__header">
                  <strong>{previewTitle}</strong>
                  <span>{isPreviewLoading ? '正在读取...' : markdownPreview ? '已加载' : '暂无内容'}</span>
                </div>

                <AnimatePresence mode="wait">
                  <motion.pre
                    key={markdownPreview ? 'loaded' : isPreviewLoading ? 'loading' : 'empty'}
                    className="markdownPreview"
                    initial={{ opacity: 0, y: 8 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -8 }}
                    transition={{ duration: 0.18, ease: 'easeOut' }}
                  >
                    {isPreviewLoading
                      ? `正在读取${previewMode === 'asr' ? '课程 ASR' : ' Notes'}...`
                      : markdownPreview || '当前没有可预览的 Markdown。'}
                  </motion.pre>
                </AnimatePresence>
              </div>
            </div>
          ) : (
            <div className="detail__body">
            <div className="emptyState">从左侧选择一个课程查看 ASR 与 Notes。</div>
            </div>
          )}
        </motion.aside>
      </div>
    </section>
  )
}
