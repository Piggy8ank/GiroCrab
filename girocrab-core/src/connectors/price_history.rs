//! Backfill of external historical closing prices for a security, keyed by
//! ISIN (shared across every account that holds it – see
//! `schema::HOLDING_EXTRAS`).
//!
//! Uses Börse Frankfurt's public price-history endpoint. There is no
//! official, documented API for this; it's the same "undocumented JSON
//! endpoint, no login" pattern several bank connectors in this crate
//! already rely on (see e.g. `trade_republic.rs`). No credentials are sent
//! and nothing here ever syncs an account – it only fills gaps in
//! `security_prices` for a single ISIN on request.
//!
//! The exact response shape isn't formally documented. Parsing below
//! accepts the field-name variants the Börse Frankfurt site and community
//! tools built against it (e.g. `bf4py`) are known to use, and fails
//! loudly with a snippet of the raw body if none of them match – so if the
//! site changes its shape, that shows up as a clear error instead of
//! silently stored garbage. **This has not been exercised against a live
//! response** (this crate's CI/dev sandbox cannot reach the internet);
//! verify with `cargo run -p girocrab-core --example backfill_price_history
//! -- <ISIN>` on a machine with normal network access before relying on it.

use serde_json::Value;

use crate::error::{CoreError, CoreResult};
use crate::vault::Vault;

use super::{http_client, read_text_limited};

const ENDPOINT: &str = "https://api.boerse-frankfurt.de/v1/data/price_history";

/// One day's close, as parsed from the external response.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExternalPricePoint {
    /// ISO date (`YYYY-MM-DD`).
    pub date: String,
    pub close_price_cents: i64,
    pub currency: String,
}

/// Fetches daily closes for `isin` on Xetra between `from`/`to` (ISO
/// dates, inclusive).
pub(crate) async fn fetch_boerse_frankfurt(
    isin: &str,
    from: &str,
    to: &str,
) -> CoreResult<Vec<ExternalPricePoint>> {
    let isin = isin.trim();
    if isin.is_empty() {
        return Err(CoreError::InvalidInput {
            message: "ISIN fehlt.".into(),
        });
    }
    let client = http_client()?;
    let resp = client
        .get(ENDPOINT)
        .query(&[
            ("isin", isin),
            ("mic", "XETR"),
            ("minDate", from),
            ("maxDate", to),
            ("limit", "2000"),
            ("offset", "0"),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(CoreError::Network {
            message: format!("Börse Frankfurt antwortete mit Status {}", resp.status()),
        });
    }
    let body = read_text_limited(resp).await?;
    parse_price_history(&body)
}

fn parse_price_history(body: &str) -> CoreResult<Vec<ExternalPricePoint>> {
    let v: Value = serde_json::from_str(body).map_err(|_| unrecognized(body))?;
    let entries = v
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
        .ok_or_else(|| unrecognized(body))?;
    let mut out = Vec::new();
    for e in entries {
        let Some(date) = first_str(e, &["date", "day", "tradeDate"]) else {
            continue;
        };
        let Some(price) = first_f64(e, &["close", "closingPrice", "closePrice", "price", "last"])
        else {
            continue;
        };
        let currency =
            first_str(e, &["currency", "currencyCode"]).unwrap_or_else(|| "EUR".to_string());
        out.push(ExternalPricePoint {
            // the site's dates are commonly full timestamps, e.g.
            // "2026-11-17T00:00:00"; we only ever store the day.
            date: date.chars().take(10).collect(),
            close_price_cents: (price * 100.0).round() as i64,
            currency,
        });
    }
    if out.is_empty() && !entries.is_empty() {
        return Err(unrecognized(body));
    }
    Ok(out)
}

fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|&k| v.get(k))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn first_f64(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|&k| v.get(k)).and_then(Value::as_f64)
}

fn unrecognized(body: &str) -> CoreError {
    let snippet: String = body.chars().take(300).collect();
    CoreError::Protocol {
        message: format!(
            "Antwortformat der Kursdaten-API nicht erkannt. Anfang der Antwort: {snippet}"
        ),
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Vault {
    /// Backfills `security_prices` for one ISIN from an external provider
    /// (currently: Börse Frankfurt / Xetra), for the inclusive date range
    /// `from`..`to` (ISO `YYYY-MM-DD`). Never overwrites a day already
    /// recorded, whether by a sync or an earlier backfill – only fills
    /// gaps. Returns how many new days were actually stored.
    pub async fn backfill_security_price_history(
        &self,
        isin: String,
        from: String,
        to: String,
    ) -> Result<u32, CoreError> {
        let isin = isin.trim().to_string();
        let points = fetch_boerse_frankfurt(&isin, &from, &to).await?;
        let db = self.db()?;
        let mut stored = 0u32;
        for p in &points {
            let changed = db.execute(
                "INSERT OR IGNORE INTO security_prices
                    (isin, price_date, close_price_cents, currency, source)
                 VALUES (?1, ?2, ?3, ?4, 'boerse-frankfurt')",
                rusqlite::params![isin, p.date, p.close_price_cents, p.currency],
            )?;
            stored += changed as u32;
        }
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_data_envelope_with_documented_field_names() {
        let body = r#"{
            "data": [
                {"date": "2026-11-16T00:00:00", "open": 10.0, "close": 11.5, "currency": "EUR"},
                {"date": "2026-11-17T00:00:00", "open": 11.5, "close": 11.9}
            ],
            "recordsTotal": 2,
            "recordsFiltered": 2
        }"#;
        let points = parse_price_history(body).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].date, "2026-11-16");
        assert_eq!(points[0].close_price_cents, 1150);
        assert_eq!(points[0].currency, "EUR");
        // missing currency on the second entry falls back to EUR
        assert_eq!(points[1].currency, "EUR");
        assert_eq!(points[1].close_price_cents, 1190);
    }

    #[test]
    fn parses_a_bare_array_and_alternate_price_field_names() {
        let body = r#"[
            {"date": "2026-11-16", "closingPrice": 42.0},
            {"date": "2026-11-17", "price": 43.5}
        ]"#;
        let points = parse_price_history(body).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].close_price_cents, 4200);
        assert_eq!(points[1].close_price_cents, 4350);
    }

    #[test]
    fn entries_missing_date_or_price_are_skipped_not_fatal() {
        let body = r#"{"data": [
            {"date": "2026-11-16", "close": 10.0},
            {"close": 11.0},
            {"date": "2026-11-18"}
        ]}"#;
        let points = parse_price_history(body).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].date, "2026-11-16");
    }

    #[test]
    fn empty_data_is_not_an_error() {
        let body = r#"{"data": [], "recordsTotal": 0}"#;
        assert_eq!(parse_price_history(body).unwrap(), vec![]);
    }

    #[test]
    fn unrecognized_shape_is_a_protocol_error_with_a_snippet() {
        let body = r#"{"unexpected": "shape"}"#;
        match parse_price_history(body) {
            Err(CoreError::Protocol { message }) => {
                assert!(message.contains("unexpected"), "{message}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn non_json_body_is_a_protocol_error() {
        assert!(matches!(
            parse_price_history("<html>not json</html>"),
            Err(CoreError::Protocol { .. })
        ));
    }
}
