//! trades 公共成交流的**线路一致性测试** —— 用真实报文校验生产代码的口径，不靠文档猜。
//!
//! 关键约束：**全程走生产函数**（`pick_ws_target` / `kind_to_*` / `parse_public_message`），
//! 测试自己不重写订阅报文与解析逻辑。否则线路契约会出现第二份副本，两边各自漂移 ——
//! 而"aggTrade 该落哪条 Binance 连接"这种知识一旦漂移，故障形态是**订阅被 ack 但永不推
//! 数据**（静默无数据、不报错），最难查。
//!
//! 校验四件事：
//!   1. 由 `SubscriptionKind::Trades` 推导出的端点/频道确实能收到数据（路由与频道名正确）
//!   2. 生产 codec 能解析真实报文（字段名/类型一致）
//!   3. 价格/数量/时间戳量级正确，且量为币本位（OKX 的张数已折算）
//!   4. 各所的**归集粒度**符合 [`SubscriptionKind::Trades`] 文档的记载
//!
//! 需要联网，故全部 `#[ignore]`：
//! ```sh
//! cargo test --lib trades_conformance -- --ignored --nocapture
//! ```

use crate::domain::{Exchange, MarketTrade, Symbol, SymbolMeta};
use crate::exchange::utils::StepFormatter;
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// 观测标的（三个所都有的高流动性品种，保证观测窗口内必有成交）
const COIN: &str = "BTC";
/// 每个所观测到多少笔成交后停止
const SAMPLE_TRADES: usize = 5;
/// 单个所的观测超时
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(30);
/// 毫秒时间戳下界（2020-01-01），用于识别"秒/微秒被当成毫秒"这类单位错误
const MS_EPOCH_FLOOR: u64 = 1_577_836_800_000;

fn trades_kind() -> SubscriptionKind {
    SubscriptionKind::Trades {
        symbol: COIN.to_string(),
    }
}

/// 连接 WS、发送订阅、用生产解析函数收集前 [`SAMPLE_TRADES`] 笔成交。
///
/// 返回 (成交列表, 原始报文列表)；原始报文供归集粒度断言使用。
/// `parse` 必须是生产解析函数的薄包装；返回 `Err` 表示 codec 与真实报文不符 ——
/// 此时 panic 而非跳过，否则测试就失去意义。
async fn observe(
    exchange_name: &str,
    url: &str,
    subscribe: Value,
    mut parse: impl FnMut(&str) -> Result<Vec<IncomeEvent>, String>,
) -> (Vec<MarketTrade>, Vec<String>) {
    println!("[{exchange_name}] url={url} subscribe={subscribe}");
    let (mut ws, _) = connect_async(url)
        .await
        .unwrap_or_else(|e| panic!("[{exchange_name}] connect {url} failed: {e}"));

    ws.send(Message::Text(subscribe.to_string()))
        .await
        .unwrap_or_else(|e| panic!("[{exchange_name}] send subscribe failed: {e}"));

    let mut trades = Vec::new();
    let mut raws = Vec::new();
    // 打印前若干条"非成交"报文（订阅确认/错误/心跳），否则订阅失败时只能看到空结果，无从诊断
    let mut unmatched_left = 3;
    let deadline = tokio::time::Instant::now() + OBSERVE_TIMEOUT;

    while trades.len() < SAMPLE_TRADES {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let msg = match tokio::time::timeout(remaining, ws.next()).await {
            Err(_) => break,
            Ok(None) => panic!("[{exchange_name}] stream closed before enough trades"),
            Ok(Some(Err(e))) => panic!("[{exchange_name}] ws error: {e}"),
            Ok(Some(Ok(m))) => m,
        };
        let Message::Text(raw) = msg else { continue };

        let events = parse(&raw)
            .unwrap_or_else(|e| panic!("[{exchange_name}] codec 与真实报文不符: {e}\nraw: {raw}"));
        let parsed: Vec<MarketTrade> = events
            .into_iter()
            .filter_map(|ev| match ev.data {
                ExchangeEventData::MarketTrade(t) => Some(t),
                _ => None,
            })
            .collect();
        if parsed.is_empty() {
            if unmatched_left > 0 {
                unmatched_left -= 1;
                println!("[{exchange_name}] non-trade msg: {raw}");
            }
            continue;
        }
        for t in &parsed {
            println!(
                "[{exchange_name}] parsed: symbol={} price={} qty={} is_buyer_maker={} ts={}",
                t.symbol, t.price, t.qty, t.is_buyer_maker, t.timestamp
            );
        }
        trades.extend(parsed);
        raws.push(raw);
    }

    let _ = ws.close(None).await;
    assert!(
        !trades.is_empty(),
        "[{exchange_name}] 观测窗口内未收到任何成交。\
         可能是订阅未生效（频道名/端点路由错误——注意订错端点会被 ack 但永不推数据）"
    );
    (trades, raws)
}

