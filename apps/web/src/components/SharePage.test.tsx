import { render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { SharePage } from './SharePage'

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('SharePage', () => {
  it('只展示公开课程资料并能读取 Notes Markdown', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input)
      if (url === '/public/notes/tree') {
        return new Response(
          JSON.stringify({
            files: [
              {
                path: '泌尿生殖系统/Notes/1-急性肾损伤.md',
                course: '泌尿生殖系统',
                kind: 'Notes',
                name: '1-急性肾损伤.md',
              },
              {
                path: '泌尿生殖系统/Audio/10.9-泌尿生殖系统-张老师.md',
                course: '泌尿生殖系统',
                kind: 'Audio',
                name: '10.9-泌尿生殖系统-张老师.md',
              },
            ],
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        )
      }
      if (url.includes('/public/notes/file?path=')) {
        return new Response('# 急性肾损伤\n\n这是共享 Notes。', { status: 200 })
      }
      return new Response('not found', { status: 404 })
    })
    vi.stubGlobal('fetch', fetchMock)

    render(<SharePage />)

    expect(await screen.findByText('泌尿生殖系统')).toBeInTheDocument()
    expect(await screen.findByText('1-急性肾损伤')).toBeInTheDocument()
    await waitFor(() => {
      expect(document.querySelector('.sharePreview')?.textContent).toContain('这是共享 Notes')
    })
    expect(screen.queryByText('任务台')).not.toBeInTheDocument()
    expect(screen.queryByText('SmartClass 登录')).not.toBeInTheDocument()
  })
})
