# ClassFlow 接口契约

本文档只描述第一阶段已经实现的 HTTP 契约，方便前端、Worker、userscript、运维脚本联调时对字段名。

## 鉴权

- `/api/v1/health` 不需要鉴权。
- 其余 `/api/v1/*` 路由统一要求：
  - Header: `Authorization: Bearer <CLASSFLOW_BEARER_TOKEN>`

## 1. 提交批量任务

- `POST /api/v1/intake/batches`
- 用途：userscript 把“单节”或“当天全部”的视频片段统一投递到后端。

请求体：

```json
{
  "semester": "2025-2026-2",
  "source": "userscript",
  "items": [
    {
      "new_id": "abc123__seg1",
      "page_url": "https://tmu.smartclass.cn/PlayPages/Video.aspx?NewID=abc123",
      "mp4_url": "https://media.example.com/path/VGA.mp4",
      "course_name": "病理学",
      "teacher_name": "王老师",
      "date": "2026-03-20",
      "start_time": "08:00",
      "end_time": "08:45",
      "raw_title": "病理学 王老师 第二教室 2026-03-20 08:00:00-08:45:00 [片段1/2]"
    }
  ]
}
```

响应体：

```json
{
  "batch_id": "6a2433bc-16e5-43d7-a812-b6fe2b362a46",
  "accepted_count": 1,
  "task_ids": ["10e95f74-1dbf-4806-8c69-58d2f7c51490"],
  "course_keys": ["2025-2026-2|2026-03-20|病理学|王老师"]
}
```

## 2. 查询任务列表

- `GET /api/v1/tasks`
- Query 参数：
  - `status`
  - `date`
  - `course_name`

响应体是任务摘要数组，每项都会返回：

- `id`
- `batch_id`
- `status`
- `stage`
- `semester`
- `course_key`
- `course_name`
- `teacher_name`
- `date`
- `start_time`
- `end_time`
- `last_error`
- `created_at`
- `updated_at`

## 3. 查询单个任务详情

- `GET /api/v1/tasks/:task_id`

响应体：

```json
{
  "task": { "...": "完整任务记录" },
  "events": [
    {
      "id": 1,
      "task_id": "10e95f74-1dbf-4806-8c69-58d2f7c51490",
      "stage": "transcribing",
      "level": "info",
      "message": "开始轮询 DashScope 任务状态",
      "created_at": "2026-03-22T12:30:00Z"
    }
  ]
}
```

## 4. 重试任务

- `POST /api/v1/tasks/:task_id/retry`
- 仅对失败任务生效。

响应体：

```json
{
  "task_id": "10e95f74-1dbf-4806-8c69-58d2f7c51490",
  "status": "requeued"
}
```

## 5. 查询课程聚合

- `GET /api/v1/courses`
- Query 参数：
  - `semester`
  - `date`
  - `course_name`

响应体是课程摘要数组，每项包含：

- `course_key`
- `semester`
- `course_name`
- `teacher_name`
- `date`
- `received_segment_count`
- `successful_segment_count`
- `has_failed_segment`
- `merged_markdown_path`
- `manifest_path`
- `updated_at`

## 6. 查询课程详情

- `GET /api/v1/courses/:course_key`

额外返回：

- `segments`: 该课程下的任务摘要数组

## 7. 读取课程成品

- `GET /api/v1/courses/:course_key/artifacts/course.md`
- `GET /api/v1/courses/:course_key/artifacts/manifest.json`

## 8. 健康检查

- `GET /api/v1/health`

响应体：

```json
{
  "status": "ok",
  "service": "classflow-backend"
}
```
