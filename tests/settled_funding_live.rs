//! 已结算公共资金费率的联网测试（无需凭证）。
//!
//! 运行：`cargo test --test settled_funding_live -- --ignored --nocapture`

use hft_engine_rs::domain::now_ms;
use hft_engine_rs::exchange::binance::BinanceClient;
use hft_engine_rs::exchange::hyperliquid::HyperliquidClient;

const DAY_MS: u64 = 24 * 3600 * 1000;

#[tokio::test]
#[ignore]
async fn binance_last_day_has_settlements() {
    let client = BinanceClient::new("USDT".to_string(), None).unwrap();
    let got = client.fetch_settled_funding(&"BTC".to_string(), now_ms() - DAY_MS).await.unwrap();
    println!("{got:?}");
    // BTCUSDT 8h 周期，24h 内 3 次
    assert!((2..=4).contains(&got.len()), "{}", got.len());
    assert!(got.windows(2).all(|w| w[0].settle_time < w[1].settle_time));
}

#[tokio::test]
#[ignore]
async fn hyperliquid_main_and_xyz_dex_last_day() {
    for (dex, sym) in [("", "BTC"), ("xyz", "TSLA")] {
        let client = HyperliquidClient::new("USDC".to_string(), dex.to_string(), None).unwrap();
        let got = client.fetch_settled_funding(&sym.to_string(), now_ms() - DAY_MS).await.unwrap();
        println!("{dex}:{sym} n={} first={:?}", got.len(), got.first());
        // 1h 周期，24h 内约 24 次
        assert!((22..=25).contains(&got.len()), "{dex}:{sym} {}", got.len());
    }
}
