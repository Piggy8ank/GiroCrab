//! Depot evaluations built on top of `holdings`, `holding_purchases` and
//! `security_prices` (see `vault/holdings.rs` and `schema::HOLDING_EXTRAS`):
//! portfolio value over time, allocation by position, per-position gain
//! since purchase, and a dividend-payment heuristic over bookings.

use rusqlite::params;

use super::Vault;
use crate::error::CoreError;
use crate::models::*;

#[uniffi::export]
impl Vault {
    /// The depot's total value on each day in `from`..`to` (inclusive, ISO
    /// `YYYY-MM-DD`) for which at least one held security has a recorded
    /// price. Each day's value is `sum(quantity * that day's close)` over
    /// positions whose purchase date (if entered) is on or before that day
    /// – positions without an entered purchase date count on every day,
    /// since we don't know when they were bought.
    ///
    /// This assumes today's quantity was held throughout the period (no
    /// historical buy/sell quantities are tracked), so it's an
    /// approximation once a position's size has changed – good enough for
    /// a "how did my depot do" chart, not for tax reporting.
    pub fn portfolio_value_over_time(
        &self,
        account_id: i64,
        from: String,
        to: String,
    ) -> Result<Vec<PortfolioValuePoint>, CoreError> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT sp.price_date, SUM(h.quantity * sp.close_price_cents)
             FROM holdings h
             JOIN security_prices sp ON sp.isin = h.isin AND h.isin != ''
             LEFT JOIN holding_purchases hp
               ON hp.account_id = h.account_id AND hp.isin = h.isin AND hp.wkn = h.wkn
             WHERE h.account_id = ?1
               AND sp.price_date BETWEEN ?2 AND ?3
               AND (hp.purchased_at IS NULL OR hp.purchased_at = '' OR hp.purchased_at <= sp.price_date)
             GROUP BY sp.price_date
             ORDER BY sp.price_date ASC",
        )?;
        let rows = stmt
            .query_map(params![account_id, from, to], |r| {
                Ok(PortfolioValuePoint {
                    date: r.get(0)?,
                    value_cents: r.get::<_, f64>(1)?.round() as i64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Each held position's share of the depot's current total value
    /// (`holdings.value_cents`, as last synced), largest first.
    pub fn depot_allocation(&self, account_id: i64) -> Result<Vec<PositionAllocation>, CoreError> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT isin, wkn, name, value_cents FROM holdings
             WHERE account_id = ?1 AND value_cents > 0
             ORDER BY value_cents DESC",
        )?;
        let mut rows = stmt
            .query_map(params![account_id], |r| {
                Ok(PositionAllocation {
                    isin: r.get(0)?,
                    wkn: r.get(1)?,
                    name: r.get(2)?,
                    value_cents: r.get(3)?,
                    share: 0.0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let total: i64 = rows.iter().map(|p| p.value_cents).sum();
        if total > 0 {
            for p in &mut rows {
                p.share = p.value_cents as f64 / total as f64;
            }
        }
        Ok(rows)
    }

    /// Gain/loss of one position since its purchase: the manually entered
    /// purchase price if there is one, else the price the connector itself
    /// reported (`holdings.purchase_price_cents`), else unknown.
    pub fn position_performance(
        &self,
        account_id: i64,
        isin: String,
        wkn: String,
    ) -> Result<PositionPerformance, CoreError> {
        let (isin, wkn) = (isin.trim().to_string(), wkn.trim().to_string());
        let (name_isin, name_wkn, quantity, current_price_cents, synced_purchase_price): (
            String,
            String,
            f64,
            i64,
            i64,
        ) = {
            // scoped: `holding_purchase` below takes the same lock, and
            // it isn't reentrant.
            let db = self.db()?;
            db.query_row(
                "SELECT isin, wkn, quantity, current_price_cents, purchase_price_cents
                 FROM holdings WHERE account_id = ?1 AND isin = ?2 AND wkn = ?3",
                params![account_id, isin, wkn],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?
        };
        let manual = self.holding_purchase(account_id, isin.clone(), wkn.clone())?;
        let purchased_at = manual
            .as_ref()
            .map(|m| m.purchased_at.clone())
            .unwrap_or_default();
        let purchase_price_cents =
            manual
                .and_then(|m| m.purchase_price_cents)
                .or(if synced_purchase_price > 0 {
                    Some(synced_purchase_price)
                } else {
                    None
                });
        let (gain_cents, gain_percent) = match purchase_price_cents {
            Some(p) if p != 0 => {
                let gain = ((current_price_cents - p) as f64 * quantity).round() as i64;
                let pct = (current_price_cents - p) as f64 / p as f64;
                (Some(gain), Some(pct))
            }
            _ => (None, None),
        };
        Ok(PositionPerformance {
            isin: name_isin,
            wkn: name_wkn,
            purchased_at,
            purchase_price_cents,
            current_price_cents,
            quantity,
            gain_cents,
            gain_percent,
        })
    }

    /// Bookings on `account_id` between `from`/`to` (inclusive, ISO
    /// `YYYY-MM-DD`) that look like a dividend or fund distribution: a
    /// credit whose purpose, booking text or counterparty mentions
    /// "Dividende"/"dividend"/"Ausschüttung". A heuristic, not a precise
    /// per-security breakdown – bookings don't carry an ISIN, so this
    /// can't be joined back to a specific holding, and a bank wording this
    /// doesn't know about is missed.
    pub fn list_dividend_payments(
        &self,
        account_id: i64,
        from: String,
        to: String,
    ) -> Result<Vec<DividendPayment>, CoreError> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT booking_date, counterparty, amount_cents, currency
             FROM transactions
             WHERE account_id = ?1 AND deleted_ms IS NULL
               AND booking_date BETWEEN ?2 AND ?3
               AND amount_cents > 0
               AND (
                     LOWER(purpose) LIKE '%dividend%' OR LOWER(purpose) LIKE '%ausschüttung%'
                     OR LOWER(booking_text) LIKE '%dividend%' OR LOWER(booking_text) LIKE '%ausschüttung%'
                     OR LOWER(counterparty) LIKE '%dividend%'
                   )
             ORDER BY booking_date ASC",
        )?;
        let rows = stmt
            .query_map(params![account_id, from, to], |r| {
                Ok(DividendPayment {
                    booking_date: r.get(0)?,
                    counterparty: r.get(1)?,
                    amount_cents: r.get(2)?,
                    currency: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
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

    fn depot(v: &Vault) -> i64 {
        let db = v.db().unwrap();
        db.execute(
            "INSERT INTO account_groups(name, sf_symbol, sort_order) VALUES ('G', 'tray', 1)
             ON CONFLICT DO NOTHING",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO accounts(group_id, external_ref, name, kind)
             VALUES ((SELECT min(id) FROM account_groups), 'D', 'Depot', 'brokerage')",
            [],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    fn insert_holding(
        v: &Vault,
        acc: i64,
        isin: &str,
        wkn: &str,
        qty: f64,
        price: i64,
        value: i64,
    ) {
        v.db()
            .unwrap()
            .execute(
                "INSERT INTO holdings(account_id, isin, wkn, name, quantity, current_price_cents, value_cents)
                 VALUES (?1, ?2, ?3, 'X', ?4, ?5, ?6)",
                params![acc, isin, wkn, qty, price, value],
            )
            .unwrap();
    }

    #[test]
    fn allocation_shares_sum_to_one() {
        let v = vault("allocation");
        let acc = depot(&v);
        insert_holding(&v, acc, "DE1", "1", 1.0, 100, 300);
        insert_holding(&v, acc, "DE2", "2", 1.0, 100, 700);
        let rows = v.depot_allocation(acc).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].isin, "DE2", "largest first");
        assert!((rows[0].share - 0.7).abs() < 1e-9);
        assert!((rows[1].share - 0.3).abs() < 1e-9);
    }

    #[test]
    fn portfolio_value_respects_purchase_date() {
        let v = vault("portfolio-value");
        let acc = depot(&v);
        insert_holding(&v, acc, "DE1", "1", 2.0, 150, 300);
        v.record_security_price(
            "DE1".into(),
            "2026-11-01".into(),
            100,
            "EUR".into(),
            "sync".into(),
        )
        .unwrap();
        v.record_security_price(
            "DE1".into(),
            "2026-11-10".into(),
            150,
            "EUR".into(),
            "sync".into(),
        )
        .unwrap();
        // bought only on the 5th: the 1st must not count
        v.set_holding_purchase(
            acc,
            "DE1".into(),
            "1".into(),
            "2026-11-05".into(),
            None,
            "".into(),
        )
        .unwrap();
        let points = v
            .portfolio_value_over_time(acc, "2026-10-01".into(), "2026-12-01".into())
            .unwrap();
        assert_eq!(
            points.len(),
            1,
            "the point before the purchase date is excluded"
        );
        assert_eq!(points[0].date, "2026-11-10");
        assert_eq!(points[0].value_cents, 300); // 2.0 * 150
    }

    #[test]
    fn position_performance_uses_manual_price_over_synced_one() {
        let v = vault("performance");
        let acc = depot(&v);
        insert_holding(&v, acc, "DE1", "1", 2.0, 200, 400);
        v.db()
            .unwrap()
            .execute(
                "UPDATE holdings SET purchase_price_cents = 150 WHERE account_id = ?1",
                params![acc],
            )
            .unwrap();
        // without a manual entry, falls back to the synced purchase price
        let perf = v
            .position_performance(acc, "DE1".into(), "1".into())
            .unwrap();
        assert_eq!(perf.purchase_price_cents, Some(150));
        assert_eq!(perf.gain_cents, Some(100)); // (200-150) * 2.0
        assert!((perf.gain_percent.unwrap() - (50.0 / 150.0)).abs() < 1e-9);

        // a manual entry overrides it
        v.set_holding_purchase(
            acc,
            "DE1".into(),
            "1".into(),
            "2026-01-01".into(),
            Some(100),
            "".into(),
        )
        .unwrap();
        let perf = v
            .position_performance(acc, "DE1".into(), "1".into())
            .unwrap();
        assert_eq!(perf.purchase_price_cents, Some(100));
        assert_eq!(perf.gain_cents, Some(200)); // (200-100) * 2.0
    }

    #[test]
    fn position_performance_without_any_purchase_price_has_no_gain() {
        let v = vault("performance-unknown");
        let acc = depot(&v);
        insert_holding(&v, acc, "DE1", "1", 1.0, 200, 200);
        let perf = v
            .position_performance(acc, "DE1".into(), "1".into())
            .unwrap();
        assert_eq!(perf.purchase_price_cents, None);
        assert_eq!(perf.gain_cents, None);
        assert_eq!(perf.gain_percent, None);
    }

    #[test]
    fn dividend_payments_match_by_wording_not_category() {
        let v = vault("dividends");
        let acc = depot(&v);
        let db = v.db().unwrap();
        for (ext, date, amount, cp, purpose) in [
            ("t1", "2026-03-15", 2500, "Bayer AG", "Dividendengutschrift"),
            (
                "t2",
                "2026-06-15",
                1800,
                "iShares ETF",
                "Ertragsausschüttung",
            ),
            ("t3", "2026-07-01", -500, "Broker", "Gebühr"),
            ("t4", "2026-08-01", 999, "Sparkasse", "Gehalt"),
        ] {
            db.execute(
                "INSERT INTO transactions(account_id, external_id, booking_date, amount_cents, counterparty, purpose)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![acc, ext, date, amount, cp, purpose],
            )
            .unwrap();
        }
        drop(db);
        let rows = v
            .list_dividend_payments(acc, "2026-01-01".into(), "2026-12-31".into())
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].counterparty, "Bayer AG");
        assert_eq!(rows[0].amount_cents, 2500);
        assert_eq!(rows[1].counterparty, "iShares ETF");
    }
}