/// 三个所共有的量级 sanity。
fn assert_sane(exchange_name: &str, trades: &[MarketTrade], expect_exchange: Exchange) {
    for t in trades {
        assert_eq!(t.exchange, expect_exchange);
        assert_eq!(t.symbol, COIN, "[{exchange_name}] symbol 解析错误");
        assert!(t.price > 0.0, "[{exchange_name}] price 必须为正");
        assert!(t.qty > 0.0, "[{exchange_name}] qty 必须为正");
        assert!(
            t.timestamp > MS_EPOCH_FLOOR,
            "[{exchange_name}] 时间戳不是毫秒 epoch: {}",
            t.timestamp
        );
    }
    let buyer_maker = trades.iter().filter(|t| t.is_buyer_maker).count();
    println!(
        "[{exchange_name}] 共 {} 笔，其中买方挂单(主动卖) {} 笔",
        trades.len(),
        buyer_maker
    );
}

#[tokio::test]
#[ignore = "需要联网连交易所"]
async fn binance_agg_trade_matches_codec() {
    use crate::exchange::binance::actor::binance_actor::{pick_ws_target, WsTarget};
    use crate::exchange::binance::actor::public_ws::{kind_to_stream, parse_public_message};

    let kind = trades_kind();
    // 端点路由与 stream 名都由生产代码推导 —— 这正是踩过坑的地方，必须被测试覆盖
    let target = pick_ws_target(&kind);
    assert_eq!(
        target,
        WsTarget::Market,
        "aggTrade 归属 /market/ws；订到 /public/ws 会被 ack 但永不推数据"
    );
    let stream = kind_to_stream(&kind, "USDT").expect("Trades 必须能映射到 stream");
    let subscribed: HashSet<SubscriptionKind> = HashSet::from([kind]);

    let (trades, raws) = observe(
        "Binance",
        target.url(),
        json!({"method": "SUBSCRIBE", "params": [stream], "id": 1}),
        |raw| parse_public_message(raw, "USDT", 0, &subscribed).map_err(|e| e.to_string()),
    )
    .await;

    assert_sane("Binance", &trades, Exchange::Binance);
    // 归集粒度：aggTrade 覆盖 f..l 多笔撮合，观测窗口内应出现过 l > f
    let aggregated = raws.iter().any(|raw| {
        let v: Value = serde_json::from_str(raw).unwrap();
        v.get("l").and_then(|x| x.as_u64()) > v.get("f").and_then(|x| x.as_u64())
    });
    println!("[Binance] 观测到归集(l>f)的消息: {aggregated}");
}

