//! Backfill of external historical closing prices for a security, keyed by
//! ISIN (shared across every account that holds it – see
//! `schema::HOLDING_EXTRAS`).
//!
//! Uses Alpha Vantage's documented `TIME_SERIES_DAILY` endpoint
//! (<https://www.alphavantage.co/documentation/>), chosen over an earlier
//! Börse Frankfurt attempt specifically because it's a stable, published
//! API with a free, user-obtained API key – no anti-scraping request
//! signing to reverse-engineer and keep working. The trade-off: Alpha
//! Vantage has no ISIN lookup, only a ticker `symbol`, so the user maps
//! ISIN → symbol once (see `security_ticker`/`set_security_ticker` in
//! `vault/holdings.rs`) and that mapping is reused on every backfill. The
//! free tier is capped at 25 requests/day, 5/minute – plenty for an
//! occasional manual backfill, not for automatic per-sync refreshing (that
//! stays the sync snapshot's job, see `connectors::persist::store_fetch`).

use serde_json::Value;

use crate::error::{CoreError, CoreResult};
use crate::vault::Vault;

use super::{http_client, read_text_limited};

const ENDPOINT: &str = "https://www.alphavantage.co/query";

/// One day's close, as parsed from the external response.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExternalPricePoint {
    /// ISO date (`YYYY-MM-DD`).
    pub date: String,
    pub close_price_cents: i64,
    pub currency: String,
}

