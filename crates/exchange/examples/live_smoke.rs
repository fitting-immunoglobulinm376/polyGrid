//! One-shot Polymarket Perps live smoke.
//! `PRIVATE_KEY=... cargo run -p exchange --example live_smoke`
//! Optional: `PROXY_DELETE=0x...` to revoke a known orphaned proxy first.

use exchange::{Exchange, PolymarketExchange};
use grid_engine::{OrderIntent, RunMode, Side, TimeInForce};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .try_init();

    let key = std::env::var("PRIVATE_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        eprintln!("set PRIVATE_KEY");
        std::process::exit(1);
    }

    let mut pm = PolymarketExchange::new(RunMode::Mainnet);
    if let Err(e) = pm.set_private_key(&key) {
        eprintln!("set_private_key failed: {e}");
        std::process::exit(1);
    }
    let addr = pm.address().unwrap_or("?").to_string();
    println!("EOA address: {addr}");

    if let Ok(del) = std::env::var("PROXY_DELETE") {
        let del = del.trim();
        if del.starts_with("0x") {
            println!("deleting proxy {del} ...");
            match pm.delete_proxy_eoa(del).await {
                Ok(()) => println!("deleteProxy: OK"),
                Err(e) => eprintln!("deleteProxy failed: {e}"),
            }
        }
    }

    if let Err(e) = pm.ensure_connected().await {
        eprintln!("ensure_connected / createProxy failed: {e}");
        eprintln!(
            "Hint: if proxy_limit_reached, an earlier random proxy is still registered.\n\
             Deposit pUSD at https://polymarket.com then retry after proxies expire,\n\
             or pass PROXY_DELETE=0x<proxy> if you know the orphaned proxy address."
        );
        std::process::exit(1);
    }
    println!("proxy session: OK");

    match pm.get_balances().await {
        Ok(bals) => {
            println!("balances ({} rows):", bals.len());
            for b in &bals {
                println!(
                    "  {} kind={} total={} avail={}",
                    b.asset, b.kind, b.total, b.available
                );
            }
            let equity: Decimal = bals
                .iter()
                .filter(|b| {
                    b.asset.eq_ignore_ascii_case("pUSD") || b.asset.eq_ignore_ascii_case("USDC")
                })
                .map(|b| b.available)
                .fold(Decimal::ZERO, |a, b| a.max(b));
            if equity <= Decimal::ZERO {
                eprintln!(
                    "WARNING: available pUSD is 0. Fund the EOA on https://polymarket.com \
before placing live orders (account may reject orders until funded)."
                );
            }
        }
        Err(e) => eprintln!("get_balances failed: {e}"),
    }

    let mid = match pm.get_mid("BTC").await {
        Ok(m) => {
            println!("BTC mid = {m}");
            m
        }
        Err(e) => {
            eprintln!("get_mid failed: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = pm.set_leverage("BTC", 2, true).await {
        eprintln!("set_leverage failed (continuing): {e}");
    } else {
        println!("set_leverage 2x cross: OK");
    }

    let px = (mid * dec!(0.5)).round_dp(1);
    let qty = (dec!(2) / px).round_dp(5);
    if qty <= Decimal::ZERO {
        eprintln!("computed qty is zero; abort place");
        std::process::exit(1);
    }
    let intent = OrderIntent {
        client_id: uuid::Uuid::new_v4().to_string(),
        symbol: "BTC".into(),
        side: Side::Buy,
        price: px,
        size: qty,
        level_index: 0,
        reduce_only: false,
        tif: TimeInForce::Gtc,
        cloid: Some("pgsmoke000000000000000000000001".into()),
    };
    println!(
        "placing GTC buy {} BTC @ {} (notional≈{})",
        intent.size,
        intent.price,
        intent.size * intent.price
    );

    match pm.place_order(intent).await {
        Ok(order) => {
            println!(
                "placed: exchange_id={:?} cloid={:?} size={}",
                order.exchange_id, order.cloid, order.size
            );
            if let Err(e) = pm.cancel_order(&order.client_id).await {
                eprintln!("cancel_order failed: {e}");
                let _ = pm.cancel_all("BTC").await;
            } else {
                println!("cancel_order: OK");
            }
        }
        Err(e) => {
            eprintln!("place_order failed: {e}");
            std::process::exit(1);
        }
    }

    println!("live smoke finished");
}
