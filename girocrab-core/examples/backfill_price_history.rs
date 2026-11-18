//! Manual, live check of the Alpha Vantage price-history backfill
//! (`Vault::backfill_security_price_history`), against the real API –
//! something the dev sandbox this was written in cannot do (no outbound
//! network access there).
//!
//! Usage:
//!   cargo run -q -p girocrab-core --example backfill_price_history -- <ISIN> <SYMBOL> <API_KEY> [days]
//!
//! Example (Alpha Vantage key from https://www.alphavantage.co/support/#api-key):
//!   cargo run -q -p girocrab-core --example backfill_price_history -- DE0007164600 SAP.DEX ABCDEFGH 30
//!
//! Opens a throwaway vault in a temp dir, stores the ISIN→symbol mapping and
//! the API key as the real app would (`set_security_ticker` / `set_setting`),
//! runs the backfill for the last `days` days (default 30) and prints what
//! got stored. A `Protokollfehler` / `Protocol` error printing a response
//! snippet means Alpha Vantage's JSON shape no longer matches what
//! `connectors/price_history.rs` expects – paste that snippet back so the
//! parser can be fixed. A `Netzwerkfehler` mentioning the daily limit means
//! the free tier's 25 requests/day have been used up for today.

use girocrab_core::vault::Vault;

const KEY: [u8; 32] = [0x99; 32];

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let usage = || -> String {
        eprintln!("Usage: backfill_price_history <ISIN> <SYMBOL> <API_KEY> [days]");
        std::process::exit(1)
    };
    let isin = args.next().unwrap_or_else(&usage);
    let symbol = args.next().unwrap_or_else(&usage);
    let api_key = args.next().unwrap_or_else(&usage);
    let days: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);

    let to = chrono::Local::now().date_naive();
    let from = to - chrono::Duration::days(days);
    let (from, to) = (
        from.format("%Y-%m-%d").to_string(),
        to.format("%Y-%m-%d").to_string(),
    );

    let dir = std::env::temp_dir().join(format!("girocrab-backfill-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v.db").to_string_lossy().to_string();
    let vault = Vault::open(path, KEY.to_vec()).unwrap();

    vault
        .set_security_ticker(isin.clone(), symbol.clone())
        .unwrap();
    vault
        .set_setting("alphavantage.api_key".into(), api_key)
        .unwrap();

    println!("Fetching {isin} ({symbol}) from {from} to {to} …");
    match vault
        .backfill_security_price_history(isin.clone(), from, to)
        .await
    {
        Ok(stored) => {
            println!("Stored {stored} new day(s).");
            for p in vault.security_price_history(isin).unwrap() {
                println!(
                    "  {} {:>10.2} {} (source: {})",
                    p.price_date,
                    p.close_price_cents as f64 / 100.0,
                    p.currency,
                    p.source
                );
            }
        }
        Err(e) => {
            eprintln!("Fehler: {e:?}");
            std::process::exit(1);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
