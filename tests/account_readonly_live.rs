//! 三所账户接口只读联网测试（#813：凭证拆进账户类型、签名请求收成一处之后，确认签名与回复解析
//! 在真实账户上照常工作）。只查询：持仓、净值、挂单、近一天资金费流水，不下单、不撤单。
//!
//! 需要白名单 IP 与环境变量：BINANCE_API_KEY / BINANCE_SECRET、OKX_API_KEY / OKX_SECRET /
//! OKX_PASSPHRASE。缺哪所的变量就跳过哪所。
//! 运行：`cargo test --test account_readonly_live -- --ignored --nocapture`

use hft_engine_rs::exchange::binance::{BinanceAccount, BinanceClient, BinanceCredentials};
use hft_engine_rs::exchange::okx::{OkxAccount, OkxClient, OkxCredentials};
use hft_engine_rs::exchange::{AccountClient, ExchangeClient, FundingFeeSource};
use std::sync::Arc;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

async fn read_all(account: &(impl AccountClient + FundingFeeSource)) {
    let ex = AccountClient::exchange(account);
    let equity = account.fetch_equity().await.expect("净值");
    let positions = account.fetch_positions().await.expect("持仓");
    let open = account.fetch_open_orders().await.expect("挂单");
    let now = hft_engine_rs::domain::now_ms();
    let fees = account
        .fetch_funding_fees(now - 24 * 3600 * 1000, now)
        .await
        .expect("资金费流水");
    println!(
        "{ex}: equity={:.2} {} positions={positions:?} open_orders={} funding_fees_24h={}",
        equity.amount,
        equity.asset,
        open.len(),
        fees.len()
    );
}

#[tokio::test]
#[ignore = "需要联网、白名单 IP 与 BN/OKX 凭证"]
async fn binance_and_okx_accounts_read_only() {
    if let (Some(api_key), Some(secret)) = (env("BINANCE_API_KEY"), env("BINANCE_SECRET")) {
        let public = BinanceClient::new("USDT".into()).unwrap();
        let metas = public.fetch_all_symbol_metas().await.expect("BN 合约规格");
        assert!(!metas.is_empty());
        read_all(&BinanceAccount::new(
            &public,
            BinanceCredentials { api_key, secret },
        ))
        .await;
    }
    if let (Some(api_key), Some(secret), Some(passphrase)) =
        (env("OKX_API_KEY"), env("OKX_SECRET"), env("OKX_PASSPHRASE"))
    {
        let public = Arc::new(OkxClient::new("USDT".into()).unwrap());
        let account = OkxAccount::new(
            public,
            OkxCredentials {
                api_key,
                secret,
                passphrase,
            },
        )
        .unwrap();
        read_all(&account).await;
    }
}
