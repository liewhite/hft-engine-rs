//! OkxBusinessWs - 管理 OKX Business WebSocket 连接 (K线数据)
//!
//! 职责:
//! - 维护 Business WebSocket 连接 (wss://ws.okx.com:8443/ws/v5/business)
//! - 处理 Candle 类型的订阅/退订命令
//! - 收到订阅时先通过 REST 获取 100 根历史 K线并发布，再 WS 订阅实时推送
//! - 直接解析消息并发布到行情总线

use crate::domain::{CandleInterval, now_ms, Exchange, ExchangeError, Symbol};
use crate::exchange::client::{ExchangeCmd, SubscriptionKind, WsError};
use crate::exchange::okx::codec::{
    CandleRawData, WsPush, candle_interval_to_okx_bar, okx_channel_to_candle_interval,
    parse_candle_data,
};
use crate::exchange::okx::{to_okx, REST_BASE_URL, WS_BUSINESS_URL};
use crate::exchange::ws_loop::{self, WsConn};
use crate::messaging::{HistoryCandles, MarketBus, MarketData, MarketEvent};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;

/// Business WebSocket 组件 (K线)
pub(super) struct OkxBusinessWs {
    inbox: Inbox<ExchangeCmd>,
    conn: WsConn,
    /// 行情总线（发布事件）
    market_bus: MarketBus,
    /// 计价币种 (e.g., "USDT")
    quote: String,
    /// 已订阅的 kinds (用于去重)
    subscribed: HashSet<SubscriptionKind>,
    /// HTTP 客户端 (用于获取历史 K线, 公开 API 无需签名)
    http_client: Client,
}

impl OkxBusinessWs {
    /// 在 `group` 下起 Business WS，等连接建好，返回命令地址
    pub(super) async fn spawn(
        group: &Group,
        market_bus: MarketBus,
        quote: String,
    ) -> anyhow::Result<Addr<ExchangeCmd>> {
        let (_, addr) = group
            .spawn_addressed("OkxBusinessWs", move |scope: Scope, inbox| async move {
                // 连接 Business WebSocket（失败向上传播 → 启动期受控退出）
                let (ws_stream, _) = tokio_tungstenite::connect_async(WS_BUSINESS_URL)
                    .await
                    .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;
                let (write, read) = ws_stream.split();
                let conn = WsConn::spawn(&scope.group("conn"), "OkxBusinessWsLoop", read, write, ws_loop::WsKeepalive::okx())?;

                let http_client = Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .map_err(|e| ExchangeError::Other(format!("OKX business HTTP client build: {e}")))?;

                tracing::info!("OkxBusinessWs started");
                Ok(Self {
                    inbox,
                    conn,
                    market_bus,
                    quote,
                    subscribed: HashSet::new(),
                    http_client,
                })
            })
            .await?;
        Ok(addr)
    }

    /// 发送 WS 订阅消息
    async fn send_subscribe_batch(&self, args: Vec<serde_json::Value>) -> Result<(), WsError> {
        if args.is_empty() {
            return Ok(());
        }
        let msg = json!({
            "op": "subscribe",
            "args": args
        })
        .to_string();
        self.conn.send(msg).await
    }

