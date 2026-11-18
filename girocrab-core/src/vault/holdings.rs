//! Manual purchase data per security, and its local/external price history.
//!
//! `holdings` is deleted and reinserted wholesale on every sync (see
//! `connectors::persist::store_fetch`), so anything the user enters by hand,
//! or any price recorded over time, lives here instead – keyed by the
//! security (account + ISIN/WKN for a purchase, ISIN alone for a price),
//! never by a `holdings` row's id.

use rusqlite::{params, OptionalExtension};

use super::Vault;
use crate::error::{CoreError, CoreResult};
use crate::models::*;

impl Vault {
    fn holding_purchase_inner(
        &self,
        account_id: i64,
        isin: &str,
        wkn: &str,
    ) -> CoreResult<Option<HoldingPurchase>> {
        let db = self.db()?;
        db.query_row(
            "SELECT purchased_at, purchase_price_cents, note
             FROM holding_purchases WHERE account_id = ?1 AND isin = ?2 AND wkn = ?3",
            params![account_id, isin, wkn],
            |r| {
                Ok(HoldingPurchase {
                    account_id,
                    isin: isin.to_string(),
                    wkn: wkn.to_string(),
                    purchased_at: r.get(0)?,
                    purchase_price_cents: r.get(1)?,
                    note: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(CoreError::from)
    }
}

#[uniffi::export]
impl Vault {
    /// The manually entered purchase date/price/note for one security held
    /// in one account, if one was entered. `isin` and `wkn` are whichever
    /// the connector gave for this holding; at least one must be non-empty.
    pub fn holding_purchase(
        &self,
        account_id: i64,
        isin: String,
        wkn: String,
    ) -> Result<Option<HoldingPurchase>, CoreError> {
        if isin.trim().is_empty() && wkn.trim().is_empty() {
            return Ok(None);
        }
        self.holding_purchase_inner(account_id, isin.trim(), wkn.trim())
    }

    /// Stores the purchase date/price/note for one security; an empty date,
    /// empty note and no price override together clear the entry. Survives
    /// the next sync, unlike `holdings` itself.
    pub fn set_holding_purchase(
        &self,
        account_id: i64,
        isin: String,
        wkn: String,
        purchased_at: String,
        purchase_price_cents: Option<i64>,
        note: String,
    ) -> Result<(), CoreError> {
        let (isin, wkn) = (isin.trim().to_string(), wkn.trim().to_string());
        if isin.is_empty() && wkn.is_empty() {
            return Err(CoreError::InvalidInput {
                message: "Weder ISIN noch WKN angegeben.".into(),
            });
        }
        let purchased_at = purchased_at.trim().to_string();
        let note = note.trim().to_string();
        let db = self.db()?;
        if purchased_at.is_empty() && note.is_empty() && purchase_price_cents.is_none() {
            db.execute(
                "DELETE FROM holding_purchases WHERE account_id = ?1 AND isin = ?2 AND wkn = ?3",
                params![account_id, isin, wkn],
            )?;
            return Ok(());
        }
        db.execute(
            "INSERT INTO holding_purchases
                (account_id, isin, wkn, purchased_at, purchase_price_cents, note, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'))
             ON CONFLICT(account_id, isin, wkn) DO UPDATE SET
                purchased_at = excluded.purchased_at,
                purchase_price_cents = excluded.purchase_price_cents,
                note = excluded.note,
                updated_at = excluded.updated_at",
            params![
                account_id,
                isin,
                wkn,
                purchased_at,
                purchase_price_cents,
                note
            ],
        )?;
        Ok(())
    }

    /// Price history of one security (ascending by date), combining sync
    /// snapshots and any backfilled external history.
    pub fn security_price_history(
        &self,
        isin: String,
    ) -> Result<Vec<SecurityPricePoint>, CoreError> {
        let isin = isin.trim();
        if isin.is_empty() {
            return Ok(vec![]);
        }
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT price_date, close_price_cents, currency, source
             FROM security_prices WHERE isin = ?1 ORDER BY price_date ASC",
        )?;
        let rows = stmt
            .query_map(params![isin], |r| {
                Ok(SecurityPricePoint {
                    price_date: r.get(0)?,
                    close_price_cents: r.get(1)?,
                    currency: r.get(2)?,
                    source: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Records one day's closing price for a security.
    ///
    /// `source = "sync"` always wins for that day (a fresh sync overwrites
    /// an older sync snapshot or an external backfill for the same day);
    /// any other source only fills in a day that has no row yet, so a
    /// backfill never overwrites a real sync snapshot.
    pub fn record_security_price(
        &self,
        isin: String,
        price_date: String,
        close_price_cents: i64,
        currency: String,
        source: String,
    ) -> Result<(), CoreError> {
        let (isin, price_date) = (isin.trim().to_string(), price_date.trim().to_string());
        if isin.is_empty() || price_date.is_empty() {
            return Ok(());
        }
        let db = self.db()?;
        if source == "sync" {
            db.execute(
                "INSERT INTO security_prices(isin, price_date, close_price_cents, currency, source)
                 VALUES (?1, ?2, ?3, ?4, 'sync')
                 ON CONFLICT(isin, price_date) DO UPDATE SET
                    close_price_cents = excluded.close_price_cents,
                    currency = excluded.currency,
                    source = 'sync'",
                params![isin, price_date, close_price_cents, currency],
            )?;
        } else {
            db.execute(
                "INSERT OR IGNORE INTO security_prices
                    (isin, price_date, close_price_cents, currency, source)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![isin, price_date, close_price_cents, currency, source],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::Vault;

    fn vault(tag: &str) -> Arc<Vault> {
        Vault::open(
            crate::test_support::thread_temp_path(tag)
                .to_string_lossy()
                .into(),
            vec![7u8; 32],
        )
        .unwrap()
    }

    /// A bare account row, just enough to satisfy `holding_purchases`'
    /// foreign key.
    fn account_id(v: &Vault, ext: &str) -> i64 {
        let db = v.db().unwrap();
        db.execute(
            "INSERT INTO account_groups(name, sf_symbol, sort_order) VALUES ('G', 'tray', 1)
             ON CONFLICT DO NOTHING",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO accounts(group_id, external_ref, name, kind)
             VALUES ((SELECT min(id) FROM account_groups), ?1, 'Depot', 'brokerage')",
            [ext],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    #[test]
    fn purchase_round_trips_and_clears() {
        let v = vault("purchase-round-trip");
        let acc = account_id(&v, "A");
        assert!(v
            .holding_purchase(acc, "DE0007164600".into(), "716460".into())
            .unwrap()
            .is_none());

        v.set_holding_purchase(
            acc,
            "DE0007164600".into(),
            "716460".into(),
            "2024-03-01".into(),
            Some(12_345),
            "Erster Kauf".into(),
        )
        .unwrap();
        let p = v
            .holding_purchase(acc, "DE0007164600".into(), "716460".into())
            .unwrap()
            .unwrap();
        assert_eq!(p.purchased_at, "2024-03-01");
        assert_eq!(p.purchase_price_cents, Some(12_345));
        assert_eq!(p.note, "Erster Kauf");

        // Clearing: empty date, empty note, no price.
        v.set_holding_purchase(
            acc,
            "DE0007164600".into(),
            "716460".into(),
            "".into(),
            None,
            "".into(),
        )
        .unwrap();
        assert!(v
            .holding_purchase(acc, "DE0007164600".into(), "716460".into())
            .unwrap()
            .is_none());
    }

    #[test]
    fn neither_isin_nor_wkn_is_rejected() {
        let v = vault("purchase-needs-id");
        let acc = account_id(&v, "A");
        assert!(v
            .set_holding_purchase(
                acc,
                "".into(),
                "".into(),
                "2024-01-01".into(),
                None,
                "".into()
            )
            .is_err());
    }

    #[test]
    fn sync_price_wins_over_backfill_same_day() {
        let v = vault("price-sync-wins");
        v.record_security_price(
            "DE0007164600".into(),
            "2024-03-01".into(),
            100_00,
            "EUR".into(),
            "boerse-frankfurt".into(),
        )
        .unwrap();
        v.record_security_price(
            "DE0007164600".into(),
            "2024-03-01".into(),
            101_00,
            "EUR".into(),
            "sync".into(),
        )
        .unwrap();
        // A later backfill must not overwrite the sync's own value.
        v.record_security_price(
            "DE0007164600".into(),
            "2024-03-01".into(),
            999_00,
            "EUR".into(),
            "boerse-frankfurt".into(),
        )
        .unwrap();

        let hist = v.security_price_history("DE0007164600".into()).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].close_price_cents, 101_00);
        assert_eq!(hist[0].source, "sync");
    }

    #[test]
    fn history_is_sorted_ascending() {
        let v = vault("price-history-order");
        for (date, price) in [("2024-03-03", 3), ("2024-03-01", 1), ("2024-03-02", 2)] {
            v.record_security_price(
                "IE00B4L5Y983".into(),
                date.into(),
                price,
                "EUR".into(),
                "boerse-frankfurt".into(),
            )
            .unwrap();
        }
        let hist = v.security_price_history("IE00B4L5Y983".into()).unwrap();
        let dates: Vec<_> = hist.iter().map(|p| p.price_date.as_str()).collect();
        assert_eq!(dates, ["2024-03-01", "2024-03-02", "2024-03-03"]);
    }
}