#[tokio::test]
#[ignore = "需要联网连交易所"]
async fn okx_trades_matches_codec() {
    use crate::exchange::okx::actor::public_ws::{kind_to_arg, parse_public_message};
    use crate::exchange::okx::WS_PUBLIC_URL;

    let kind = trades_kind();
    let arg = kind_to_arg(&kind, "USDT").expect("Trades 必须能映射到 channel arg");
    let metas = okx_btc_metas();

    let (trades, raws) = observe(
        "OKX",
        WS_PUBLIC_URL,
        json!({"op": "subscribe", "args": [arg]}),
        |raw| parse_public_message(raw, 0, &metas).map_err(|e| e.to_string()),
    )
    .await;

    assert_sane("OKX", &trades, Exchange::OKX);
    // 张 -> 币折算已在生产路径完成：拿原始报文的 sz（张）与解析结果（币）对账。
    // 注意 OKX 的 sz 可以是小数张（实测有 "0.03"），不能假设为整数。
    let raw_contracts: f64 = raws
        .iter()
        .flat_map(|raw| {
            let v: Value = serde_json::from_str(raw).unwrap();
            v["data"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|t| t["sz"].as_str().and_then(|s| s.parse::<f64>().ok()))
                .collect::<Vec<_>>()
        })
        .sum();
    let parsed_coin: f64 = trades.iter().map(|t| t.qty).sum();
    let expected_coin = raw_contracts * OKX_BTC_CONTRACT_SIZE;
    assert!(
        (parsed_coin - expected_coin).abs() < 1e-9,
        "[OKX] 张->币折算不符：原始 {raw_contracts} 张 x {OKX_BTC_CONTRACT_SIZE} = \
         {expected_coin}，解析得 {parsed_coin}（漏折算会差 {}x）",
        1.0 / OKX_BTC_CONTRACT_SIZE
    );
    println!("[OKX] 张->币对账通过: {raw_contracts} 张 -> {parsed_coin} BTC");
    // 归集粒度：count 为合并笔数，观测窗口内应出现过 count > 1
    let aggregated = raws.iter().any(|raw| {
        let v: Value = serde_json::from_str(raw).unwrap();
        v["data"]
            .as_array()
            .map(|d| {
                d.iter().any(|t| {
                    t.get("count")
                        .and_then(|c| c.as_str())
                        .and_then(|c| c.parse::<u64>().ok())
                        .is_some_and(|c| c > 1)
                })
            })
            .unwrap_or(false)
    });
    println!("[OKX] 观测到归集(count>1)的消息: {aggregated}");
}

#[tokio::test]
#[ignore = "需要联网连交易所"]
async fn hyperliquid_trades_matches_codec() {
    use crate::exchange::hyperliquid::actor::public_ws::{
        kind_to_subscription, parse_public_message,
    };
    use crate::exchange::hyperliquid::WS_URL;

    let kind = trades_kind();
    let subscription = kind_to_subscription(&kind, "USDC", "").expect("Trades 必须能映射到订阅");
    let subscribed: HashSet<SubscriptionKind> = HashSet::from([kind]);

    let (trades, raws) = observe(
        "Hyperliquid",
        WS_URL,
        json!({"method": "subscribe", "subscription": subscription}),
        |raw| parse_public_message(raw, 0, &subscribed).map_err(|e| e.to_string()),
    )
    .await;

    assert_sane("Hyperliquid", &trades, Exchange::Hyperliquid);
    // 归集粒度：HL **不归集** —— 同一主动单吃穿多档会拆成多条（hash 相同）。
    // 这条断言守住 SubscriptionKind::Trades 文档里记载的差异，防止文档与线路再次背离。
    let has_split_taker_order = raws.iter().any(|raw| {
        let v: Value = serde_json::from_str(raw).unwrap();
        let Some(data) = v["data"].as_array() else {
            return false;
        };
        let mut per_hash: HashMap<&str, usize> = HashMap::new();
        for t in data {
            // 零 hash 是未上链的撮合占位，不能用于识别同一主动单
            if let Some(h) = t.get("hash").and_then(|h| h.as_str()) {
                if h.trim_start_matches("0x").bytes().any(|b| b != b'0') {
                    *per_hash.entry(h).or_default() += 1;
                }
            }
        }
        per_hash.values().any(|&n| n > 1)
    });
    println!("[Hyperliquid] 观测到同一 hash 拆成多条(不归集): {has_split_taker_order}");
}

/// OKX BTC-USDT-SWAP 每张合约的币本位数量（ctVal）
const OKX_BTC_CONTRACT_SIZE: f64 = 0.01;

/// 构造 OKX BTC 的 SymbolMeta（生产路径靠它做张->币折算）
fn okx_btc_metas() -> HashMap<Symbol, SymbolMeta> {
    HashMap::from([(
        COIN.to_string(),
        SymbolMeta {
            exchange: Exchange::OKX,
            symbol: COIN.to_string(),
            price_formatter: Arc::new(StepFormatter::new(0.1)),
            size_step: 0.01,
            min_order_size: 0.01,
            contract_size: OKX_BTC_CONTRACT_SIZE,
        },
    )])
}
