/**
 * 课程总稿下载文件名工具。
 *
 * 这个文件专门负责“浏览器下载时显示给用户看的文件名”，
 * 不参与后端真实存储路径、课程归组键、数据库字段计算。
 *
 * 这样拆开有两个直接好处：
 *
 * 1. 命名规则以后再改时，只需要改这一处，不会误伤 API 与存储层。
 * 2. 可以把边界情况单独写成单元测试，而不是把所有判断都塞进组件 JSX 附近。
 */

/**
 * Windows 和常见浏览器下载文件名里不适合直接出现的非法字符。
 *
 * 这里统一替换成短横线，避免老师名、课程名里一旦出现特殊符号，
 * 浏览器保存时出现“文件名被截断”或“系统拒绝保存”的问题。
 */
const INVALID_FILENAME_CHARACTERS = /[\\/:*?"<>|]/g

export interface CourseDownloadFilenameInput {
  date: string
  courseName: string
  teacherName: string
}

/**
 * 生成“课程总稿”的下载文件名。
 *
 * 输入：
 * - `date`：原始课程日期，通常是 `YYYY-MM-DD`。
 * - `courseName`：课程名称。
 * - `teacherName`：老师姓名。
 *
 * 输出：
 * - 符合旧习惯的下载名：`月.日-课程名-老师.md`。
 *
 * 核心约束：
 *
 * 1. 下载名里的日期只保留月和日，不展示年份。
 * 2. 真实数据里的完整日期仍保留在别处，这里只处理“展示给下载目录看的名字”。
 * 3. 即使日期为空、老师名缺失，函数也要产出一个可保存的兜底文件名。
 */
export function buildCourseMarkdownDownloadFilename(input: CourseDownloadFilenameInput): string {
  const datePart = formatCourseDateForDownload(input.date)
  const courseNamePart = sanitizeFilenameSegment(input.courseName, '未命名课程')
  const teacherNamePart = sanitizeFilenameSegment(input.teacherName, '未知老师')
  return `${datePart}-${courseNamePart}-${teacherNamePart}.md`
}

/**
 * 把后端完整日期压缩成“月.日”。
 *
 * 优先原因：
 *
 * 1. 后端主格式本来就是 `YYYY-MM-DD`，正则解析最稳，不会受时区影响。
 * 2. 如果后续碰到 `/`、`.` 之类的兼容格式，也尽量提取出月日。
 * 3. 真遇到完全无法识别的日期，就回退成一个可读兜底值，而不是返回空串。
 */
function formatCourseDateForDownload(rawDate: string): string {
  const normalizedDate = rawDate.trim()

  if (!normalizedDate) {
    return '未知日期'
  }

  const simpleMatch = normalizedDate.match(/^(\d{4})[-/.](\d{1,2})[-/.](\d{1,2})$/)
  if (simpleMatch) {
    return `${Number(simpleMatch[2])}.${Number(simpleMatch[3])}`
  }

  const parsedDate = new Date(normalizedDate)
  if (!Number.isNaN(parsedDate.getTime())) {
    return `${parsedDate.getMonth() + 1}.${parsedDate.getDate()}`
  }

  return sanitizeFilenameSegment(normalizedDate, '未知日期')
}

/**
 * 清洗文件名片段。
 *
 * 这里只做非常克制的处理：
 *
 * 1. 去首尾空白。
 * 2. 把非法文件名字符替换成 `-`。
 * 3. 把连续空白压成一个空格，避免下载目录里出现一串不可读空格。
 * 4. 如果最终还是空串，则使用调用方提供的兜底值。
 */
function sanitizeFilenameSegment(rawValue: string, fallback: string): string {
  const cleanedValue = rawValue
    .trim()
    .replace(INVALID_FILENAME_CHARACTERS, '-')
    .replace(/\s+/g, ' ')
    .trim()

  return cleanedValue || fallback
}