    /// 发送 WS 取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_business_arg(kind, &self.quote).ok_or_else(|| {
            WsError::ParseError("Non-Candle kind reached OkxBusinessWs unsubscribe (routing bug)".to_string())
        })?;
        let msg = json!({
            "op": "unsubscribe",
            "args": [arg]
        })
        .to_string();
        self.conn.send(msg).await
    }

    /// 获取历史 K线并以 HistoryCandles 事件发布
    async fn fetch_and_publish_history(
        &self,
        symbol: &Symbol,
        interval: CandleInterval,
    ) -> Result<(), WsError> {
        let inst_id = to_okx(symbol, &self.quote);
        let bar = candle_interval_to_okx_bar(interval);
        let url = format!(
            "{}/api/v5/market/candles?instId={}&bar={}&limit=100",
            REST_BASE_URL, inst_id, bar
        );

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<CandleRawData>,
        }

        let resp = self.http_client.get(&url).send().await
            .map_err(|e| WsError::Network(format!("History candles request failed: {}", e)))?;

        let data: Response = resp.json().await
            .map_err(|e| WsError::ParseError(format!("History candles parse failed: {}", e)))?;

        if data.code != "0" {
            return Err(WsError::ParseError(format!(
                "OKX history candles API error: code={}, msg={}", data.code, data.msg
            )));
        }

        // OKX 返回的数据按时间倒序，反转为正序
        let mut candles: Vec<_> = data.data.iter().rev()
            .map(|raw| parse_candle_data(raw, &inst_id, &self.quote, interval))
            .collect::<Result<Vec<_>, _>>()
            ?;

        // 最新一条若未 confirm 则删除（WS 实时推送会补上）
        if candles.last().is_some_and(|c| !c.confirm) {
            candles.pop();
        }

        // 空批没有内容可给策略，不发布，留一条可见信号
        if candles.is_empty() {
            tracing::warn!(%inst_id, "历史 K 线为空（全部未 confirm 或交易所返回空），不发布");
            return Ok(());
        }

        let count = candles.len();
        let local_ts = now_ms();
        self.market_bus.publish(MarketEvent {
            exchange_ts: local_ts,
            local_ts,
            data: MarketData::HistoryCandles(HistoryCandles {
                exchange: Exchange::OKX,
                symbol: symbol.clone(),
                candles,
            }),
        });

        tracing::info!(
            symbol = %symbol,
            interval = %interval,
            count,
            "Published history candles"
        );
        Ok(())
    }

    /// 解析并处理 Business WS 消息
    fn handle_message(&self, raw: &str) -> anyhow::Result<()> {
        let events = parse_business_message(raw, &self.quote, now_ms())
            .with_context(|| format!("OKX business WS 解析失败: {raw}"))?;
        for event in events {
            self.market_bus.publish(event);
        }
        Ok(())
    }

    async fn handle_cmd(&mut self, cmd: ExchangeCmd) -> anyhow::Result<()> {
        match cmd {
            ExchangeCmd::Subscribe(kind) => self.subscribe_batch(vec![kind]).await,
            ExchangeCmd::SubscribeBatch(kinds) => self.subscribe_batch(kinds).await,
            ExchangeCmd::Unsubscribe(kind) => self.unsubscribe(kind).await,
        }
    }

    async fn subscribe_batch(&mut self, kinds: Vec<SubscriptionKind>) -> anyhow::Result<()> {
        let mut new_args = Vec::new();
        let mut new_kinds = Vec::new();

        for kind in kinds {
            // 品种在不在目录、这一所支不支持这种订阅，投产时已校验（provisioning::validate_subscriptions）；
            // 到这里还对不上就是路由或校验的 bug，报错停机，不静默跳过
            if self.subscribed.contains(&kind) {
                continue;
            }
            let arg = kind_to_business_arg(&kind, &self.quote)
                .ok_or_else(|| anyhow::anyhow!("非 K 线订阅被路由到 OkxBusinessWs（路由 bug）: {kind:?}"))?;
            new_args.push(arg);
            new_kinds.push(kind);
        }

        // 1. 对每个新订阅先获取历史 K线（阻塞式，保证历史数据先于实时到达）
        for kind in &new_kinds {
            if let SubscriptionKind::Candle { symbol, interval } = kind {
                self.fetch_and_publish_history(symbol, *interval)
                    .await
                    .with_context(|| format!("OKX 历史 K 线获取失败: {symbol} {interval}"))?;
            }
        }

        // 2. WS 订阅实时推送（历史获取完毕后再订阅，确保顺序）
        if !new_args.is_empty() {
            tracing::info!(
                exchange = "OKX",
                count = new_args.len(),
                "Batch subscribing to business channels"
            );
            self.send_subscribe_batch(new_args)
                .await
                .context("OKX business 批量订阅发送失败")?;
        }

        // 3. 记录已订阅
        for kind in new_kinds {
            self.subscribed.insert(kind);
        }
        Ok(())
    }

    async fn unsubscribe(&mut self, kind: SubscriptionKind) -> anyhow::Result<()> {
        if !self.subscribed.remove(&kind) {
            return Ok(());
        }
        self.send_unsubscribe(&kind)
            .await
            .context("OKX business 取消订阅发送失败")?;
        Ok(())
    }
}

impl Task for OkxBusinessWs {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                cmd = self.inbox.recv() => match cmd {
                    Some(cmd) => self.handle_cmd(cmd).await?,
                    None => return Ok(()),
                },
                frame = self.conn.recv() => self.handle_message(&frame?)?,
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_business_message(raw: &str, quote: &str, local_ts: u64) -> Result<Vec<MarketEvent>, WsError> {
    // 应用层心跳应答：ws_loop 周期发文本 "ping"，OKX 回文本 "pong"（非 JSON）
    if raw == "pong" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查事件响应（控制消息）
    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
        match event {
            "subscribe" | "unsubscribe" => return Ok(Vec::new()),
            "error" => {
                let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(WsError::ParseError(format!(
                    "OKX business error: code={}, msg={}",
                    code, msg
                )));
            }
            _ => return Ok(Vec::new()),
        }
    }

    // 获取频道
    let channel = value
        .get("arg")
        .and_then(|a| a.get("channel"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| WsError::ParseError(format!("Missing channel: {}", raw)))?;

    // 匹配 candle 频道 (candle1m, candle5m, candle1H, ...)
    if let Some(interval) = okx_channel_to_candle_interval(channel) {
        let push: WsPush<CandleRawData> = serde_json::from_str(raw)
            .map_err(|e| WsError::ParseError(format!("{} parse: {}", channel, e)))?;
        let inst_id = push
            .arg
            .inst_id
            .as_ref()
            .ok_or_else(|| WsError::ParseError(format!("Missing instId in {}", channel)))?;

        let mut events = Vec::new();
        for raw_data in &push.data {
            let candle = parse_candle_data(raw_data, inst_id, quote, interval)
                ?;
            events.push(MarketEvent {
                exchange_ts: candle.open_time,
                local_ts,
                data: MarketData::Candle(candle),
            });
        }
        return Ok(events);
    }

    tracing::warn!(channel, raw, "Unknown OKX business channel");
    Ok(Vec::new())
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 把 Candle 订阅转成 OKX business 频道 arg。
///
/// 仅处理 `Candle`；其它类型由 OkxNode 路由到 OkxPublicWs，正常不会到达此处，
/// 返回 `None` 表示路由错误，由调用方记录并跳过（不 panic）。
fn kind_to_business_arg(kind: &SubscriptionKind, quote: &str) -> Option<serde_json::Value> {
    match kind {
        SubscriptionKind::Candle { symbol, interval } => {
            let bar = candle_interval_to_okx_bar(*interval);
            Some(json!({
                "channel": format!("candle{}", bar),
                "instId": to_okx(symbol, quote)
            }))
        }
        _ => None,
    }
}