/// Fetches Alpha Vantage's full daily-close history for `symbol` (its own
/// ticker, not an ISIN/WKN). Alpha Vantage always returns its documented
/// currency-agnostic OHLCV series for the exchange the symbol trades on; we
/// default the currency to EUR (every current use is a Xetra-style German
/// listing) since the free `TIME_SERIES_DAILY` response carries no currency
/// field at all.
pub(crate) async fn fetch_alpha_vantage(
    symbol: &str,
    api_key: &str,
) -> CoreResult<Vec<ExternalPricePoint>> {
    let symbol = symbol.trim();
    if symbol.is_empty() {
        return Err(CoreError::InvalidInput {
            message: "Kein Ticker-Symbol für diese ISIN hinterlegt.".into(),
        });
    }
    if api_key.trim().is_empty() {
        return Err(CoreError::MissingSetting {
            key: "alphavantage.api_key".into(),
        });
    }
    let client = http_client()?;
    let resp = client
        .get(ENDPOINT)
        .query(&[
            ("function", "TIME_SERIES_DAILY"),
            ("symbol", symbol),
            ("outputsize", "full"),
            ("apikey", api_key),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(CoreError::Network {
            message: format!("Alpha Vantage antwortete mit Status {}", resp.status()),
        });
    }
    let body = read_text_limited(resp).await?;
    parse_time_series_daily(&body)
}

fn parse_time_series_daily(body: &str) -> CoreResult<Vec<ExternalPricePoint>> {
    let v: Value = serde_json::from_str(body).map_err(|_| unrecognized(body))?;

    // Alpha Vantage reports every problem (bad symbol, rate limit, premium
    // endpoint on a free key, …) as HTTP 200 with one of these fields
    // instead of the data – surface them as distinct, actionable errors
    // rather than falling through to "shape not recognised".
    if let Some(msg) = v.get("Error Message").and_then(Value::as_str) {
        return Err(CoreError::InvalidInput {
            message: format!("Alpha Vantage kennt dieses Symbol nicht: {msg}"),
        });
    }
    if let Some(msg) = v
        .get("Note")
        .or_else(|| v.get("Information"))
        .and_then(Value::as_str)
    {
        return Err(CoreError::Network {
            message: format!(
                "Alpha Vantage hat die Anfrage abgelehnt (Tageslimit des kostenlosen Tarifs \
                 ist 25 Anfragen – später erneut versuchen): {msg}"
            ),
        });
    }

    let series = v
        .get("Time Series (Daily)")
        .and_then(Value::as_object)
        .ok_or_else(|| unrecognized(body))?;
    let mut out = Vec::with_capacity(series.len());
    for (date, day) in series {
        let Some(close) = day.get("4. close").and_then(Value::as_str) else {
            continue;
        };
        let Ok(price) = close.parse::<f64>() else {
            continue;
        };
        out.push(ExternalPricePoint {
            date: date.clone(),
            close_price_cents: (price * 100.0).round() as i64,
            currency: "EUR".to_string(),
        });
    }
    Ok(out)
}

fn unrecognized(body: &str) -> CoreError {
    let snippet: String = body.chars().take(300).collect();
    CoreError::Protocol {
        message: format!(
            "Antwortformat der Alpha-Vantage-API nicht erkannt. Anfang der Antwort: {snippet}"
        ),
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Vault {
    /// Backfills `security_prices` for one ISIN from Alpha Vantage, for the
    /// inclusive date range `from`..`to` (ISO `YYYY-MM-DD`). Requires a
    /// ticker symbol already stored for the ISIN (`set_security_ticker`)
    /// and an `alphavantage.api_key` setting; returns `MissingSetting` /
    /// `InvalidInput` with a clear message if either is absent. Never
    /// overwrites a day already recorded, whether by a sync or an earlier
    /// backfill – only fills gaps. Returns how many new days were actually
    /// stored.
    pub async fn backfill_security_price_history(
        &self,
        isin: String,
        from: String,
        to: String,
    ) -> Result<u32, CoreError> {
        let isin = isin.trim().to_string();
        let symbol = self.security_ticker(isin.clone())?.unwrap_or_default();
        if symbol.is_empty() {
            return Err(CoreError::InvalidInput {
                message: format!(
                    "Kein Ticker-Symbol für ISIN {isin} hinterlegt. Bitte bei der Position \
                     eintragen (z. B. \"SAP.DEX\" für Xetra)."
                ),
            });
        }
        let api_key = {
            let db = self.db()?;
            Self::setting_inner(&db, "alphavantage.api_key")?.unwrap_or_default()
        };
        let points = fetch_alpha_vantage(&symbol, &api_key).await?;
        let db = self.db()?;
        let mut stored = 0u32;
        for p in points.iter().filter(|p| p.date >= from && p.date <= to) {
            let changed = db.execute(
                "INSERT OR IGNORE INTO security_prices
                    (isin, price_date, close_price_cents, currency, source)
                 VALUES (?1, ?2, ?3, ?4, 'alphavantage')",
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
    fn parses_the_documented_daily_series() {
        let body = r#"{
            "Meta Data": {"2. Symbol": "SAP.DEX"},
            "Time Series (Daily)": {
                "2026-11-17": {"1. open": "10.0", "2. high": "11.0", "3. low": "9.5", "4. close": "11.90", "5. volume": "100"},
                "2026-11-16": {"1. open": "9.0", "2. high": "10.5", "3. low": "8.9", "4. close": "11.50", "5. volume": "90"}
            }
        }"#;
        let mut points = parse_time_series_daily(body).unwrap();
        points.sort_by(|a, b| a.date.cmp(&b.date));
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].date, "2026-11-16");
        assert_eq!(points[0].close_price_cents, 1150);
        assert_eq!(points[1].date, "2026-11-17");
        assert_eq!(points[1].close_price_cents, 1190);
    }

    #[test]
    fn unknown_symbol_is_an_invalid_input_error() {
        let body =
            r#"{"Error Message": "Invalid API call. Please retry or visit the documentation."}"#;
        match parse_time_series_daily(body) {
            Err(CoreError::InvalidInput { message }) => {
                assert!(message.contains("Symbol"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_note_is_a_network_error() {
        let body = r#"{"Note": "Thank you for using Alpha Vantage! Our standard API call frequency is 25 requests per day."}"#;
        assert!(matches!(
            parse_time_series_daily(body),
            Err(CoreError::Network { .. })
        ));
    }

    #[test]
    fn premium_endpoint_information_is_a_network_error() {
        let body =
            r#"{"Information": "Thank you for using Alpha Vantage! This is a premium endpoint."}"#;
        assert!(matches!(
            parse_time_series_daily(body),
            Err(CoreError::Network { .. })
        ));
    }

    #[test]
    fn entries_missing_close_are_skipped_not_fatal() {
        let body = r#"{"Time Series (Daily)": {
            "2026-11-16": {"4. close": "10.00"},
            "2026-11-17": {"1. open": "11.0"}
        }}"#;
        let points = parse_time_series_daily(body).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].date, "2026-11-16");
    }

    #[test]
    fn unrecognized_shape_is_a_protocol_error_with_a_snippet() {
        let body = r#"{"unexpected": "shape"}"#;
        match parse_time_series_daily(body) {
            Err(CoreError::Protocol { message }) => {
                assert!(message.contains("unexpected"), "{message}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn non_json_body_is_a_protocol_error() {
        assert!(matches!(
            parse_time_series_daily("<html>not json</html>"),
            Err(CoreError::Protocol { .. })
        ));
    }

    #[tokio::test]
    async fn fetch_rejects_an_empty_symbol_before_any_request() {
        assert!(matches!(
            fetch_alpha_vantage("", "key").await,
            Err(CoreError::InvalidInput { .. })
        ));
    }

    #[tokio::test]
    async fn fetch_rejects_a_missing_api_key_before_any_request() {
        assert!(matches!(
            fetch_alpha_vantage("SAP.DEX", "").await,
            Err(CoreError::MissingSetting { .. })
        ));
    }
}
