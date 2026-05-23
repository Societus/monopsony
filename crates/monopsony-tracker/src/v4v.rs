//! V4V (Value-for-Value) schema — conditional tables for bounty/governance.
//!
//! Only created when `--v4v-enabled` flag is set at tracker startup.
//! These tables are planned for post-v1.0 and are dormant until then.

use rusqlite::Connection;
use tracing::info;

/// Create V4V-specific tables. Only call when v4v_enabled = true.
pub fn init_v4v_schema(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS bounty_pool (
            id             TEXT PRIMARY KEY,
            sponsor_npub   TEXT NOT NULL REFERENCES operators(npub),
            total_funded   INTEGER NOT NULL,
            available      INTEGER NOT NULL,
            currency       TEXT NOT NULL DEFAULT 'sat',
            created_at     INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            expires_at     INTEGER
        );

        CREATE TABLE IF NOT EXISTS bounty_delegations (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            pool_id        TEXT NOT NULL REFERENCES bounty_pool(id),
            delegator_npub TEXT NOT NULL,
            champion_npub  TEXT NOT NULL,
            amount         INTEGER NOT NULL,
            created_at     INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            revoked_at     INTEGER
        );

        CREATE TABLE IF NOT EXISTS bounty_proposals (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            champion_npub  TEXT NOT NULL,
            package_name   TEXT NOT NULL,
            version        TEXT NOT NULL,
            priority       TEXT NOT NULL,
            bounty_amount  INTEGER NOT NULL,
            delegations_behind INTEGER DEFAULT 0,
            status         TEXT NOT NULL DEFAULT 'proposed',
            created_at     INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );

        CREATE TABLE IF NOT EXISTS audit_badges (
            recipe_id       TEXT NOT NULL,
            auditor_npub    TEXT NOT NULL,
            audit_report_url TEXT NOT NULL,
            scope           TEXT NOT NULL,
            issued_at       INTEGER NOT NULL,
            trust_broker_sig TEXT NOT NULL,
            PRIMARY KEY (recipe_id, auditor_npub)
        );",
    )
    .expect("V4V schema creation failed");
    info!("V4V schema initialized (bounty_pool, bounty_delegations, bounty_proposals, audit_badges)");
}

/// Check if V4V tables exist (i.e., were created at startup).
#[allow(dead_code)]
pub fn v4v_tables_exist(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='bounty_pool'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_v4v_schema_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        // Need operators table for FK reference
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS operators (
                npub TEXT PRIMARY KEY,
                first_seen INTEGER NOT NULL,
                display_name TEXT,
                payment_pointer TEXT,
                reputation REAL NOT NULL DEFAULT 50.0,
                total_uptime INTEGER DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
            );",
        )
        .unwrap();

        assert!(!v4v_tables_exist(&conn));
        init_v4v_schema(&conn);
        assert!(v4v_tables_exist(&conn));

        // Verify bounty_pool is queryable
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM bounty_pool", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify other tables
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM bounty_delegations", [], |row| row.get(0))
            .unwrap();
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM bounty_proposals", [], |row| row.get(0))
            .unwrap();
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_badges", [], |row| row.get(0))
            .unwrap();
    }

    #[test]
    fn test_v4v_tables_not_exist_by_default() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(!v4v_tables_exist(&conn));
    }

    #[test]
    fn test_v4v_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS operators (
                npub TEXT PRIMARY KEY,
                first_seen INTEGER NOT NULL,
                display_name TEXT,
                payment_pointer TEXT,
                reputation REAL NOT NULL DEFAULT 50.0,
                total_uptime INTEGER DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
            );",
        )
        .unwrap();

        init_v4v_schema(&conn);
        init_v4v_schema(&conn); // Should not panic
        assert!(v4v_tables_exist(&conn));
    }
}
