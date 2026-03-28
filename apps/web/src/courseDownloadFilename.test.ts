import { describe, expect, it } from 'vitest'
import { buildCourseMarkdownDownloadFilename } from './courseDownloadFilename'

describe('buildCourseMarkdownDownloadFilename', () => {
  it('应该按“月.日-课程名-老师”生成课程总稿文件名', () => {
    expect(
      buildCourseMarkdownDownloadFilename({
        date: '2026-03-10',
        courseName: '心理危机干预与预防',
        teacherName: '赵朋',
      }),
    ).toBe('3.10-心理危机干预与预防-赵朋.md')
  })

  it('应该兼容斜杠日期，并继续只保留月日', () => {
    expect(
      buildCourseMarkdownDownloadFilename({
        date: '2026/03/10',
        courseName: '大学英语',
        teacherName: '李老师',
      }),
    ).toBe('3.10-大学英语-李老师.md')
  })

  it('应该把非法文件名字符替换掉，避免浏览器下载名不可保存', () => {
    expect(
      buildCourseMarkdownDownloadFilename({
        date: '2026-03-10',
        courseName: '心理危机:干预/预防',
        teacherName: '赵*朋',
      }),
    ).toBe('3.10-心理危机-干预-预防-赵-朋.md')
  })

  it('应该在日期或老师缺失时给出稳定兜底值，而不是返回空文件名', () => {
    expect(
      buildCourseMarkdownDownloadFilename({
        date: '   ',
        courseName: '形势与政策',
        teacherName: '',
      }),
    ).toBe('未知日期-形势与政策-未知老师.md')
  })
})
