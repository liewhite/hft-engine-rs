//! HL 账户只读联网测试：一个 client 接主站 + xyz，读统一账户的净值、持仓总价值与持仓。
//! 只查询、不下单（签名机地址是占位的，本测试不会用到）。
//!
//! 运行：`HL_WALLET=0x… cargo test --test hyperliquid_account_readonly -- --ignored --nocapture`

use hft_engine_rs::exchange::hyperliquid::{HyperliquidClient, HyperliquidCredentials};
use hft_engine_rs::exchange::AccountClient;

#[tokio::test]
#[ignore = "需要联网与 HL_WALLET（主钱包地址，只读）"]
async fn unified_account_equity_and_positions_across_dexes() {
    let wallet = std::env::var("HL_WALLET").expect("HL_WALLET");
    // 占位签名机：本测试只读，不会提交任何签名请求
    let creds = HyperliquidCredentials::with_signer(wallet, "http://127.0.0.1:1".to_string(), 0.05);
    let client = HyperliquidClient::connect(
        "USDC".to_string(),
        vec![String::new(), "xyz".to_string()],
        Some(creds),
    )
    .await
    .expect("统一账户应能同时接主站与 xyz");
    let info = client.fetch_account_info().await.expect("净值");
    let positions = client.fetch_positions().await.expect("持仓");
    println!(
        "equity={} notional={} positions={positions:?}",
        info.equity, info.notional
    );
    assert!(
        info.equity > 0.0,
        "统一账户净值应读 spot USDC，不是各 dex 的 0"
    );
}
