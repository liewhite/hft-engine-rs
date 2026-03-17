//! OKX Account Greeks 轮询测试
//!
//! 通过 ManagerActor + mock 策略，测试 OKX Greeks REST 轮询的完整框架链路。
//!
//! 运行:
//! OKX_API_KEY=xxx OKX_SECRET=xxx OKX_PASSPHRASE=xxx \
//!   cargo test --test okx_greeks_test -- --ignored --nocapture

use hft_engine_rs::domain::Exchange;
use hft_engine_rs::engine::{
    AddStrategies, ManagerActor, ManagerActorArgs,
};
use hft_engine_rs::exchange::okx::OkxCredentials;
use hft_engine_rs::exchange::SubscriptionKind;
use hft_engine_rs::messaging::{ExchangeEventData, IncomeEvent, StateManager};
use hft_engine_rs::strategy::{OutcomeEvent, Strategy};
use kameo::actor::Spawn;
use kameo::mailbox;
use std::collections::{HashMap, HashSet};

fn get_credentials() -> Option<OkxCredentials> {
    let api_key = std::env::var("OKX_API_KEY").ok()?;
    let secret = std::env::var("OKX_SECRET").ok()?;
    let passphrase = std::env::var("OKX_PASSPHRASE").ok()?;
    Some(OkxCredentials {
        api_key,
        secret,
        passphrase,
        quote: "USDT".to_string(),
    })
}

/// Mock 策略：只打印 Greeks 事件
struct GreeksPrintStrategy {
    symbol: String,
}

impl Strategy for GreeksPrintStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        // 订阅一个 BBO 以触发 OkxActor 创建（Greeks 由 REST 轮询获取）
        let mut kinds = HashSet::new();
        kinds.insert(SubscriptionKind::BBO {
            symbol: self.symbol.clone(),
        });
        let mut map = HashMap::new();
        map.insert(Exchange::OKX, kinds);
        map
    }

    fn order_timeout_ms(&self) -> u64 {
        30_000
    }

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent> {
        match &event.data {
            ExchangeEventData::Greeks(raw) => {
                // state.greeks() 返回修正后的 delta (含现货 cashBal)
                if let Some(g) = state.greeks(Exchange::OKX, &raw.ccy) {
                    println!(
                        "[GREEKS] ccy={} delta={:.6} (raw={:.6}) gamma={:.6} theta={:.6} vega={:.6} ts={}",
                        g.ccy, g.delta, raw.delta, g.gamma, g.theta, g.vega, g.timestamp
                    );
                } else {
                    println!(
                        "[GREEKS] ccy={} waiting for cashBal... (raw delta={:.6})",
                        raw.ccy, raw.delta
                    );
                }
            }
            _ => {}
        }
        vec![]
    }
}

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_okx_greeks_push() {
    tracing_subscriber::fmt()
        .with_env_filter("hft_engine_rs=info")
        .init();

    let credentials = get_credentials().expect("需要设置 OKX_API_KEY, OKX_SECRET, OKX_PASSPHRASE");

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance_credentials: None,
            okx_credentials: Some(credentials),
            hyperliquid_credentials: None,
            ibkr_credentials: None,
        },
        mailbox::unbounded(),
    );

    let strategy = GreeksPrintStrategy {
        symbol: "BTC".to_string(),
    };

    let result = manager
        .ask(AddStrategies(vec![Box::new(strategy)]))
        .send()
        .await;
    result.expect("添加策略失败");

    println!("策略已启动，等待 Greeks 轮询 (每333ms, Ctrl+C 退出)...\n");

    // 持续运行，等待推送
    tokio::signal::ctrl_c().await.unwrap();
}
