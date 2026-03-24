/**
 * 统一封装“受控下载”。
 *
 * 为什么这里不能继续直接用 `<a href download>`：
 *
 * 1. 裸链接只能把请求交给浏览器，组件本身拿不到“已经下载了多少字节”。
 * 2. 一旦后端响应比较慢，用户会觉得“按钮点了没反应”。
 * 3. 我们需要在按钮旁边显示下载中、下载百分比、当前速率，以及失败提示。
 *
 * 因此，这里改成：
 *
 * 1. 前端主动 `fetch` 文件流。
 * 2. 一边读取 `ReadableStream`，一边计算进度与速率。
 * 3. 最后再把 Blob 交给浏览器保存。
 */

export interface DownloadProgressSnapshot {
  receivedBytes: number
  totalBytes: number | null
  progressPercent: number | null
  speedBytesPerSec: number | null
}

export async function downloadWithProgress(
  url: string,
  preferredFilename: string,
  onProgress: (snapshot: DownloadProgressSnapshot) => void,
): Promise<void> {
  const response = await fetch(url)
  if (!response.ok) {
    throw new Error(await readDownloadError(response))
  }

  const totalBytes = readContentLength(response.headers.get('content-length'))
  const suggestedFilename = readFilenameFromHeaders(response.headers.get('content-disposition')) || preferredFilename

  if (!response.body) {
    const blob = await response.blob()
    onProgress({
      receivedBytes: blob.size,
      totalBytes: blob.size,
      progressPercent: 100,
      speedBytesPerSec: null,
    })
    saveBlob(blob, suggestedFilename)
    return
  }

  const reader = response.body.getReader()
  const chunks: ArrayBuffer[] = []
  let receivedBytes = 0
  const startedAt = performance.now()

  onProgress({
    receivedBytes: 0,
    totalBytes,
    progressPercent: totalBytes ? 0 : null,
    speedBytesPerSec: 0,
  })

  let isReading = true
  while (isReading) {
    const { done, value } = await reader.read()
    if (done) {
      isReading = false
      continue
    }

    const normalizedChunk = new Uint8Array(value.byteLength)
    normalizedChunk.set(value)
    chunks.push(normalizedChunk.buffer)
    receivedBytes += value.byteLength

    const elapsedSeconds = Math.max((performance.now() - startedAt) / 1000, 0.001)
    const speedBytesPerSec = Math.round(receivedBytes / elapsedSeconds)
    const progressPercent =
      totalBytes && totalBytes > 0 ? Math.min((receivedBytes / totalBytes) * 100, 100) : null

    onProgress({
      receivedBytes,
      totalBytes,
      progressPercent,
      speedBytesPerSec,
    })
  }

  const blob = new Blob(chunks)
  onProgress({
    receivedBytes,
    totalBytes: totalBytes ?? receivedBytes,
    progressPercent: 100,
    speedBytesPerSec: Math.round(receivedBytes / Math.max((performance.now() - startedAt) / 1000, 0.001)),
  })
  saveBlob(blob, suggestedFilename)
}

async function readDownloadError(response: Response): Promise<string> {
  try {
    const body = (await response.json()) as { error?: string }
    if (body.error) {
      return body.error
    }
  } catch {
    // 这里不额外抛错，因为下面还有 HTTP 状态码兜底。
  }

  return `下载失败，HTTP ${response.status}`
}

function readContentLength(rawValue: string | null): number | null {
  if (!rawValue) {
    return null
  }

  const parsed = Number(rawValue)
  return Number.isFinite(parsed) && parsed > 0 ? parsed : null
}

function readFilenameFromHeaders(contentDisposition: string | null): string | null {
  if (!contentDisposition) {
    return null
  }

  const utf8Match = contentDisposition.match(/filename\*=UTF-8''([^;]+)/i)
  if (utf8Match?.[1]) {
    return decodeURIComponent(utf8Match[1])
  }

  const plainMatch = contentDisposition.match(/filename="?([^";]+)"?/i)
  return plainMatch?.[1] ?? null
}

function saveBlob(blob: Blob, filename: string) {
  const blobUrl = URL.createObjectURL(blob)
  const anchor = document.createElement('a')
  anchor.href = blobUrl
  anchor.download = filename
  anchor.click()
  URL.revokeObjectURL(blobUrl)
}
