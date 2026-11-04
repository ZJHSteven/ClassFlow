import { describe, expect, it } from 'vitest'

import { buildStagePushPayload, dispatchStageEvent, type StageEventWebhookPayload } from './push'

function event(overrides: Partial<StageEventWebhookPayload> = {}): StageEventWebhookPayload {
  return {
    event_key: 'event-1',
    source_kind: 'notes',
    source_id: 'lecture-1',
    stage: 'notes',
    outcome: 'succeeded',
    title: '实验诊断学 · 2026-10-26 08:00-09:40',
    message: '课堂 Notes 已生成并发布到 GitHub',
    payload: {},
    created_at: '2026-10-27T05:04:36Z',
    ...overrides,
  }
}

describe('ClassFlow Web Push', () => {
  it('整堂 Notes 成功应该生成高信号完成通知', () => {
    const payload = buildStagePushPayload(event(), 'https://classflow.zjhstudio.com')
    expect(payload.notification.title).toBe('ClassFlow · 课堂笔记完成')
    expect(payload.notification.body).toContain('实验诊断学')
    expect(payload.notification.navigate).toContain('source=push')
  })

  it('task 失败通知点击后应该能带回对应任务', () => {
    const payload = buildStagePushPayload(event({
      source_kind: 'task',
      source_id: 'task-404',
      stage: 'downloading',
      outcome: 'failed',
      message: '录播源不存在',
    }), 'https://classflow.zjhstudio.com')

    expect(payload.notification.title).toBe('ClassFlow · 流水线异常')
    expect(payload.notification.navigate).toContain('task=task-404')
  })

  it('低信号内部阶段应该在 Worker 第二道门直接静默', async () => {
    const result = await dispatchStageEvent({}, event({
      source_kind: 'task',
      stage: 'transcribing',
      outcome: 'succeeded',
    }))
    expect(result).toMatchObject({
      accepted: true,
      suppressed: true,
      subscriptions: 0,
      sent: 0,
    })
  })

  it('邮件服务确认后才推送最终完成通知', () => {
    const payload = buildStagePushPayload(event({
      source_kind: 'delivery',
      stage: 'email',
      outcome: 'sent',
    }), 'https://classflow.zjhstudio.com')
    expect(payload.notification.title).toContain('摘要邮件已发送')
  })

  it('计划课程未完成时应发送告警，而非显示Notes成功', () => {
    const payload = buildStagePushPayload(event({
      source_kind: 'schedule',
      source_id: '2026-11-03:医学统计学',
      stage: 'coverage',
      outcome: 'warning',
    }), 'https://classflow.zjhstudio.com')
    expect(payload.notification.title).toContain('课表课程逾期')
  })
})
