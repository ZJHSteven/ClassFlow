/**
 * 前端统一类型定义。
 *
 * 这些类型与后端 API 的响应结构保持一一对应，
 * 这样一旦后端字段变化，TypeScript 会第一时间提醒前端同步修正。
 */

export type TaskStatus = 'pending' | 'running' | 'succeeded' | 'failed'

export type TaskStage =
  | 'queued'
  | 'downloading'
  | 'extracting_audio'
  | 'uploading_audio'
  | 'transcribing'
  | 'storing_artifacts'
  | 'merging_course'
  | 'cleanup'
  | 'done'

export interface TaskSummary {
  id: string
  batch_id: string
  status: TaskStatus
  stage: TaskStage
  semester: string
  course_key: string
  course_name: string
  teacher_name: string
  date: string
  start_time: string
  end_time: string
  last_error: string | null
  created_at: string
  updated_at: string
}

export interface TaskEventRecord {
  id: number
  task_id: string
  stage: string
  level: string
  message: string
  created_at: string
}

export interface TaskDetail {
  task: {
    id: string
    batch_id: string
    status: TaskStatus
    stage: TaskStage
    semester: string
    course_key: string
    course_name: string
    teacher_name: string
    date: string
    start_time: string
    end_time: string
    new_id: string
    page_url: string
    mp4_url: string
    last_error: string | null
    created_at: string
    updated_at: string
  }
  events: TaskEventRecord[]
}

export interface CourseSummary {
  course_key: string
  semester: string
  course_name: string
  teacher_name: string
  date: string
  received_segment_count: number
  successful_segment_count: number
  has_failed_segment: boolean
  merged_markdown_path: string | null
  manifest_path: string | null
  updated_at: string
}

export interface CourseDetail extends CourseSummary {
  segments: TaskSummary[]
}
