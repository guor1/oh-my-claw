//! 前向迁移（设计 §3.2）。
//!
//! `user_version` PRAGMA 记录版本；启动时逐步升级；**只前不后**（不写 down）。
//! 每个版本步进用事务包裹。

use rusqlite::Connection;

use crate::error::{StoreError, StoreResult};
use crate::schema;

/// 当前目标 schema 版本。新增迁移时 +1 并在 [`step`] 中追加分支。
pub const TARGET_VERSION: u32 = 3;

/// 从当前 `user_version` 前向迁移到 [`TARGET_VERSION`]。
pub fn run_migrations(conn: &mut Connection) -> StoreResult<()> {
    let mut current: u32 = {
        let v: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        v as u32
    };

    if current > TARGET_VERSION {
        return Err(StoreError::Migration(format!(
            "db version {current} is newer than target {TARGET_VERSION}; downgrade unsupported"
        )));
    }

    while current < TARGET_VERSION {
        let next = current + 1;
        let tx = conn.transaction()?;
        step(&tx, next)?;
        // user_version 不能用参数绑定，需内联；next 由内部控制，无注入风险。
        tx.pragma_update(None, "user_version", next as i64)?;
        tx.commit()?;
        current = next;
    }

    Ok(())
}

/// 应用单个版本步进。
fn step(conn: &Connection, version: u32) -> StoreResult<()> {
    match version {
        1 => {
            conn.execute_batch(schema::V1)?;
            #[cfg(feature = "sqlite-vec")]
            conn.execute_batch(schema::V1_VEC)?;
            Ok(())
        }
        // P1-3：memory 加 pref_key（偏好主题），供 supersede 查同主题既有项。
        2 => {
            conn.execute_batch(schema::V2)?;
            Ok(())
        }
        // FEAT-3：memory 加 source（来源追溯）。
        3 => {
            conn.execute_batch(schema::V3)?;
            Ok(())
        }
        other => Err(StoreError::Migration(format!(
            "no migration step defined for version {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent_on_reopen() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        // 再跑一次：已在目标版本，应为 no-op 不报错。
        run_migrations(&mut conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v as u32, TARGET_VERSION);
    }

    #[test]
    fn rejects_future_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", (TARGET_VERSION + 1) as i64)
            .unwrap();
        assert!(run_migrations(&mut conn).is_err());
    }

    /// v1 老库升到 v2：既有数据必须完好，新列为 NULL。
    ///
    /// 这是用户机上真实发生的路径（P1-3 之前建的库）。若 v2 迁移写坏，
    /// 用户升级后会丢记忆——比功能不可用严重得多，故单独锁住。
    #[test]
    fn migration_v1_to_v2_preserves_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();

        // 造一个只到 v1 的库。
        {
            let tx = conn.transaction().unwrap();
            step(&tx, 1).unwrap();
            tx.pragma_update(None, "user_version", 1i64).unwrap();
            tx.commit().unwrap();
        }
        // v1 时期写入的一条记忆（那时还没有 pref_key 列）。
        conn.execute(
            "INSERT INTO memory(id, tier, origin, text, importance, created_at, content_hash)
             VALUES('old-1', 'curated', 'owner', '我用 VS Code', 0.8, 1000, 'h1')",
            [],
        )
        .unwrap();

        // 升级到最新。
        run_migrations(&mut conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v as u32, TARGET_VERSION, "应升到目标版本");

        // 既有行仍在、内容未变、新列为 NULL。
        let (text, pref): (String, Option<String>) = conn
            .query_row(
                "SELECT text, pref_key FROM memory WHERE id = 'old-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("v1 时期的行升级后必须还在");
        assert_eq!(text, "我用 VS Code", "既有内容不得被迁移改动");
        assert_eq!(pref, None, "新列对既有行应为 NULL（非偏好类，走原去重路径）");
    }

    /// v2 老库升到 v3：既有数据完好，source 新列为 NULL。
    #[test]
    fn migration_v2_to_v3_preserves_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();

        // 造一个到 v2 的库。
        {
            let tx = conn.transaction().unwrap();
            step(&tx, 1).unwrap();
            tx.pragma_update(None, "user_version", 1i64).unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = conn.transaction().unwrap();
            step(&tx, 2).unwrap();
            tx.pragma_update(None, "user_version", 2i64).unwrap();
            tx.commit().unwrap();
        }
        // v2 时期写入的一条记忆（那时还没有 source 列）。
        conn.execute(
            "INSERT INTO memory(id, tier, origin, text, importance, created_at, content_hash)
             VALUES('old-2', 'episodic', 'agent', '关于某次调试的情节', 0.6, 2000, 'h2')",
            [],
        )
        .unwrap();

        run_migrations(&mut conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v as u32, TARGET_VERSION, "应升到目标版本");

        let (text, source): (String, Option<String>) = conn
            .query_row(
                "SELECT text, source FROM memory WHERE id = 'old-2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("v2 时期的行升级后必须还在");
        assert_eq!(text, "关于某次调试的情节", "既有内容不得被迁移改动");
        assert_eq!(source, None, "source 新列对既有行应为 NULL");
    }
}
