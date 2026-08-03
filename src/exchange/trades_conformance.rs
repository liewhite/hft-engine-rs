//! trades 公共成交流的**线路一致性测试** —— 用真实报文校验 codec 口径，不靠文档猜。
//!
//! 直连三个所的公共 WS（无需凭证），订阅 trades，打印原始报文并用**生产 codec** 解析。
//! 校验三件事：
//!   1. 字段名/类型与 codec 声明一致（解析不报错即通过）
//!   2. `side` / `m` 的**主动方**语义能被 codec 接受（各所编码不同：Binance `m` 布尔、
//!      OKX `buy`/`sell`、Hyperliquid `B`/`A`）
//!   3. 价格/数量/时间戳量级正确（单位没搞错）
//!
//! 放在 crate 内而非 `tests/`：codec 是 `pub(crate)` 的线路 DTO，不该为测试放开公共 API。
//! 需要联网，故全部 `#[ignore]`，按需运行：
//! ```sh
//! cargo test --lib trades_conformance -- --ignored --nocapture
//! ```

use crate::domain::MarketTrade;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// 每个所观测到多少笔成交后停止
const SAMPLE_TRADES: usize = 5;
/// 单个所的观测超时（BTC 成交密集，5 笔通常 1 秒内到齐）
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(30);
/// 毫秒时间戳下界（2020-01-01），用于识别"秒/微秒被当成毫秒"这类单位错误
const MS_EPOCH_FLOOR: u64 = 1_577_836_800_000;

/// 连接 WS、发送订阅、收集前 [`SAMPLE_TRADES`] 笔成交。
///
/// `parse` 把一条原始报文映射为零到多笔成交（非成交报文返回空 Vec）；返回 `Err` 表示
/// **解析失败**，即 codec 与真实报文不符 —— 此时 panic 而非跳过，否则测试就失去意义。
async fn observe(
    exchange_name: &str,
    url: &str,
    subscribe: Value,
    mut parse: impl FnMut(&str) -> Result<Vec<MarketTrade>, String>,
) -> Vec<MarketTrade> {
    let (mut ws, _) = connect_async(url)
        .await
        .unwrap_or_else(|e| panic!("[{exchange_name}] connect {url} failed: {e}"));

    ws.send(Message::Text(subscribe.to_string()))
        .await
        .unwrap_or_else(|e| panic!("[{exchange_name}] send subscribe failed: {e}"));

    let mut trades = Vec::new();
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

        let parsed = parse(&raw)
            .unwrap_or_else(|e| panic!("[{exchange_name}] codec 与真实报文不符: {e}\nraw: {raw}"));
        if parsed.is_empty() {
            if unmatched_left > 0 {
                unmatched_left -= 1;
                println!("[{exchange_name}] non-trade msg: {raw}");
            }
            continue;
        }
        println!("[{exchange_name}] raw: {raw}");
        for t in &parsed {
            println!(
                "[{exchange_name}] parsed: symbol={} price={} qty={} is_buyer_maker={} ts={}",
                t.symbol, t.price, t.qty, t.is_buyer_maker, t.timestamp
            );
        }
        trades.extend(parsed);
    }

    let _ = ws.close(None).await;
    assert!(
        !trades.is_empty(),
        "[{exchange_name}] 观测窗口内未收到任何成交（订阅未生效或 channel 名有误）"
    );
    trades
}

/// 三个所共有的量级 sanity。
fn assert_sane(exchange_name: &str, trades: &[MarketTrade], expect_symbol: &str) {
    for t in trades {
        assert_eq!(t.symbol, expect_symbol, "[{exchange_name}] symbol 解析错误");
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
    use crate::exchange::binance::codec::AggTrade;
    // aggTrade 归属 /market/ws；订到 /public/ws 会被 ack 但永不推数据（实测确认）
    use crate::exchange::binance::WS_MARKET_URL;

    let trades = observe(
        "Binance",
        WS_MARKET_URL,
        json!({"method": "SUBSCRIBE", "params": ["btcusdt@aggTrade"], "id": 1}),
        |raw| {
            let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            if v.get("e").and_then(|e| e.as_str()) != Some("aggTrade") {
                return Ok(Vec::new());
            }
            let agg: AggTrade = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            Ok(vec![agg.to_market_trade("USDT")?])
        },
    )
    .await;

    assert_sane("Binance", &trades, "BTC");
}

#[tokio::test]
#[ignore = "需要联网连交易所"]
async fn okx_trades_matches_codec() {
    use crate::exchange::okx::codec::{TradeData, WsPush};
    use crate::exchange::okx::WS_PUBLIC_URL;

    let trades = observe(
        "OKX",
        WS_PUBLIC_URL,
        json!({"op": "subscribe", "args": [{"channel": "trades", "instId": "BTC-USDT-SWAP"}]}),
        |raw| {
            let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            let is_trades = v
                .get("arg")
                .and_then(|a| a.get("channel"))
                .and_then(|c| c.as_str())
                == Some("trades");
            if !is_trades || v.get("data").is_none() {
                return Ok(Vec::new());
            }
            let push: WsPush<TradeData> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            let inst_id = push.arg.inst_id.as_ref().ok_or("missing instId")?;
            push.data.iter().map(|d| d.to_market_trade(inst_id)).collect()
        },
    )
    .await;

    // 注意：OKX 的 qty 此处仍是**张数**（币本位折算在 actor 层做），故只校验为正
    assert_sane("OKX", &trades, "BTC");
}

#[tokio::test]
#[ignore = "需要联网连交易所"]
async fn hyperliquid_trades_matches_codec() {
    use crate::exchange::hyperliquid::codec::WsTrade;
    use crate::exchange::hyperliquid::WS_URL;

    let trades = observe(
        "Hyperliquid",
        WS_URL,
        json!({"method": "subscribe", "subscription": {"type": "trades", "coin": "BTC"}}),
        |raw| {
            let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            if v.get("channel").and_then(|c| c.as_str()) != Some("trades") {
                return Ok(Vec::new());
            }
            let ws_trades: Vec<WsTrade> =
                serde_json::from_value(v["data"].clone()).map_err(|e| e.to_string())?;
            ws_trades.iter().map(|t| t.to_market_trade()).collect()
        },
    )
    .await;

    assert_sane("Hyperliquid", &trades, "BTC");
}
