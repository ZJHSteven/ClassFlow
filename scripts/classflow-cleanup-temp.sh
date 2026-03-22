#!/usr/bin/env bash
#
# ClassFlow 临时目录清理脚本。
#
# 设计目标很单纯：
# 1. 只清 `CLASSFLOW_TEMP_ROOT/jobs` 下过期的任务目录，不碰数据库与正式产物。
# 2. 默认按“最后修改时间超过 N 小时”判断是否过期，N 由环境变量控制。
# 3. 脚本可直接给 systemd timer 调，也可以手工执行做一次性清理。

set -euo pipefail

TEMP_ROOT="${CLASSFLOW_TEMP_ROOT:-/opt/classflow/tmp}"
CLEANUP_HOURS="${CLASSFLOW_TMP_CLEANUP_HOURS:-24}"
JOBS_DIR="${TEMP_ROOT%/}/jobs"

if [[ ! "$CLEANUP_HOURS" =~ ^[0-9]+$ ]]; then
  echo "CLASSFLOW_TMP_CLEANUP_HOURS 必须是非负整数，当前值: $CLEANUP_HOURS" >&2
  exit 1
fi

if [[ ! -d "$JOBS_DIR" ]]; then
  echo "临时任务目录不存在，跳过清理: $JOBS_DIR"
  exit 0
fi

EXPIRE_MINUTES=$((CLEANUP_HOURS * 60))
echo "开始清理 ClassFlow 临时目录: $JOBS_DIR (超过 ${CLEANUP_HOURS}h)"

find "$JOBS_DIR" \
  -mindepth 1 \
  -maxdepth 1 \
  -type d \
  -mmin "+${EXPIRE_MINUTES}" \
  -print \
  -exec rm -rf {} +

echo "临时目录清理完成"
