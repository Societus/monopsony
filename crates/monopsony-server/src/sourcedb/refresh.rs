//! Periodic oracle delta generation + publish (Sprint 21 fleet wiring).
//!
//! Every tick: read all floors from the observations table, build a signed
//! snapshot envelope, and publish it into the shared slot the admin
//! well-known endpoint serves. Failure to refresh never clears a previously
//! published delta (clients keep the last good one).

use std::sync::{Arc, RwLock};

use crate::sourcedb::delivery::{sign_snapshot, OracleKeyPair, OracleSnapshot};

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build one signed delta envelope (client wire shape: key_id + sig +
/// snapshot) from the DB. The signature covers the canonical payload of the
/// snapshot exactly as the client's verify_and_merge re-derives it.
async fn refresh_once(
    db: &monopsony_db::Database,
    kp: &OracleKeyPair,
) -> anyhow::Result<(String, Option<i64>)> {
    let floors = db.all_floors().await?;
    let last_resolved = db.oracle_last_resolved().await?;
    let now = now_secs();
    let observations = floors
        .into_iter()
        .map(|r| crate::sourcedb::ObservedVersion {
            package_key: r.package_key,
            source: r.source,
            version: r.version,
            version_ord: r.version_ord,
            scheme: r.scheme,
            observed_at: r.observed_at,
            observed_by: r.observed_by,
        })
        .collect();
    let snapshot = OracleSnapshot {
        expires_at: now + 7 * 86400,
        generated_at: now,
        observations,
        scheme_tables: Vec::new(),
        version: 1,
    };
    let sig_hex = sign_snapshot(kp, &snapshot)?;
    let envelope = serde_json::json!({
        "key_id": "oracle-v1",
        "sig": sig_hex,
        "snapshot": snapshot,
    });
    Ok((envelope.to_string(), last_resolved))
}

/// Spawn the periodic refresh loop. First tick fires immediately (tokio
/// interval semantics), so a delta exists as soon as the server starts with
/// a DB and an oracle key.
pub fn spawn(
    db: Arc<monopsony_db::Database>,
    kp: OracleKeyPair,
    delta_slot: Arc<RwLock<Option<String>>>,
    last_update_slot: Arc<RwLock<Option<i64>>>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(60)));
        loop {
            ticker.tick().await;
            match refresh_once(&db, &kp).await {
                Ok((envelope, last_resolved)) => {
                    *delta_slot.write().unwrap_or_else(|e| e.into_inner()) = Some(envelope);
                    *last_update_slot.write().unwrap_or_else(|e| e.into_inner()) = last_resolved;
                }
                Err(e) => {
                    // Keep the last good delta published; log and retry next tick.
                    tracing::warn!("oracle delta refresh failed: {}", e);
                }
            }
        }
    })
}
