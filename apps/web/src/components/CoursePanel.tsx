/**
 * 课程库面板。
 *
 * 这个组件负责展示“课程层面”的聚合结果：
 *
 * 1. 左侧按课程维度浏览。
 * 2. 右侧展示课程总稿 Markdown 预览。
 * 3. 如果课程还有失败片段，也能在列表里一眼看出来。
 */

import { useEffect, useState } from 'react'
import { getCourseDetail, getCourseMarkdown, listCourses } from '../api'
import type { CourseDetail, CourseSummary } from '../types'

export function CoursePanel() {
  const [courses, setCourses] = useState<CourseSummary[]>([])
  const [selectedCourseKey, setSelectedCourseKey] = useState<string>('')
  const [selectedCourseDetail, setSelectedCourseDetail] = useState<CourseDetail | null>(null)
  const [markdownPreview, setMarkdownPreview] = useState<string>('')
  const [semesterFilter, setSemesterFilter] = useState<string>('')
  const [dateFilter, setDateFilter] = useState<string>('')
  const [courseFilter, setCourseFilter] = useState<string>('')
  const [errorMessage, setErrorMessage] = useState<string>('')

  useEffect(() => {
    let isMounted = true

    const loadCourses = async () => {
      try {
        const nextCourses = await listCourses({
          semester: semesterFilter || undefined,
          date: dateFilter || undefined,
          course_name: courseFilter || undefined,
        })
        if (!isMounted) {
          return
        }

        setCourses(nextCourses)
        setErrorMessage('')
        if (!selectedCourseKey && nextCourses[0]) {
          setSelectedCourseKey(nextCourses[0].course_key)
        }
      } catch (error) {
        if (isMounted) {
          setErrorMessage(error instanceof Error ? error.message : '课程库加载失败')
        }
      }
    }

    void loadCourses()
    const timer = window.setInterval(() => {
      void loadCourses()
    }, 8000)

    return () => {
      isMounted = false
      window.clearInterval(timer)
    }
  }, [semesterFilter, dateFilter, courseFilter, selectedCourseKey])

  useEffect(() => {
    if (!selectedCourseKey) {
      setSelectedCourseDetail(null)
      setMarkdownPreview('')
      return
    }

    let isMounted = true

    const loadCourseDetail = async () => {
      try {
        const detail = await getCourseDetail(selectedCourseKey)
        const markdown = await getCourseMarkdown(selectedCourseKey)
        if (isMounted) {
          setSelectedCourseDetail(detail)
          setMarkdownPreview(markdown)
        }
      } catch (error) {
        if (isMounted) {
          setErrorMessage(error instanceof Error ? error.message : '课程详情加载失败')
        }
      }
    }

    void loadCourseDetail()

    return () => {
      isMounted = false
    }
  }, [selectedCourseKey])

  return (
    <section className="panel">
      <div className="panel__grid">
        <div className="card card--padded">
          <div className="card__header">
            <div>
              <h2>课程库</h2>
              <p>以“课程”而不是“单个任务”的视角审查最终交付物。</p>
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

          {courses.length === 0 ? <div className="emptyState">当前没有课程总稿可展示。</div> : null}
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
