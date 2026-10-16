import { describe, expect, it, vi } from 'vitest'
import { handleWorkerRequest, proxyApiRequest, type WorkerEnv } from './proxy'

class MemoryEmailBinding {
  public readonly sent: Array<{
    to: string
    from: string
    subject: string
    text: string
    html: string
  }> = []

  async send(message: {
    to: string
    from: string
    subject: string
    text: string
    html: string
  }) {
    this.sent.push(message)
    return { messageId: 'mail-demo-1' }
  }
}

class MemoryR2Object {
  constructor(
    private readonly bytes: Uint8Array,
    public readonly httpMetadata?: { contentType?: string },
  ) {}

  async arrayBuffer() {
    return this.bytes.buffer.slice(
      this.bytes.byteOffset,
      this.bytes.byteOffset + this.bytes.byteLength,
    )
  }
}

class MemoryR2Bucket {
  private readonly objects = new Map<string, MemoryR2Object>()

  async get(key: string) {
    return this.objects.get(key) ?? null
  }

  async put(key: string, value: ArrayBuffer, options?: { httpMetadata?: { contentType?: string } }) {
    this.objects.set(key, new MemoryR2Object(new Uint8Array(value), options?.httpMetadata))
  }

  async delete(key: string) {
    this.objects.delete(key)
  }
}

const defaultEnv: WorkerEnv = {
  BACKEND_BASE_URL: 'https://backend.example.com',
  BACKEND_TOKEN: 'secret-token',
  ARTIFACT_PROXY_TOKEN: 'artifact-token',
  ARTIFACTS: new MemoryR2Bucket(),
  ASSETS: {
    fetch: vi.fn(async () => new Response('asset', { status: 200 })),
  },
}

