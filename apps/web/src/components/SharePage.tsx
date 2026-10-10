/**
 * 面向朋友的只读课堂资料页。
 *
 * 这里刻意不复用管理端 CoursePanel：
 * 1. 分享页只读取公开 GitHub 笔记仓库，不拥有 ClassFlow 后端 token。
 * 2. 页面只提供课程、Audio、Notes 浏览与 Markdown 下载，不出现任务/重试/登录等控制。
 * 3. GitHub 请求由同域 Worker 代理并做短缓存，浏览器不直接消耗 GitHub API 配额。
 */
import { useEffect, useMemo, useState } from 'react'
import { getPublicNoteMarkdown, listPublicNoteFiles, type PublicNoteFile } from '../api'

type PublicKind = 'Notes' | 'Audio'

function sortFiles(left: PublicNoteFile, right: PublicNoteFile) {
  return left.name.localeCompare(right.name, 'zh-CN', { numeric: true })
}

export function SharePage() {
  const [files, setFiles] = useState<PublicNoteFile[]>([])
  const [selectedCourse, setSelectedCourse] = useState('')
  const [kind, setKind] = useState<PublicKind>('Notes')
  const [selectedPath, setSelectedPath] = useState('')
  const [markdown, setMarkdown] = useState('')
  const [isLoading, setIsLoading] = useState(true)
  const [isMarkdownLoading, setIsMarkdownLoading] = useState(false)
  const [error, setError] = useState('')

  useEffect(() => {
    let cancelled = false
    setIsLoading(true)
    listPublicNoteFiles()
      .then((nextFiles) => {
        if (cancelled) return
        setFiles(nextFiles)
        const firstCourse = nextFiles.map((file) => file.course).sort((a, b) => a.localeCompare(b, 'zh-CN'))[0] || ''
        setSelectedCourse(firstCourse)
        setError('')
      })
      .catch((reason: unknown) => {
        if (cancelled) return
        setError(reason instanceof Error ? reason.message : '共享课程库加载失败')
      })
      .finally(() => {
        if (!cancelled) setIsLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  const courses = useMemo(
    () => Array.from(new Set(files.map((file) => file.course))).sort((a, b) => a.localeCompare(b, 'zh-CN')),
    [files],
  )

  const visibleFiles = useMemo(
    () =>
      files
        .filter((file) => file.course === selectedCourse && file.kind === kind)
        .sort(sortFiles),
    [files, kind, selectedCourse],
  )

  useEffect(() => {
    const preferred = visibleFiles[0]?.path || ''
    setSelectedPath(preferred)
  }, [visibleFiles])

  useEffect(() => {
    if (!selectedPath) {
      setMarkdown('')
      return
    }

    let cancelled = false
    setIsMarkdownLoading(true)
    getPublicNoteMarkdown(selectedPath)
      .then((content) => {
        if (!cancelled) {
          setMarkdown(content)
          setError('')
        }
      })
      .catch((reason: unknown) => {
        if (!cancelled) {
          setMarkdown('')
          setError(reason instanceof Error ? reason.message : 'Markdown 加载失败')
        }
      })
      .finally(() => {
        if (!cancelled) setIsMarkdownLoading(false)
      })

    return () => {
      cancelled = true
    }
  }, [selectedPath])

  const selectedFile = files.find((file) => file.path === selectedPath)

  return (
    <div className="shell shareShell">
      <header className="hero shareHero">
        <div className="hero__eyebrow">ClassFlow / Shared Notes</div>
        <div className="hero__content">
          <div>
            <h1>课堂资料共享库</h1>
            <p>这里仅提供自动整理后的课堂 ASR 与 Notes 阅读，不包含 ClassFlow 管理、登录或重试能力。</p>
          </div>
          <div className="hero__badgeList">
            <span>只读</span>
            <span>自动更新</span>
            <span>GitHub 同步源</span>
          </div>
        </div>
      </header>

      <main className="shareLibrary">
        {error ? <div className="detailNotice detailNotice--error">{error}</div> : null}

        <section className="shareControls" aria-label="共享课程选择">
          <div className="shareControls__label">课程</div>
          <div className="shareCourseTabs">
            {courses.map((course) => (
              <button
                type="button"
                key={course}
                className={course === selectedCourse ? 'shareChip is-active' : 'shareChip'}
                onClick={() => setSelectedCourse(course)}
              >
                {course}
              </button>
            ))}
          </div>

          <div className="shareKindTabs">
            {(['Notes', 'Audio'] as const).map((nextKind) => (
              <button
                type="button"
                key={nextKind}
                className={kind === nextKind ? 'tabBar__button is-active' : 'tabBar__button'}
                onClick={() => setKind(nextKind)}
              >
                {nextKind === 'Notes' ? '课堂 Notes' : '课程 ASR'}
              </button>
            ))}
          </div>
        </section>

        <div className="shareGrid">
          <aside className="shareFileList" aria-label="共享文件列表">
            {isLoading ? <div className="emptyState">正在读取共享课程……</div> : null}
            {!isLoading && visibleFiles.length === 0 ? (
              <div className="emptyState">这门课程暂时还没有 {kind}。</div>
            ) : null}
            {visibleFiles.map((file) => (
              <button
                type="button"
                key={file.path}
                className={file.path === selectedPath ? 'shareFileButton is-active' : 'shareFileButton'}
                onClick={() => setSelectedPath(file.path)}
              >
                <strong>{file.name.replace(/\.md$/i, '')}</strong>
                <small>{file.kind === 'Notes' ? '整理笔记' : '完整课堂 ASR'}</small>
              </button>
            ))}
          </aside>

          <section className="shareDocument">
            <div className="shareDocument__toolbar">
              <div>
                <div className="shareDocument__eyebrow">{selectedCourse || '共享课程'}</div>
                <strong>{selectedFile?.name.replace(/\.md$/i, '') || '请选择一份资料'}</strong>
              </div>
              {selectedFile ? (
                <a
                  className="shareDownload"
                  href={'/public/notes/file?path=' + encodeURIComponent(selectedFile.path)}
                  download={selectedFile.name}
                >
                  下载 Markdown
                </a>
              ) : null}
            </div>
            <pre className="markdownPreview sharePreview">
              {isMarkdownLoading ? '正在读取 Markdown……' : markdown || '当前没有可预览内容。'}
            </pre>
          </section>
        </div>
      </main>
    </div>
  )
}
