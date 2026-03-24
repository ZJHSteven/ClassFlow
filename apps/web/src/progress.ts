/**
 * 统一放置“字节 / 速率 / 百分比 / ETA”相关的前端格式化工具。
 *
 * 这些函数保持纯函数，方便：
 *
 * 1. 任务详情直接显示后端回传的实时进度。
 * 2. 受控下载过程中显示浏览器侧实时下载进度。
 * 3. 测试时单独验证，不和组件渲染逻辑耦合。
 */

export function formatBytes(value?: number | null): string {
  if (value == null || !Number.isFinite(value) || value < 0) {
    return '--'
  }

  if (value < 1024) {
    return `${Math.round(value)} B`
  }

  const units = ['KiB', 'MiB', 'GiB', 'TiB']
  let size = value / 1024
  let unitIndex = 0

  while (size >= 1024 && unitIndex < units.length - 1) {
    size /= 1024
    unitIndex += 1
  }

  const digits = size >= 100 ? 0 : size >= 10 ? 1 : 2
  return `${size.toFixed(digits)} ${units[unitIndex]}`
}

export function formatSpeed(value?: number | null): string {
  if (value == null || !Number.isFinite(value) || value < 0) {
    return '--'
  }

  return `${formatBytes(value)}/s`
}

export function formatPercent(value?: number | null): string {
  if (value == null || !Number.isFinite(value)) {
    return '--'
  }

  return `${Math.max(0, Math.min(100, value)).toFixed(value >= 10 ? 0 : 1)}%`
}

export function formatEta(value?: number | null): string {
  if (value == null || !Number.isFinite(value) || value < 0) {
    return '--'
  }

  const totalSeconds = Math.round(value)
  const hours = Math.floor(totalSeconds / 3600)
  const minutes = Math.floor((totalSeconds % 3600) / 60)
  const seconds = totalSeconds % 60

  if (hours > 0) {
    return `${hours}h ${minutes}m`
  }
  if (minutes > 0) {
    return `${minutes}m ${seconds}s`
  }
  return `${seconds}s`
}

export function normalizePercent(value?: number | null): number | null {
  if (value == null || !Number.isFinite(value)) {
    return null
  }

  return Math.max(0, Math.min(100, value))
}