describe('worker proxy', () => {
  it('应该把 /api/* 请求转发到后端并追加 Authorization', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ ok: true }), { status: 200 }))
    vi.stubGlobal('fetch', fetchMock)

    const response = await proxyApiRequest(new Request('https://worker.example.com/api/v1/tasks'), defaultEnv)
    expect(response.status).toBe(200)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    const firstCall = fetchMock.mock.calls[0]
    const targetUrl = String(firstCall?.[0] ?? '')
    const init = (firstCall?.[1] ?? {}) as RequestInit
    expect(targetUrl).toBe('https://backend.example.com/api/v1/tasks')
    expect((init.headers as Headers).get('Authorization')).toBe('Bearer secret-token')
  })

  it('应该在配置了 Access Service Token 时一并追加回源请求头', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ ok: true }), { status: 200 }))
    vi.stubGlobal('fetch', fetchMock)

    const response = await proxyApiRequest(new Request('https://worker.example.com/api/v1/tasks'), {
      ...defaultEnv,
      CF_ACCESS_CLIENT_ID: 'client-id',
      CF_ACCESS_CLIENT_SECRET: 'client-secret',
    })

    expect(response.status).toBe(200)
    const firstCall = fetchMock.mock.calls[0]
    const init = (firstCall?.[1] ?? {}) as RequestInit
    expect((init.headers as Headers).get('CF-Access-Client-Id')).toBe('client-id')
    expect((init.headers as Headers).get('CF-Access-Client-Secret')).toBe('client-secret')
  })

  it('应该拒绝只配置了一半的 Access Service Token', async () => {
    const response = await proxyApiRequest(new Request('https://worker.example.com/api/v1/tasks'), {
      ...defaultEnv,
      CF_ACCESS_CLIENT_ID: 'client-id',
    })

    expect(response.status).toBe(500)
    const body = await response.json()
    expect(body.error).toContain('CF_ACCESS_CLIENT_ID')
  })

  it('应该在非 API 请求时回退到静态资源绑定', async () => {
    const response = await handleWorkerRequest(new Request('https://worker.example.com/'), defaultEnv)
    expect(response.status).toBe(200)
    expect(defaultEnv.ASSETS.fetch).toHaveBeenCalled()
  })

  it('应该把后端 401 掩码成 Worker 层错误', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('unauthorized', { status: 401 })))

    const response = await proxyApiRequest(new Request('https://worker.example.com/api/v1/tasks'), defaultEnv)
    expect(response.status).toBe(502)
    const body = await response.json()
    expect(body.error).toContain('BACKEND_TOKEN')
  })

  it('应该允许后端通过 Worker 私有接口写入并读取 R2 产物', async () => {
    const putResponse = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/artifacts/segments/demo.md', {
        method: 'PUT',
        headers: {
          Authorization: 'Bearer artifact-token',
          'Content-Type': 'text/markdown; charset=utf-8',
        },
        body: '# demo',
      }),
      defaultEnv,
    )
    expect(putResponse.status).toBe(204)

    const getResponse = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/artifacts/segments/demo.md', {
        headers: {
          Authorization: 'Bearer artifact-token',
        },
      }),
      defaultEnv,
    )
    expect(getResponse.status).toBe(200)
    expect(getResponse.headers.get('content-type')).toContain('text/markdown')
    expect(await getResponse.text()).toBe('# demo')
  })

  it('应该拒绝未携带正确私有 token 的产物请求', async () => {
    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/artifacts/segments/demo.md'),
      defaultEnv,
    )
    expect(response.status).toBe(401)
  })

  it('应该只允许 backend token 调用课堂摘要邮件接口并转交 Email Service', async () => {
    const email = new MemoryEmailBinding()
    const env: WorkerEnv = {
      ...defaultEnv,
      SUMMARY_EMAIL: email,
      SUMMARY_EMAIL_TO: 'verified@example.com',
    }
    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/email-summary', {
        method: 'POST',
        headers: {
          Authorization: 'Bearer secret-token',
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          lecture_key: 'lecture-demo',
          course_name: '流行病学',
          teacher_name: '赵老师',
          date: '2026-10-15',
          start_time: '15:30',
          end_time: '17:10',
          title: '筛检试验评价',
          summary: '## 通知与待办\n**下周小测**。\n\n- 范围：筛检试验评价\n\n## 本堂讲了什么\n灵敏度与特异度。\n\n<script>alert(1)</script>',
        }),
      }),
      env,
    )
    expect(response.status).toBe(200)
    expect(email.sent).toHaveLength(1)
    expect(email.sent[0]?.from).toBe('classflow@zjhstudio.com')
    expect(email.sent[0]?.subject).toContain('流行病学')
    expect(email.sent[0]?.text).toContain('**下周小测**')
    const html = email.sent[0]?.html ?? ''
    expect(html).toContain('<h2>通知与待办</h2>')
    expect(html).toContain('<strong>下周小测</strong>')
    expect(html).toContain('<ul>')
    expect(html).toContain('<h2>本堂讲了什么</h2>')
    expect(html).not.toContain('## 通知与待办')
    expect(html).not.toContain('**下周小测**')
    expect(html).not.toContain('<script>')
    expect(html).toContain('&lt;script&gt;alert(1)&lt;/script&gt;')
    expect(await response.json()).toEqual({ ok: true, message_id: 'mail-demo-1' })
  })

  it('应该按 CJK 书写习惯解析紧邻中文标点的强调与删除线，同时保持普通 Markdown 正常', async () => {
    const email = new MemoryEmailBinding()
    const env: WorkerEnv = {
      ...defaultEnv,
      SUMMARY_EMAIL: email,
      SUMMARY_EMAIL_TO: 'verified@example.com',
    }
    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/email-summary', {
        method: 'POST',
        headers: {
          Authorization: 'Bearer secret-token',
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          lecture_key: 'lecture-cjk-markdown',
          course_name: '泌尿生殖系统',
          teacher_name: '任老师',
          date: '2026-10-16',
          start_time: '08:00',
          end_time: '09:40',
          title: '利尿药与子宫兴奋药',
          summary: [
            '本堂主要围绕**利尿药（diuretics）**和**子宫平滑肌兴奋药（uterine stimulants）**展开。',
            '这是**非常重要的一句话。**然后继续说。',
            '这是**「关键内容」**需要掌握。',
            '中文~~删除。~~继续。',
            'ordinary **English bold** text；**bold *nested italic* bold**。',
            '[**bold link**](https://example.com)；`**not bold**`。',
          ].join('\n\n'),
        }),
      }),
      env,
    )

    expect(response.status).toBe(200)
    expect(email.sent).toHaveLength(1)
    const html = email.sent[0]?.html ?? ''

    // 真实故障：原 CommonMark 会留下两端 **，并只把“和”加粗。
    expect(html).toContain(
      '<strong>利尿药（diuretics）</strong>和<strong>子宫平滑肌兴奋药（uterine stimulants）</strong>展开。',
    )
    expect(html).not.toContain('**利尿药')
    expect(html).not.toContain('<strong>和</strong>')

    // 中文全角句号/引号紧邻后文时仍应正常闭合。
    expect(html).toContain('<strong>非常重要的一句话。</strong>然后继续说。')
    expect(html).toContain('<strong>「关键内容」</strong>需要掌握。')

    // markdown-it 的删除线同样使用 delimiter 扫描，CJK-friendly 应同步修正。
    expect(html).toContain('中文<s>删除。</s>继续。')

    // 非 CJK 常规 Markdown 行为不能被破坏。
    expect(html).toContain('ordinary <strong>English bold</strong> text')
    expect(html).toContain('<strong>bold <em>nested italic</em> bold</strong>')
    expect(html).toContain('<a href="https://example.com"><strong>bold link</strong></a>')
    expect(html).toContain('<code>**not bold**</code>')
  })

  it('应该拒绝未授权或字段不完整的课堂摘要邮件请求', async () => {
    const email = new MemoryEmailBinding()
    const env: WorkerEnv = {
      ...defaultEnv,
      SUMMARY_EMAIL: email,
      SUMMARY_EMAIL_TO: 'verified@example.com',
    }
    const unauthorized = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/email-summary', {
        method: 'POST',
        body: '{}',
      }),
      env,
    )
    expect(unauthorized.status).toBe(401)

    const invalid = await handleWorkerRequest(
      new Request('https://worker.example.com/__classflow/email-summary', {
        method: 'POST',
        headers: { Authorization: 'Bearer secret-token' },
        body: JSON.stringify({ summary: '只有摘要' }),
      }),
      env,
    )
    expect(invalid.status).toBe(400)
    expect(email.sent).toHaveLength(0)
  })

  it('应该从公开 GitHub tree 只暴露 Audio 与 Notes Markdown', async () => {
    const fetchMock = vi.fn(async () =>
      new Response(
        JSON.stringify({
          tree: [
            { path: '泌尿生殖系统/Notes/1-测试.md', type: 'blob' },
            { path: '泌尿生殖系统/Audio/10.1-测试.md', type: 'blob' },
            { path: 'README.md', type: 'blob' },
            { path: '泌尿生殖系统/private.json', type: 'blob' },
          ],
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      ),
    )
    vi.stubGlobal('fetch', fetchMock)

    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/public/notes/tree'),
      defaultEnv,
    )
    expect(response.status).toBe(200)
    const body = (await response.json()) as { files: Array<{ path: string }> }
    expect(body.files.map((file) => file.path)).toEqual([
      '泌尿生殖系统/Notes/1-测试.md',
      '泌尿生殖系统/Audio/10.1-测试.md',
    ])
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect(String(fetchMock.mock.calls[0]?.[0] ?? '')).toContain('/git/trees/main?recursive=1')
  })

  it('公开 GitHub tree 瞬时 5xx 时应该短退避重试后恢复', async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(new Response('temporary upstream error', { status: 502 }))
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            tree: [{ path: '医学统计学/Notes/1-统计流程与数据分类.md', type: 'blob' }],
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
      )
    vi.stubGlobal('fetch', fetchMock)

    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/public/notes/tree'),
      defaultEnv,
    )

    expect(response.status).toBe(200)
    expect(fetchMock).toHaveBeenCalledTimes(2)
    const body = (await response.json()) as { files: Array<{ path: string }> }
    expect(body.files.map((file) => file.path)).toEqual([
      '医学统计学/Notes/1-统计流程与数据分类.md',
    ])
  })

  it('应该拒绝公开 Markdown 路径穿越', async () => {
    const fetchMock = vi.fn()
    vi.stubGlobal('fetch', fetchMock)

    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/public/notes/file?path=../secret.md'),
      defaultEnv,
    )
    expect(response.status).toBe(400)
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('应该直接通过 Worker 绑定的 R2 返回课程总稿，避免再次回打私有 Worker 接口', async () => {
    await defaultEnv.ARTIFACTS?.put(
      'courses/demo/course.md',
      new TextEncoder().encode('# cached course').buffer,
      {
        httpMetadata: {
          contentType: 'text/markdown; charset=utf-8',
        },
      },
    )

    const fetchMock = vi.fn(async () =>
      new Response(
        JSON.stringify({
          merged_markdown_path: 'courses/demo/course.md',
          manifest_path: 'courses/demo/manifest.json',
        }),
        {
          status: 200,
          headers: {
            'content-type': 'application/json; charset=utf-8',
          },
        },
      ),
    )
    vi.stubGlobal('fetch', fetchMock)

    const response = await handleWorkerRequest(
      new Request('https://worker.example.com/api/v1/courses/demo%7Ccourse/artifacts/course.md'),
      defaultEnv,
    )

    expect(response.status).toBe(200)
    expect(await response.text()).toBe('# cached course')
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect(String(fetchMock.mock.calls[0]?.[0] ?? '')).toBe('https://backend.example.com/api/v1/courses/demo%7Ccourse')
  })
})
