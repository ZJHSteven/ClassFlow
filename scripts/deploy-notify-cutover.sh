#!/usr/bin/env bash
set -euo pipefail

# ClassFlow iOS Web Push 通知上线的一次性安全切换脚本。
#
# 前置条件：
# - /tmp/classflow-backend.new 已经是通过校验的 ARM64 新 binary；
# - /tmp/repository.rs、/tmp/notifications.rs 是对应源码快照；
# - 当前服务仍使用 /etc/classflow/backend.env。
#
# 关键安全顺序：
# 1. 先停旧 backend，阻止新的 stage_events 在切换窗口继续入库；
# 2. 备份 SQLite / env / binary；
# 3. 把所有历史 pending 标记 suppressed（用户明确要求“不补发历史通知”）；
# 4. 安装新 binary + env；
# 5. 新 backend health 失败时自动恢复旧 binary/env。

DB=/home/zjhsteven/.local/state/classflow/data/classflow.db
APP=/home/zjhsteven/ClassFlow
ENV=/etc/classflow/backend.env
SERVICE=classflow-backend.service
TS="$(date +%Y%m%d-%H%M%S)"
BACKUP="$APP/deploy-backups/notify-$TS"

mkdir -p "$BACKUP"

echo '--- stopping backend ---'
systemctl stop "$SERVICE"

echo '--- backup ---'
cp -a "$DB" "$BACKUP/classflow.db"
cp -a "$ENV" "$BACKUP/backend.env"
cp -a "$APP/target/release/backend" "$BACKUP/backend"

BEFORE="$(sqlite3 "$DB" "SELECT COUNT(*) FROM stage_events WHERE delivery_status='pending';")"
echo "PENDING_BEFORE=$BEFORE"

sqlite3 "$DB" <<'SQL'
BEGIN IMMEDIATE;
UPDATE stage_events
SET delivery_status='suppressed',
    last_error='historical notification suppressed 2026-10-27',
    next_attempt_at=NULL,
    updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
WHERE delivery_status='pending';
COMMIT;
SQL

AFTER="$(sqlite3 "$DB" "SELECT COUNT(*) FROM stage_events WHERE delivery_status='pending';")"
SUPPRESSED="$(sqlite3 "$DB" "SELECT COUNT(*) FROM stage_events WHERE delivery_status='suppressed';")"
echo "PENDING_AFTER=$AFTER"
echo "SUPPRESSED_TOTAL=$SUPPRESSED"

echo '--- install source snapshot + binary ---'
install -o zjhsteven -g zjhsteven -m 664 /tmp/repository.rs "$APP/apps/backend/src/repository.rs"
install -o zjhsteven -g zjhsteven -m 664 /tmp/notifications.rs "$APP/apps/backend/src/notifications.rs"
install -o zjhsteven -g zjhsteven -m 755 /tmp/classflow-backend.new "$APP/target/release/backend.new"
mv -f "$APP/target/release/backend.new" "$APP/target/release/backend"

set_env() {
    local key="$1"
    local value="$2"
    if grep -q "^$key=" "$ENV"; then
        sed -i "s|^$key=.*|$key=$value|" "$ENV"
    else
        printf '%s=%s\n' "$key" "$value" >> "$ENV"
    fi
}

set_env CLASSFLOW_NOTIFICATIONS_ENABLED true
set_env CLASSFLOW_NOTIFICATION_WEBHOOK_URL https://classflow.zjhstudio.com/__classflow/stage-event

rollback() {
    echo '--- rolling back binary/env ---' >&2
    systemctl stop "$SERVICE" || true
    cp -a "$BACKUP/backend" "$APP/target/release/backend"
    cp -a "$BACKUP/backend.env" "$ENV"
    systemctl start "$SERVICE"
}

echo '--- start new backend ---'
if ! systemctl start "$SERVICE"; then
    rollback
    exit 1
fi

sleep 3
if ! curl -fsS http://127.0.0.1:8787/api/v1/health; then
    echo
    echo 'NEW_BACKEND_HEALTH_FAILED=1' >&2
    rollback
    exit 1
fi
echo

echo '--- verify ---'
systemctl is-active "$SERVICE"
systemctl is-enabled "$SERVICE"
sha256sum "$APP/target/release/backend"
grep -E '^CLASSFLOW_(NOTIFICATIONS_ENABLED|NOTIFICATION_WEBHOOK_URL|SMARTCLASS_POLL_INTERVAL_SECS)=' "$ENV"
sqlite3 -header -column "$DB" \
    "SELECT delivery_status,COUNT(*) AS n FROM stage_events GROUP BY delivery_status ORDER BY delivery_status;"
echo "BACKUP=$BACKUP"
