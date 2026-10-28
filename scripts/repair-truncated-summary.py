#!/usr/bin/env python3
"""只修复一条已确认截断的 ClassFlow 摘要，并重新入队邮件。

运行前必须独立备份线上 SQLite（例如 sqlite3 的 .backup），核对这条课堂
的原始 Archive final 回复；绝不重新发送 ChatGPT prompt，也不重写笔记/GitHub。
默认仅预检查；显式 --apply 才会在一个 SQLite 事务里更新摘要及邮件 checkpoint。
"""

import argparse
import hashlib
import json
import sqlite3
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--payload", type=Path, required=True)
    parser.add_argument("--lecture-key", required=True)
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()

    payload = json.loads(args.payload.read_text(encoding="utf-8"))
    lecture_key = str(payload.get("lecture_key", ""))
    summary = str(payload.get("text", ""))
    ordinal = payload.get("summary_final_ordinal")
    if lecture_key != args.lecture_key:
        raise SystemExit("拒绝操作：指定 lecture_key 与修复文件不一致")
    if not isinstance(ordinal, int) or ordinal <= 0:
        raise SystemExit("拒绝操作：缺少完整回复的正整数 ordinal")
    if not (120 <= len(summary) <= 2000) or "## 本堂讲了什么" not in summary:
        raise SystemExit("拒绝操作：完整摘要不符合 ClassFlow v2 模板")

    # 只允许修复这次实际事故中六字符半成品，已修好的记录重复执行会失败，
    # 防止二次清空邮件 checkpoint 造成重复投递。
    uri = f"file:{args.db.resolve().as_posix()}?mode={'rw' if args.apply else 'ro'}"
    with sqlite3.connect(uri, uri=True, timeout=15) as db:
        db.execute("PRAGMA busy_timeout=15000")
        if args.apply:
            db.execute("BEGIN IMMEDIATE")
        original = db.execute(
            """
            SELECT status, summary_prompt_version, summary_markdown,
                   summary_final_ordinal, summary_email_sent_at
              FROM lecture_notes WHERE lecture_key=?
            """, (lecture_key,)
        ).fetchone()
        if not original:
            raise SystemExit("拒绝操作：数据库不存在指定课堂")
        status, version, existing, old_ordinal, sent_at = original
        if (status, version, existing) != ("succeeded", 2, "## 通知与"):
            raise SystemExit("拒绝操作：原数据不是已确认的 6 字符截断故障")
        if sent_at is None or not isinstance(old_ordinal, int) or ordinal <= old_ordinal:
            raise SystemExit("拒绝操作：既有邮件或消息顺序不符合预期")
        print(json.dumps({
            "mode": "apply" if args.apply else "dry-run",
            "lecture_key": lecture_key,
            "old_chars": len(existing),
            "new_chars": len(summary),
            "new_sha256": hashlib.sha256(summary.encode("utf-8")).hexdigest(),
            "old_ordinal": old_ordinal,
            "new_ordinal": ordinal,
            "previous_email_sent": True,
        }, ensure_ascii=False))
        if not args.apply:
            return
        # status=succeeded 不变，GitHub 提交、讲义、原课堂音频均不触碰。
        # 邮件 Worker 只会在 summary_email_sent_at=NULL 时重新发送。
        changed = db.execute(
            """
            UPDATE lecture_notes
               SET summary_markdown=?, summary_final_ordinal=?,
                   summary_email_sent_at=NULL,
                   summary_email_message_id=NULL,
                   summary_email_attempt_count=0,
                   summary_email_last_error=NULL,
                   summary_email_next_attempt_at=NULL
             WHERE lecture_key=? AND status='succeeded'
               AND summary_prompt_version=2 AND summary_markdown='## 通知与'
               AND summary_email_sent_at IS NOT NULL
            """,
            (summary, ordinal, lecture_key),
        )
        if changed.rowcount != 1:
            db.rollback()
            raise SystemExit("拒绝操作：并发状态变化，未写入任何修改")
        db.commit()
        print("已提交事务：仅此课堂摘要恢复完整，并将邮件放回待发送队列")


if __name__ == "__main__":
    main()
