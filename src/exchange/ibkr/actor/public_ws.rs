//! IbkrPublicWs - 管理 IBKR WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接（读循环是子任务，见 [`WsConn`]）
//! - 处理 BBO 订阅 (smd topic)：订阅前先行情预热 (pre-flight snapshot)，见 `send_subscribe`；
//!   订阅后按 `SMD_REFRESH_INTERVAL` 周期性刷新，见 `refresh_subscriptions`
//! - 处理订单状态推送 (sor topic)
//! - 处理成交推送 (str topic)：两条消息配对补 commission，见 [`FillPairing`]
//! - 增量更新 bid/ask 缓存并发布 BBO 到行情总线
//!
//! **IBKR 的 smd 订阅会被服务端自动终止**（2026-04-10 起的行为变更）。终止时**没有 close 帧、
//! 没有错误**，流就静默停掉——ws 连接、ping、tickle 全都照常健康，因此完全不可从连接层察觉
//! (实测：韩股腿断流后报价冻结、且不自愈，同一条 ws 上另一条腿因被借券 snapshot 轮询顺带续命
//! 而无恙)。故本组件必须在到期前主动重订阅；IBKR 建议先 `umd+` 再 `smd+` 干净刷新。
//!
//! # BBO 只在价量真的变化时发布
//!
//! IBKR 的行情报文**不带交易所侧时间戳**，发布时只能盖本地时刻。若"收到报文"就发一条，
//! 这个本地时刻表达的就是**收包时刻**而不是**报价时刻**——而这条 ws 上还跑着周期性重订阅
//! （见 `refresh_subscriptions`）和他方字段（借券可借量/费率）的推送，两者都会带着 conid
//! 路由进 `handle_bbo`。结果是闭市期间源源不断发出"上一个交易日的收盘价 + 此刻的时间戳"。
//!
//! 下游（`spread_arb`）的三道闸门全部建立在 **"收到 BBO ≡ 有新报价"** 之上：陈旧的腿剔出
//! 候选、中断恢复时重置 EMA、陈旧告警。等价一破三道全塌，其中最坏的一幕是 EMA 因为"从未
//! 观测到中断"而永不重置——跳空开盘时，策略会把整段跳空幅度当成错价，在已经反映了新价格的
//! 盘口上吃单。故判据收在 [`BboCache::merge`] 一处：**价量没变就不发**。这样本地时刻重新
//! 等于"该报价第一次被观测到的时刻"，语义自洽。
//!
//! 代价是"行情静止"与"链路断流"在 BBO 流上不再可分。这是对的——链路存活本就不该由行情流
//! 兼职承担：ws keepalive、tickle、以及本组件的 smd 周期刷新（失败即致命）已经独立证明它。
//! 反过来，一个几分钟没动过的盘口本来就该被判为陈旧，策略不该在上面算偏离度。
//!
//! 该不变量**不覆盖进程重启后的第一条回包**：缓存是组件内存态，重启即清空，于是首条回包
//! 哪怕带的还是上一个交易日的收盘价，也会被判为"变化"而发布一次。这里不额外设防，因为本
//! 组件致命会经任务树整机退出——策略的 EMA 随之清零，需重新预热满 `ema_period`
//! 条才产出偏离度，闭市期间单条 BBO 撑不起任何信号。写在这里是为了说明它是**有条件的**
//! 不变量，别让后来者以为"发出来的每一条都必定是新报价"。
//!
//! 到期时限的两个来源不一致：IBKR 文档写 **10 分钟**；社区实测与客服答复是 **15 分钟**
//! （"the topic will terminate automatically after 15 minutes and you will need to send a new
//! request to continue to retrieve data for the instrument"，见 Voyz/ibind#145）。故以**更紧
//! 的 10 分钟**为约束（见 `SMD_SERVER_TTL`），刷新周期取 8 分钟。

use crate::domain::{now_ms, Exchange, ExchangeError, Fill, OrderStatus, OrderUpdate, Side, BBO};
use crate::exchange::client::{ExchangeCmd, SubscriptionKind, WsError};
use crate::exchange::ibkr::auth::IbkrAuth;
use crate::exchange::ibkr::client::IbkrClient;
use crate::exchange::ibkr::wire;
use crate::exchange::ws_loop::{self, WsConn};
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, MarketBus, MarketData, MarketEvent, SymbolAccountData,
};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::{handshake::client::generate_key, http, Message as WsMessage};

/// IbkrPublicWs 构造参数
pub(super) struct IbkrPublicWsArgs {
    /// 认证器 (共享，不可变)
    pub auth: Arc<dyn IbkrAuth>,
    /// REST client (共享)：订阅前的行情预热走它的 `preflight_market_data`
    pub client: Arc<IbkrClient>,
    /// 公共行情总线（BBO）
    pub market_bus: MarketBus,
    /// 账户私有事件总线（sor 订单更新 / str 成交 —— IBKR 用同一条 WS 混发公私两类 topic）
    pub account_bus: AccountBus,
    /// conid 映射 (symbol → conid)
    pub conids: HashMap<String, i64>,
    /// tickle 返回的 session_id (用于 WS Cookie)
    pub session_id: String,
    /// 除 BBO 之外、**同一 conid 上其他调用方需要的字段 tag**（如借券 poller 的可借量/费率/
    /// 可用性）。
    ///
    /// 为什么要在这里给：IBKR 对每个 conid 只维护一份活跃字段集，snapshot 与 ws 共用它。本
    /// 组件的 `umd+`→`smd+` 刷新会把该 conid 的字段集重建成"本次 smd 请求的那套"，别人注册
    /// 的字段会被连带抹掉（实测每逢刷新，借券 poller 就拿到只有 BBO 字段的空壳）。故字段集必须
    /// 是**并集**，并且由这一处（唯一重建它的地方）负责完整声明。
    pub extra_md_fields: Vec<String>,
}

/// 每个 conid 的 BBO 缓存 (IB 推送增量数据)
#[derive(Default)]
struct BboCache {
    bid_price: f64,
    ask_price: f64,
    bid_size: f64,
    ask_size: f64,
}

impl BboCache {
    /// 把报文里出现的 BBO 字段并进缓存，返回**是否有字段真的变了**。
    ///
    /// 返回值就是"这条报文算不算一次新报价"的唯一判据（见 `handle_bbo`）。缺失的字段保持
    /// 缓存原值——IBKR 推的是增量，"这条没带 bid" 不等于 "bid 没了"。
    ///
    /// 精确比较、不设容差：两侧的值来自同一套解析路径，同一个报价必然解析出同一个 f64，
    /// 容差只会把真实的最小价位跳动吞成"没变"。非有限值已在 [`wire::number`] 判死，
    /// 不会以 `NaN != NaN` 的形式伪造出变化。
    fn merge(&mut self, value: &serde_json::Value) -> bool {
        // IB 字段映射: "84"→bid_price, "86"→ask_price, "85"→ask_size, "88"→bid_size
        // IB 数字可能含逗号 "1,234.56"
        let mut changed = false;
        for (tag, slot) in [
            ("84", &mut self.bid_price),
            ("86", &mut self.ask_price),
            ("85", &mut self.ask_size),
            ("88", &mut self.bid_size),
        ] {
            let Some(n) = value.get(tag).and_then(wire::number) else {
                continue;
            };
            if *slot != n {
                *slot = n;
                changed = true;
            }
        }
        changed
    }
}

const MAX_SEEN_EXECUTIONS: usize = 10_000;
/// Fill commission 等待超时
const FILL_COMMISSION_TIMEOUT: Duration = Duration::from_secs(3);

/// BBO 订阅的 IBKR 字段 tag：84=bid_price 86=ask_price 85=ask_size 88=bid_size。
/// 预热 (snapshot) 与流式订阅 (smd) 共用同一份，保证"预热的字段"就是"要推的字段"。
const BBO_FIELDS: [&str; 4] = ["84", "86", "85", "88"];

/// 该 conid 上要注册的完整字段集 = BBO ∪ 其他调用方声明的字段（去重、保持顺序）。
///
/// IBKR 每个 conid 只有一份活跃字段集，而本组件是唯一重建它的地方（`umd+`→`smd+`），
/// 所以这里必须给全——漏掉谁的字段，谁就会在每次刷新后拿到空壳。
fn merge_md_fields(base: &[&str], extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    for f in extra {
        let f = f.trim();
        if !f.is_empty() && !out.iter().any(|x| x == f) {
            out.push(f.to_string());
        }
    }
    out
}

/// IBKR 服务端终止 smd 订阅的时限：取两个来源里**更紧**的那个（文档 10 分钟 / 实测 15 分钟，
/// 见模块文档）。取紧的一侧是因为代价不对称：估短了只是多刷几次（一次两条报文），估长了就是
/// 一段静默断流窗口——而这种断流从连接层看不出来。
const SMD_SERVER_TTL: Duration = Duration::from_secs(10 * 60);

/// smd 订阅刷新周期：必须 **显著小于** `SMD_SERVER_TTL`，否则会出现"到期了还没刷新"的静默
/// 断流窗口。取 8 分钟留 2 分钟余量（覆盖一次 REST 预热耗时 + 一轮多腿刷新）。
const SMD_REFRESH_INTERVAL: Duration = Duration::from_secs(8 * 60);

/// 构造 `smd` 订阅报文：`smd+{conid}+{"fields":[...]}`
fn smd_message(conid: i64, fields: &[&str]) -> String {
    format!("smd+{}+{}", conid, serde_json::json!({ "fields": fields }))
}

/// 构造 `umd` 退订报文
fn umd_message(conid: i64) -> String {
    format!("umd+{}+{{}}", conid)
}

/// 连接 IBKR WebSocket（需要 Cookie + User-Agent + 标准 WebSocket 握手 header；Gateway 模式
/// 另需跳过证书验证的 TLS connector，由 `auth.ws_connector()` 给出）
async fn connect(
    auth: &dyn IbkrAuth,
    session_id: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ExchangeError,
> {
    let ws_url = auth.ws_url();
    let connector = auth
        .ws_connector()
        .map_err(|e| ExchangeError::Other(format!("IBKR failed to build WS connector: {e}")))?;
    let cookie = auth.format_ws_cookie(session_id);
    let uri: http::Uri = ws_url
        .parse()
        .map_err(|e| ExchangeError::Other(format!("IBKR invalid WS URL: {e}")))?;
    let host = uri
        .host()
        .ok_or_else(|| ExchangeError::Other("IBKR WS URL missing host".into()))?;
    let ws_key = generate_key();
    let ws_request = http::Request::builder()
        .uri(&ws_url)
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", &ws_key)
        .header("Cookie", &cookie)
        .header("User-Agent", "ClientPortalGW/1")
        .body(())
        .map_err(|e| ExchangeError::Other(format!("IBKR failed to build WS request: {e}")))?;

    // 安全：ws_url 含 OAuth access_token，cookie 是 session cookie，严禁进日志。
    // 只打印 host。
    tracing::info!(host = %host, "Connecting IBKR WebSocket");

    let (ws_stream, _) = match connector {
        Some(conn) => {
            tokio_tungstenite::connect_async_tls_with_config(ws_request, None, false, Some(conn))
                .await
                .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?
        }
        None => tokio_tungstenite::connect_async(ws_request)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?,
    };
    Ok(ws_stream)
}

/// IBKR 公共 WS 组件（实际混发公私两类 topic，见 [`IbkrPublicWsArgs::account_bus`]）
pub(super) struct IbkrPublicWs {
    inbox: Inbox<ExchangeCmd>,
    conn: WsConn,
    market_bus: MarketBus,
    account_bus: AccountBus,
    /// REST client：订阅前行情预热用
    client: Arc<IbkrClient>,
    /// conid 映射 (symbol → conid)
    conids: HashMap<String, i64>,
    /// 反向映射 (conid → symbol)
    conid_to_symbol: Arc<HashMap<i64, String>>,
    /// 该 conid 要注册的完整字段集 (BBO ∪ 他方字段)，预热与 smd 共用同一份
    md_fields: Vec<String>,
    /// 已订阅的 kinds
    subscribed: HashSet<SubscriptionKind>,
    /// 每个 conid 的 BBO 缓存
    bbo_cache: HashMap<i64, BboCache>,
    /// str 成交的两条消息配对（等 commission）
    fills: FillPairing,
}

impl IbkrPublicWs {
    /// 在 `group` 下起 IBKR WS，等连接建好、sor/str 订阅发出，返回命令地址
    pub(super) async fn spawn(
        group: &Group,
        args: IbkrPublicWsArgs,
    ) -> anyhow::Result<Addr<ExchangeCmd>> {
        let (_, addr) = group
            .spawn_addressed("IbkrPublicWs", move |scope: Scope, inbox| async move {
                // 连接失败向上传播 → 启动期受控退出，不重试/不重连
                let (write, read) = connect(&*args.auth, &args.session_id).await?.split();
                Self::build(&scope, inbox, args, read, write).await
            })
            .await?;
        Ok(addr)
    }

    /// 在已建好的连接上完成构造：读循环挂成子任务、发 sor/str 订阅
    async fn build(
        scope: &Scope,
        inbox: Inbox<ExchangeCmd>,
        args: IbkrPublicWsArgs,
        read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
            + Unpin
            + Send
            + 'static,
        write: impl SinkExt<WsMessage> + Unpin + Send + 'static,
    ) -> anyhow::Result<Self> {
        let conn = WsConn::spawn(
            &scope.group("conn"),
            "IbkrPublicWsLoop",
            read,
            write,
            ws_loop::WsKeepalive::ibkr(),
        )?;

        // 订阅订单状态推送 (sor topic)。发送失败只可能是读循环已退出，原因由它自己上报
        match conn.send("sor+{}".to_string()).await {
            Ok(()) => tracing::info!("IBKR sor (order status) subscription sent"),
            Err(e) => tracing::error!(error = %e, "Failed to send IBKR sor subscription"),
        }
        // 订阅成交推送 (str topic)
        match conn.send(r#"str+{"realtimeUpdatesOnly":true}"#.to_string()).await {
            Ok(()) => tracing::info!("IBKR str (trades) subscription sent"),
            Err(e) => tracing::error!(error = %e, "Failed to send IBKR str subscription"),
        }

        let md_fields = merge_md_fields(&BBO_FIELDS, &args.extra_md_fields);
        tracing::info!(
            exchange = "IBKR",
            fields = ?md_fields,
            "IBKR 行情字段集 (BBO ∪ 他方字段；IBKR 每 conid 只维护一份，故必须给全)"
        );

        // 构建反向映射
        let conid_to_symbol: Arc<HashMap<i64, String>> =
            Arc::new(args.conids.iter().map(|(s, c)| (*c, s.clone())).collect());

        tracing::info!("IbkrPublicWs started");
        Ok(Self {
            inbox,
            conn,
            market_bus: args.market_bus,
            account_bus: args.account_bus.clone(),
            client: args.client,
            conids: args.conids,
            conid_to_symbol: conid_to_symbol.clone(),
            md_fields,
            subscribed: HashSet::new(),
            bbo_cache: HashMap::new(),
            fills: FillPairing::new(args.account_bus, conid_to_symbol),
        })
    }

    /// 发送 WebSocket 订阅消息
    ///
    /// **先预热再订阅**：IBKR 要求先对 conid 打一次 `/iserver/marketdata/snapshot`，IServer 才会
    /// 开始消费该合约的实时流；未预热就发 `smd` 只能收到订阅瞬间的一份值、之后不再有增量。
    ///
    /// 预热失败即**致命**（Err 上抛 → 本组件出错退出 → 经任务树整机退出，与借券
    /// snapshot poller 的"数据源失效即退出"同口径）：没预热的订阅是一条假活的行情线——有价、
    /// 但永远陈旧，比没有行情更危险。宁可退出重来，不留降级分支。
    async fn send_subscribe(&self, conid: i64) -> Result<(), WsError> {
        let fields: Vec<&str> = self.md_fields.iter().map(|s| s.as_str()).collect();
        self.client
            .preflight_market_data(conid, &fields)
            .await
            .map_err(|e| WsError::Network(format!("行情预热失败 conid={conid}: {e}")))?;
        self.conn.send(smd_message(conid, &fields)).await
    }

    /// 发送 WebSocket 取消订阅消息
    async fn send_unsubscribe(&self, conid: i64) -> Result<(), WsError> {
        self.conn.send(umd_message(conid)).await
    }

    /// 在服务端 TTL (`SMD_SERVER_TTL`) 到期前重订阅所有已订阅的 BBO 腿。
    ///
    /// 按 IBKR 建议的顺序 **先 `umd+` 再 `smd+`**（服务端过期后并不自动 unsubscribe，直接重发
    /// smd 可能落在一个"服务端认为还在、实际已停"的状态上）。`send_subscribe` 内含预热，因此
    /// 刷新走的是与首次订阅完全相同的路径——不为刷新另开一套逻辑。
    ///
    /// 任一腿刷新失败即返回 Err → 本组件出错退出、整机退出。这条腿刷新不掉就等于服务端 TTL
    /// 到期后静默变成死流（而这种死法从连接层完全看不出来），宁可退出重来，不留降级分支。
    ///
    /// **为什么重建时要带上他方字段**：IBKR 每个 conid 只维护一份活跃字段集（snapshot 与 ws
    /// 共用），`umd+` 会把它整个拆掉、`smd+` 按本次请求重建。曾经只重建 BBO 四字段，于是每逢
    /// 刷新借券 poller 就拿到没有 7636/7637 的空壳。现在订阅用的是 `md_fields`（BBO ∪ 他方
    /// 字段），刷新即把所有人的字段一起重新武装——不留"拆掉再等它自己回来"的窗口。
    async fn refresh_subscriptions(&self) -> Result<(), WsError> {
        for kind in &self.subscribed {
            let SubscriptionKind::BBO { symbol } = kind else {
                continue;
            };
            let Some(&conid) = self.conids.get(symbol) else {
                // 订阅时已校验过 conid 存在，此处缺失属不该发生
                tracing::error!(exchange = "IBKR", symbol = %symbol, "刷新 smd：symbol 无 conid 映射");
                continue;
            };
            self.send_unsubscribe(conid).await?;
            self.send_subscribe(conid).await?;
            tracing::info!(
                exchange = "IBKR",
                symbol = %symbol,
                conid,
                interval_secs = SMD_REFRESH_INTERVAL.as_secs(),
                "smd 订阅已刷新 (服务端到期会静默终止订阅，无 close 帧无错误)"
            );
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
        for kind in kinds {
            // IBKR 只支持 BBO；其余类型跳过并告警（静默丢弃会让上游误以为已订阅）
            let symbol = match &kind {
                SubscriptionKind::BBO { symbol } => symbol.clone(),
                other => {
                    tracing::warn!(
                        exchange = "IBKR",
                        kind = ?other,
                        "Unsupported subscription kind, skipping (IBKR only provides BBO)"
                    );
                    continue;
                }
            };

            if self.subscribed.contains(&kind) {
                continue;
            }

            let conid = match self.conids.get(&symbol) {
                Some(c) => *c,
                None => {
                    tracing::warn!(
                        exchange = "IBKR",
                        symbol = %symbol,
                        "Symbol not found in conid mapping, ignoring"
                    );
                    continue;
                }
            };

            self.send_subscribe(conid)
                .await
                .with_context(|| format!("IBKR 订阅 {symbol} (conid={conid}) 失败"))?;

            self.subscribed.insert(kind);
            tracing::info!(exchange = "IBKR", symbol = %symbol, conid, "Subscribed to BBO");
        }
        Ok(())
    }

    async fn unsubscribe(&mut self, kind: SubscriptionKind) -> anyhow::Result<()> {
        if !self.subscribed.remove(&kind) {
            return Ok(());
        }
        if let SubscriptionKind::BBO { ref symbol } = kind {
            if let Some(&conid) = self.conids.get(symbol) {
                self.send_unsubscribe(conid)
                    .await
                    .with_context(|| format!("IBKR 退订 {symbol} (conid={conid}) 失败"))?;
            }
        }
        Ok(())
    }

    /// 处理一帧；失败即本组件出错退出（带原文）
    fn handle_frame(&mut self, raw: &str) -> anyhow::Result<()> {
        self.handle_message(raw)
            .with_context(|| format!("IBKR WS 消息处理失败: {raw}"))
    }

    /// 解析并处理 WebSocket 消息
    fn handle_message(&mut self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();

        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

        // 路由: topic 以 "sor" 开头 → 订单状态更新; "str" 开头 → 成交推送
        if let Some(topic) = value.get("topic").and_then(|v| v.as_str()) {
            if topic.starts_with("sor") {
                self.handle_order_update(&value, local_ts);
                return Ok(());
            }
            if topic.starts_with("str") {
                return self.fills.on_trade_message(&value, local_ts, Instant::now());
            }
        }

        // 路由: 含 conid → BBO 行情
        let conid = match value.get("conid") {
            Some(v) => match v.as_i64() {
                Some(id) => id,
                None => {
                    tracing::warn!(raw = %value, "IBKR WS: conid field is not i64");
                    return Ok(());
                }
            },
            None => {
                tracing::debug!(raw = %value, "IBKR WS: no conid, skipping");
                return Ok(());
            }
        };

        self.handle_bbo(&value, conid, local_ts);
        Ok(())
    }

    /// 处理 BBO 行情消息
    fn handle_bbo(&mut self, value: &serde_json::Value, conid: i64, local_ts: u64) {
        let symbol = match self.conid_to_symbol.get(&conid) {
            Some(s) => s.clone(),
            None => {
                tracing::warn!(conid, "Received data for unknown conid");
                return;
            }
        };

        let cache = self.bbo_cache.entry(conid).or_default();

        // 价量没有任何变化 → 这不是一次新报价，不发布（见模块文档"BBO 只在价量真的变化时发布"）。
        // 会走到这里的两类报文：smd 周期重订阅的回包、以及同一 conid 上他方字段（借券可借量/
        // 费率/可用性）的推送——它们都带 conid、都会路由进来，但都不代表盘口动了。
        if !cache.merge(value) {
            // debug 级：默认不输出，开了就是为了分辨"报文根本没到"与"到了但盘口没动"——
            // 这条腿静默时，这两者是完全不同的故障（前者是断流，后者是闭市/静市）。
            tracing::debug!(conid, symbol = %symbol, "IBKR BBO: 价量无变化，不发布");
            return;
        }

        // 当 bid > 0 && ask > 0 时发布 BBO
        if cache.bid_price > 0.0 && cache.ask_price > 0.0 {
            let bbo = BBO {
                exchange: Exchange::IBKR,
                symbol,
                bid_price: cache.bid_price,
                bid_qty: cache.bid_size,
                ask_price: cache.ask_price,
                ask_qty: cache.ask_size,
                timestamp: local_ts,
            };
            self.market_bus.publish(MarketEvent {
                exchange_ts: local_ts,
                local_ts,
                data: MarketData::BBO(bbo),
            });
        }
    }

    /// 处理 sor 订单状态更新
    ///
    /// IBKR WS `sor` topic 推送订单更新，包含 order_ref (= 我们的 cOID)。
    /// 跳过无 order_ref 的非策略订单。
    fn handle_order_update(&self, value: &serde_json::Value, local_ts: u64) {
        let args = match value.get("args").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return,
        };

        for item in args {
            // order_ref 是我们下单时设置的 cOID
            let order_ref = match item.get("order_ref").and_then(|v| v.as_str()) {
                Some(r) if !r.is_empty() => r,
                _ => continue, // 跳过非策略订单
            };

            // 双态解析收在 wire::order_id 一处（三条路径共用，见该模块文档）
            let order_id = wire::order_id(item.get("orderId")).unwrap_or_default();

            let ib_status = match item.get("status").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };

            let filled_qty = item
                .get("filledQuantity")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);

            let status = match ib_status {
                "Submitted" if filled_qty > 0.0 => OrderStatus::PartiallyFilled,
                "PendingSubmit" | "PreSubmitted" | "Submitted" => OrderStatus::Pending,
                "Filled" => OrderStatus::Filled,
                "Cancelled" => OrderStatus::Cancelled,
                // Inactive 是 IBKR 的异常中间状态，通常表示订单被暂停或有问题。
                // 不更新 pending order 状态（保持 Created），让 timeout 机制正常清理。
                // 真正的结果会通过 REST 返回或后续 WS 推送终态。
                "Inactive" => {
                    tracing::warn!(
                        order_ref,
                        "IBKR order status: Inactive, ignoring (let timeout handle)"
                    );
                    continue;
                }
                other => {
                    tracing::debug!(
                        ib_status = other,
                        order_ref,
                        "IBKR unknown order status, ignoring"
                    );
                    continue;
                }
            };

            // 解析 symbol: 通过 conid 反查
            let conid = item.get("conid").and_then(|v| v.as_i64());
            let symbol = match conid.and_then(|c| self.conid_to_symbol.get(&c).cloned()) {
                Some(s) => s,
                None => {
                    tracing::warn!(
                        order_ref,
                        order_id,
                        ?conid,
                        "IBKR order update: cannot resolve symbol"
                    );
                    continue;
                }
            };

            let side_str = item.get("side").and_then(|v| v.as_str()).unwrap_or("");
            let side = match side_str {
                "BUY" | "B" => Side::Long,
                "SELL" | "S" => Side::Short,
                other => {
                    tracing::warn!(side = other, order_ref, "IBKR unknown order side, skipping");
                    continue;
                }
            };

            tracing::info!(
                symbol = %symbol,
                order_ref,
                order_id,
                ib_status,
                ?status,
                filled_qty,
                "IBKR order status update"
            );

            let update = OrderUpdate {
                order_id,
                client_order_id: Some(order_ref.to_string()),
                exchange: Exchange::IBKR,
                symbol,
                side,
                status,
                quantity: 0.0,
                filled_quantity: filled_qty,
            };

            self.account_bus.publish(AccountEvent {
                // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                account: crate::domain::AccountId::Live,
                exchange_ts: local_ts,
                local_ts,
                data: AccountData::Symbol(SymbolAccountData::OrderUpdate(update)),
            });
        }
    }
}

impl Task for IbkrPublicWs {
    async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
        // smd 刷新定时器：服务端到期会静默终止订阅，必须主动重订阅 (见模块文档)。
        // 用 interval_at 跳过 tokio::time::interval 的"立即首跳"——刚订阅完不需要马上刷。
        let mut refresh =
            tokio::time::interval_at(Instant::now() + SMD_REFRESH_INTERVAL, SMD_REFRESH_INTERVAL);
        refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tracing::info!(
            interval_secs = SMD_REFRESH_INTERVAL.as_secs(),
            ttl_secs = SMD_SERVER_TTL.as_secs(),
            "IBKR smd 刷新定时器已启动"
        );
        loop {
            let commission_due = self.fills.next_deadline();
            tokio::select! {
                cmd = self.inbox.recv() => match cmd {
                    Some(cmd) => self.handle_cmd(cmd).await?,
                    None => {
                        // 已经收到的帧处理完再走（可能有成交；仍在等 commission 的由 stop 补发）
                        while let Some(raw) = self.conn.next(scope).await? {
                            self.handle_frame(&raw)?;
                        }
                        return Ok(());
                    }
                },
                frame = self.conn.recv() => self.handle_frame(&frame?)?,
                // 刷新失败即致命：刷不上就等于这条腿到期后静默死掉，而这种死法从连接层完全
                // 看不出来，所以不留降级分支
                _ = refresh.tick() => self
                    .refresh_subscriptions()
                    .await
                    .context("IBKR smd 订阅刷新失败")?,
                _ = sleep_until_opt(commission_due) => self.fills.expire_due(Instant::now()),
            }
        }
    }

    /// 停机时把还在等 commission 的成交全部发出去（fee=0），直接发到账户总线
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.fills.flush_all();
        tracing::info!("IbkrPublicWs stopped");
        Ok(())
    }
}

/// 到点完成；`None` 永不完成（select 分支里"没有待办的定时"）
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

// ============================================================================
// str 成交配对
// ============================================================================

/// 暂存的 Fill (等待 commission 到来)
struct PendingFill {
    fill: Fill,
    order_update: Option<OrderUpdate>,
}

/// str 成交推送的配对与去重。
///
/// IBKR 对同一笔成交推送 2 条消息（同一 execution_id）：
/// - 第一条：无 commission 字段
/// - 第二条：带 commission 字段
///
/// 策略：第一条暂存，等第二条带 commission 后发布；超时 3 秒则以 fee=0 发布；
/// 停机时仍在等的以 fee=0 全部补发（见 [`Self::flush_all`]）。
struct FillPairing {
    account_bus: AccountBus,
    /// 反向映射 (conid → symbol)
    conid_to_symbol: Arc<HashMap<i64, String>>,
    /// 已完成发布的 execution_id 集合（去重）
    seen_executions: HashSet<String>,
    /// 按插入顺序记录 execution_id，用于淘汰最旧条目
    seen_executions_order: VecDeque<String>,
    /// 等待 commission 的 pending fills (execution_id → PendingFill)
    pending_fills: HashMap<String, PendingFill>,
    /// 各 pending fill 的 commission 超时时刻。超时时长固定，按入队顺序即按到期顺序；
    /// 第二条消息先到时条目留在队里，到期时 pending 里已没有它，空操作
    deadlines: VecDeque<(Instant, String)>,
}

impl FillPairing {
    fn new(account_bus: AccountBus, conid_to_symbol: Arc<HashMap<i64, String>>) -> Self {
        Self {
            account_bus,
            conid_to_symbol,
            seen_executions: HashSet::new(),
            seen_executions_order: VecDeque::new(),
            pending_fills: HashMap::new(),
            deadlines: VecDeque::new(),
        }
    }

    /// 处理一条 str 成交推送（`now` 用于排 commission 超时）。
    ///
    /// 解析后生成 Fill 更新仓位，同时（有 order_ref 时）生成 Filled OrderUpdate 关闭 pending order。
    fn on_trade_message(
        &mut self,
        value: &serde_json::Value,
        local_ts: u64,
        now: Instant,
    ) -> Result<(), WsError> {
        let args = match value.get("args").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => {
                tracing::debug!(raw = %value, "IBKR str message without args");
                return Ok(());
            }
        };

        for item in args {
            let execution_id = item
                .get("execution_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // execution_id 是这笔成交的身份：去重靠它、两条消息配对也靠它。没有它这笔
            // 成交既无法发布也无法追回 —— 丢一笔成交 = 本地持仓从此永久落后于交易所，
            // 必须致命（与本模块"预热失败即致命"同一姿态）。
            if execution_id.is_empty() {
                return Err(WsError::ParseError(format!(
                    "IBKR 成交推送缺 execution_id，无法去重与配对，丢弃即持仓失真: {item}"
                )));
            }

            // 已发布过的 execution_id，直接跳过
            if self.seen_executions.contains(&execution_id) {
                tracing::debug!(execution_id, "IBKR trade: already published, skipping");
                continue;
            }

            // IBKR WS str topic 用 "commission" (正确拼写)，REST API 用 "comission" (typo)
            let commission = match wire_number(item, "commission")? {
                Some(v) => Some(v),
                None => wire_number(item, "comission")?,
            };

            // 如果已有 pending fill，说明这是第二条消息（带 commission）
            if let Some(mut pending) = self.pending_fills.remove(&execution_id) {
                // 第二条消息带了 price/size 才校验（缺失是常态，IBKR 多数只补 commission）。
                // 一旦带了且与第一条不符，说明交易所修正了成交明细 —— 本地持仓均价会算错，
                // 必须致命而不是 warn 后照旧发布：半吊子的检查比没有更糟，它只会训练运维
                // 忽略告警。此前两个字段缺失时兜 0，导致每笔成交都误报不一致。
                if let Some(price2) = wire_number(item, "price")? {
                    if (price2 - pending.fill.price).abs() > FILL_FIELD_EPSILON {
                        return Err(WsError::ParseError(format!(
                            "IBKR 成交 {execution_id} 两条消息价格不一致（{} vs {price2}），\
                             本地持仓均价会失真",
                            pending.fill.price
                        )));
                    }
                }
                if let Some(size2) = wire_number(item, "size")? {
                    if (size2 - pending.fill.size).abs() > FILL_FIELD_EPSILON {
                        return Err(WsError::ParseError(format!(
                            "IBKR 成交 {execution_id} 两条消息数量不一致（{} vs {size2}），\
                             本地持仓会失真",
                            pending.fill.size
                        )));
                    }
                }
                match commission {
                    Some(fee) => {
                        pending.fill.fee = fee;
                        tracing::info!(execution_id, fee, "IBKR fill: commission received, publishing");
                    }
                    // 字段缺失是"这条没带手续费"，不是"手续费为 0" —— 按 0 发布与超时
                    // 路径同口径，但日志必须如实说明，不能谎报 "commission received"
                    None => {
                        pending.fill.fee = 0.0;
                        tracing::warn!(
                            execution_id,
                            "IBKR fill: 第二条成交消息未带 commission，以 fee=0 发布"
                        );
                    }
                }
                self.publish(pending);
                self.mark_seen(execution_id);
                continue;
            }

            // 第一条消息：解析并暂存
            let conid = item.get("conid").and_then(|v| v.as_i64());
            let symbol = match conid.and_then(|c| self.conid_to_symbol.get(&c).cloned()) {
                Some(s) => s,
                None => {
                    tracing::warn!(?conid, raw = %item, "IBKR trade: cannot resolve symbol");
                    continue;
                }
            };

            let order_ref = item
                .get("order_ref")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let side_str = item.get("side").and_then(|v| v.as_str()).unwrap_or("");
            let side = match side_str {
                "BUY" | "B" => Side::Long,
                "SELL" | "S" => Side::Short,
                other => {
                    return Err(WsError::ParseError(format!(
                        "IBKR 成交 {execution_id} 方向无法识别（'{other}'），丢弃即持仓失真"
                    )));
                }
            };

            // 价量解析不出来就是这笔成交不可用。此前是 warn + 跳过 —— 守卫本身对
            // （没让 0 进账本），但**丢弃整笔成交**同样致命：本地持仓从此落后于交易所，
            // 只能等对账连续失配后停机兜底。宁可现在就死。
            // 措辞区分"字段缺失"与"值非法"：两者都致命，但归因时不该把缺失说成 0
            let (Some(price), Some(size)) = (wire_number(item, "price")?, wire_number(item, "size")?)
            else {
                return Err(WsError::ParseError(format!(
                    "IBKR 成交 {execution_id} 缺 price/size 字段，丢弃即持仓失真: {item}"
                )));
            };
            if price <= 0.0 || size <= 0.0 {
                return Err(WsError::ParseError(format!(
                    "IBKR 成交 {execution_id} 价量非法（price={price}, size={size}）: {item}"
                )));
            }

            let fill = Fill {
                exchange: Exchange::IBKR,
                symbol: symbol.clone(),
                order_id: execution_id.clone(),
                client_order_id: if order_ref.is_empty() { None } else { Some(order_ref.to_string()) },
                side,
                price,
                size,
                timestamp: local_ts,
                fee: commission.unwrap_or(0.0),
                reason: crate::domain::FillReason::Normal, // IBKR 执行流不区分强平/ADL
            };
            let order_update = build_order_update(item, order_ref, &symbol, side, size);

            // 如果第一条就带了 commission（罕见），直接发布
            if let Some(fee) = commission {
                tracing::info!(
                    symbol = %symbol, side = ?side, price, size, order_ref, fee,
                    "IBKR fill: commission in first message, publishing immediately"
                );
                self.publish(PendingFill { fill, order_update });
                self.mark_seen(execution_id);
                continue;
            }

            // 暂存，等待 commission
            tracing::info!(
                symbol = %symbol, side = ?side, price, size, order_ref,
                "IBKR fill: pending commission, waiting up to {}s",
                FILL_COMMISSION_TIMEOUT.as_secs()
            );
            self.pending_fills
                .insert(execution_id.clone(), PendingFill { fill, order_update });
            self.deadlines
                .push_back((now + FILL_COMMISSION_TIMEOUT, execution_id));
        }

        Ok(())
    }

    /// 最早一笔待发成交的 commission 超时时刻
    fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.front().map(|(t, _)| *t)
    }

    /// Commission 超时处理 — 已到期且仍在等的，以 fee=0 发布。
    ///
    /// 这是第二条消息不来时该 fill 的**唯一兜底发布路径**（停机时另由 [`Self::flush_all`] 补发）：
    /// 它不被驱动就意味着这笔成交永远留在 `pending_fills` 里、永不发布 —— 本地持仓从此永久
    /// 落后于交易所。所以它由 `run` 的 `select!` 按 [`Self::next_deadline`] 直接驱动，不经任何可能投递
    /// 失败的消息。
    fn expire_due(&mut self, now: Instant) {
        while let Some((t, _)) = self.deadlines.front() {
            if *t > now {
                break;
            }
            let (_, execution_id) = self.deadlines.pop_front().expect("front 已确认存在");
            if let Some(pending) = self.pending_fills.remove(&execution_id) {
                tracing::warn!(
                    execution_id,
                    "IBKR fill: commission timeout ({}s), publishing with fee=0",
                    FILL_COMMISSION_TIMEOUT.as_secs()
                );
                self.publish(pending);
                self.mark_seen(execution_id);
            }
        }
    }

    /// 停机时把还在等 commission 的成交全部发出去（fee=0）：它们是**已经发生的成交**，
    /// 带着它们静默退出等于让本地持仓少记一笔，而下次启动的基线会把这个缺口固化下来
    /// （基线只写一次、永不纠正）。手续费不准可以事后核对，持仓少一笔不行。
    fn flush_all(&mut self) {
        self.deadlines.clear();
        let pending: Vec<_> = self.pending_fills.drain().collect();
        if pending.is_empty() {
            return;
        }
        tracing::warn!(
            count = pending.len(),
            "IbkrPublicWs 停机，把等待 commission 的成交以 fee=0 补发"
        );
        for (execution_id, fill) in pending {
            self.publish(fill);
            self.mark_seen(execution_id);
        }
    }

    /// 发布 Fill + OrderUpdate 到账户总线
    fn publish(&self, pending: PendingFill) {
        let local_ts = pending.fill.timestamp;
        self.account_bus.publish(AccountEvent {
            account: crate::domain::AccountId::Live,
            exchange_ts: local_ts,
            local_ts,
            data: AccountData::Symbol(SymbolAccountData::Fill(pending.fill)),
        });
        if let Some(order_update) = pending.order_update {
            self.account_bus.publish(AccountEvent {
                account: crate::domain::AccountId::Live,
                exchange_ts: local_ts,
                local_ts,
                data: AccountData::Symbol(SymbolAccountData::OrderUpdate(order_update)),
            });
        }
    }

    /// 标记 execution_id 为已发布，并维护 FIFO 淘汰
    fn mark_seen(&mut self, execution_id: String) {
        self.seen_executions_order.push_back(execution_id.clone());
        self.seen_executions.insert(execution_id);
        if self.seen_executions_order.len() > MAX_SEEN_EXECUTIONS {
            if let Some(oldest) = self.seen_executions_order.pop_front() {
                self.seen_executions.remove(&oldest);
            }
        }
    }
}

/// 构建 Filled OrderUpdate（如有 order_ref）
fn build_order_update(
    item: &serde_json::Value,
    order_ref: &str,
    symbol: &str,
    side: Side,
    size: f64,
) -> Option<OrderUpdate> {
    if order_ref.is_empty() {
        return None;
    }
    let order_id = wire::order_id(item.get("orderId")).unwrap_or_default();
    Some(OrderUpdate {
        order_id,
        client_order_id: Some(order_ref.to_string()),
        exchange: Exchange::IBKR,
        symbol: symbol.to_string(),
        side,
        status: OrderStatus::Filled,
        quantity: size,
        // 逐笔成交推出的 Filled 只知道这一笔的量（IBKR 的已知简化：部分成交也报 Filled）
        filled_quantity: size,
    })
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 成交明细比对的容差（价量都是交易所回传的精确值，不一致即真不一致）
const FILL_FIELD_EPSILON: f64 = 1e-9;

/// [`wire::optional_number`] 的 WS 侧封装：解析失败即 `WsError`（→ 本组件出错退出）
fn wire_number(item: &serde_json::Value, field: &str) -> Result<Option<f64>, WsError> {
    wire::optional_number(item, field).map_err(WsError::ParseError)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smd_message_carries_requested_fields() {
        assert_eq!(
            smd_message(17382246, &BBO_FIELDS),
            r#"smd+17382246+{"fields":["84","86","85","88"]}"#
        );
    }

    #[test]
    fn umd_message_has_empty_body() {
        assert_eq!(umd_message(899700992), "umd+899700992+{}");
    }

    /// 订阅字段集必须是并集：漏掉他方字段 → 每次 umd+/smd+ 刷新都会把对方的字段抹掉，
    /// 对方只能拿到空壳（线上表现为每轮一条"券源 snapshot 字段缺失"）。
    #[test]
    fn md_fields_union_keeps_bbo_and_appends_others() {
        let merged = merge_md_fields(&BBO_FIELDS, &["7636".into(), "7637".into(), "6509".into()]);
        assert_eq!(merged, ["84", "86", "85", "88", "7636", "7637", "6509"]);
    }

    #[test]
    fn md_fields_union_dedups_and_ignores_blank() {
        // 他方字段与 BBO 重叠 / 空串 / 带空白，都不应污染字段集
        let merged = merge_md_fields(&BBO_FIELDS, &["84".into(), "  ".into(), " 7637 ".into()]);
        assert_eq!(merged, ["84", "86", "85", "88", "7637"]);
    }

    #[test]
    fn md_fields_without_extras_is_just_bbo() {
        assert_eq!(merge_md_fields(&BBO_FIELDS, &[]), ["84", "86", "85", "88"]);
    }

    /// 首次收齐价量算一次新报价（缓存从零到有）
    #[test]
    fn merge_reports_change_on_first_quote() {
        let mut cache = BboCache::default();
        assert!(cache.merge(&serde_json::json!({"84": "1,234.5", "86": 1235.0, "88": 100, "85": 200})));
        assert_eq!(cache.bid_price, 1234.5);
        assert_eq!(cache.ask_price, 1235.0);
        assert_eq!(cache.bid_size, 100.0);
        assert_eq!(cache.ask_size, 200.0);
    }

    /// **本次修复的核心**：重订阅回包带的是同一份收盘价，不能算作一次新报价。
    /// 若算，本地时刻被刷新 → 下游"行情有多久没动过"当场失真 → 闭市期间用陈旧价出信号。
    #[test]
    fn merge_rejects_identical_requote() {
        let mut cache = BboCache::default();
        let quote = serde_json::json!({"84": 1234.5, "86": 1235.0, "88": 100, "85": 200});
        assert!(cache.merge(&quote));
        assert!(!cache.merge(&quote), "同值回包不是新报价");
        assert!(!cache.merge(&serde_json::json!({"84": "1,234.5"})), "换个线路形态也还是同一个值");
    }

    /// 同一 conid 上他方字段（借券可借量/费率/可用性）的推送同样会路由进 handle_bbo，
    /// 但它不带任何 BBO 字段——不能冒充一次新报价。
    #[test]
    fn merge_ignores_foreign_fields() {
        let mut cache = BboCache::default();
        assert!(cache.merge(&serde_json::json!({"84": 1234.5, "86": 1235.0})));
        assert!(!cache.merge(&serde_json::json!({"7636": 50000, "7637": "1.25", "6509": "RB"})));
        assert_eq!(cache.bid_price, 1234.5, "他方字段不得污染缓存");
    }

    /// 增量语义：这条没带 bid ≠ bid 没了，缺失字段保持原值；带来的新值才算变化。
    #[test]
    fn merge_keeps_absent_fields_and_detects_real_move() {
        let mut cache = BboCache::default();
        cache.merge(&serde_json::json!({"84": 1234.5, "86": 1235.0, "88": 100, "85": 200}));
        assert!(cache.merge(&serde_json::json!({"86": 1235.5})));
        assert_eq!(cache.bid_price, 1234.5, "未提及的 bid 保持原值");
        assert_eq!(cache.ask_price, 1235.5);
    }

    /// 只有盘口量变化也是真实行情：它直接决定信号的下单量，不能当成"没变"吞掉。
    #[test]
    fn merge_treats_size_only_move_as_change() {
        let mut cache = BboCache::default();
        cache.merge(&serde_json::json!({"84": 1234.5, "86": 1235.0, "88": 100, "85": 200}));
        assert!(cache.merge(&serde_json::json!({"88": 150})));
        assert_eq!(cache.bid_size, 150.0);
    }

    /// 非有限值必须在解析层就判死：否则 `NaN != NaN` 会让每一条报文都被判成"有变化"，
    /// 把本次修复从正门堵上的假新鲜从窗户放回来。
    #[test]
    fn merge_is_not_fooled_by_non_finite_values() {
        let mut cache = BboCache::default();
        cache.merge(&serde_json::json!({"84": 1234.5, "86": 1235.0, "88": 100, "85": 200}));
        assert!(!cache.merge(&serde_json::json!({"88": "NaN", "85": "inf"})));
        assert_eq!(cache.bid_size, 100.0, "垃圾值不得进缓存");
        assert_eq!(cache.ask_size, 200.0);
    }

    /// 刷新周期必须显著小于服务端 TTL——否则到期与刷新之间会出现静默断流窗口。
    /// 这条不变量是本次修复的全部要点，写成测试防止日后被"顺手调大"。
    #[test]
    fn refresh_interval_leaves_margin_before_server_ttl() {
        assert!(
            SMD_REFRESH_INTERVAL < SMD_SERVER_TTL,
            "刷新周期 {:?} 必须小于服务端 TTL {:?}",
            SMD_REFRESH_INTERVAL,
            SMD_SERVER_TTL
        );
        let margin = SMD_SERVER_TTL - SMD_REFRESH_INTERVAL;
        assert!(
            margin >= Duration::from_secs(120),
            "余量 {:?} 太小：一次刷新失败/预热耗时就可能越过 TTL",
            margin
        );
    }

    // ------------------------------------------------------------------
    // str 成交配对、commission 超时与停机补发
    // ------------------------------------------------------------------

    use crate::exchange::ibkr::gateway::IbkrGateway;
    use crate::tasks::Root;
    use tokio::sync::mpsc;

    const CONID: i64 = 265598;

    /// 一个订阅账户总线、把收到的事件转给测试的消费者组件
    struct Collector {
        inbox: Inbox<AccountEvent>,
        out: mpsc::UnboundedSender<AccountEvent>,
    }

    impl Task for Collector {
        async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
            while let Some(e) = self.inbox.recv().await {
                let _ = self.out.send(e);
            }
            Ok(())
        }
    }

    /// 在 `group` 下起 Collector（订阅完成才返回）
    async fn collect(group: &Group, bus: &AccountBus) -> mpsc::UnboundedReceiver<AccountEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let bus = bus.clone();
        group
            .spawn("collector", move |scope: Scope| async move {
                let (_addr, mut inbox) = scope.inbox::<AccountEvent>();
                inbox.subscribe(&bus, |_| true, |e| e);
                Ok(Collector { inbox, out: tx })
            })
            .await
            .unwrap();
        rx
    }

    fn conid_map() -> Arc<HashMap<i64, String>> {
        Arc::new(HashMap::from([(CONID, "AAPL".to_string())]))
    }

    fn trade(execution_id: &str, commission: Option<f64>) -> serde_json::Value {
        let mut item = serde_json::json!({
            "execution_id": execution_id, "conid": CONID, "side": "B",
            "price": "150.5", "size": "10", "order_ref": "cid-1", "orderId": 123
        });
        if let Some(c) = commission {
            item["commission"] = serde_json::json!(c);
        }
        serde_json::json!({ "topic": "str", "args": [item] })
    }

    /// 收一条事件（等不到就失败）
    async fn next(rx: &mut mpsc::UnboundedReceiver<AccountEvent>) -> AccountData {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("等待总线事件超时")
            .expect("收集器已退出")
            .data
    }

    fn expect_fill(data: AccountData) -> Fill {
        match data {
            AccountData::Symbol(SymbolAccountData::Fill(f)) => f,
            other => panic!("应为 Fill，实际 {other:?}"),
        }
    }

    fn expect_filled_update(data: AccountData) {
        match data {
            AccountData::Symbol(SymbolAccountData::OrderUpdate(u)) => {
                assert_eq!(u.status, OrderStatus::Filled);
                assert_eq!(u.client_order_id.as_deref(), Some("cid-1"));
            }
            other => panic!("应为 OrderUpdate，实际 {other:?}"),
        }
    }

    /// 第一条暂存、第二条带 commission 才发布；同一 execution_id 再来不重复发布
    #[tokio::test]
    async fn second_message_publishes_fill_with_commission() {
        let root = Root::new("t");
        let bus = AccountBus::new("account");
        let mut rx = collect(&root.group("consumers"), &bus).await;
        let mut fills = FillPairing::new(bus, conid_map());
        let now = Instant::now();

        fills.on_trade_message(&trade("E1", None), 1, now).unwrap();
        assert_eq!(fills.pending_fills.len(), 1, "第一条不带 commission 应暂存");
        fills.on_trade_message(&trade("E1", Some(1.25)), 2, now).unwrap();
        let fill = expect_fill(next(&mut rx).await);
        assert_eq!((fill.fee, fill.price, fill.size, fill.side), (1.25, 150.5, 10.0, Side::Long));
        expect_filled_update(next(&mut rx).await);

        fills.on_trade_message(&trade("E1", Some(1.25)), 3, now).unwrap();
        fills.expire_due(now + FILL_COMMISSION_TIMEOUT);
        root.shutdown().await.unwrap();
        assert!(rx.recv().await.is_none(), "已发布的成交不得重复发布");
    }

    /// commission 等不到：到期以 fee=0 发布，之后迟到的第二条不再重复发布
    #[tokio::test]
    async fn commission_timeout_publishes_with_zero_fee() {
        let root = Root::new("t");
        let bus = AccountBus::new("account");
        let mut rx = collect(&root.group("consumers"), &bus).await;
        let mut fills = FillPairing::new(bus, conid_map());
        let t0 = Instant::now();

        fills.on_trade_message(&trade("E2", None), 1, t0).unwrap();
        assert_eq!(fills.next_deadline(), Some(t0 + FILL_COMMISSION_TIMEOUT));
        fills.expire_due(t0 + FILL_COMMISSION_TIMEOUT - Duration::from_millis(1));
        assert_eq!(fills.pending_fills.len(), 1, "未到期不发布");
        fills.expire_due(t0 + FILL_COMMISSION_TIMEOUT);
        assert_eq!(expect_fill(next(&mut rx).await).fee, 0.0);
        expect_filled_update(next(&mut rx).await);
        assert_eq!(fills.next_deadline(), None);

        fills.on_trade_message(&trade("E2", Some(1.0)), 2, t0).unwrap();
        root.shutdown().await.unwrap();
        assert!(rx.recv().await.is_none(), "迟到的第二条不得重复发布");
    }

    /// 缺 execution_id / 方向无法识别 / 两条价格不一致：一律致命（丢弃即持仓失真）
    #[tokio::test]
    async fn unusable_trades_are_fatal() {
        let root = Root::new("t");
        let mut fills = FillPairing::new(AccountBus::new("account"), conid_map());
        let now = Instant::now();
        let mut no_id = trade("", None);
        no_id["args"][0].as_object_mut().unwrap().remove("execution_id");
        assert!(fills.on_trade_message(&no_id, 1, now).is_err());
        let mut bad_side = trade("E3", None);
        bad_side["args"][0]["side"] = serde_json::json!("X");
        assert!(fills.on_trade_message(&bad_side, 1, now).is_err());
        fills.on_trade_message(&trade("E4", None), 1, now).unwrap();
        let mut moved = trade("E4", Some(1.0));
        moved["args"][0]["price"] = serde_json::json!("151");
        assert!(fills.on_trade_message(&moved, 2, now).is_err());
        root.shutdown().await.unwrap();
    }

    /// 离线可构造的 IbkrPublicWs：WS 两端换成内存通道，REST client 不联网
    struct Harness {
        root: Root,
        frames: mpsc::Sender<Result<WsMessage, tokio_tungstenite::tungstenite::Error>>,
        _sent: mpsc::Receiver<WsMessage>,
        rx: mpsc::UnboundedReceiver<AccountEvent>,
    }

    async fn harness() -> Harness {
        let root = Root::new("t");
        // 生产者组先声明：停机时先停它（补发），再停消费者（收齐）
        let producers = root.group("producers");
        let consumers = root.group("consumers");
        let account_bus = AccountBus::new("account");
        let rx = collect(&consumers, &account_bus).await;

        let conids = HashMap::from([("AAPL".to_string(), CONID)]);
        let (frames, frame_rx) = mpsc::channel(16);
        let (sent_tx, sent) = mpsc::channel(64);
        let args = IbkrPublicWsArgs {
            auth: Arc::new(IbkrGateway::new("https://127.0.0.1:1/v1/api/".into())),
            client: Arc::new(IbkrClient::for_test(conids.clone())),
            market_bus: MarketBus::new("market"),
            account_bus,
            conids,
            session_id: "test".into(),
            extra_md_fields: vec![],
        };
        producers
            .spawn_addressed("IbkrPublicWs", move |scope: Scope, inbox| async move {
                IbkrPublicWs::build(
                    &scope,
                    inbox,
                    args,
                    tokio_stream::wrappers::ReceiverStream::new(frame_rx),
                    tokio_util::sync::PollSender::new(sent_tx),
                )
                .await
            })
            .await
            .unwrap();
        Harness { root, frames, _sent: sent, rx }
    }

    async fn feed(h: &Harness, msg: serde_json::Value) {
        h.frames.send(Ok(WsMessage::Text(msg.to_string()))).await.unwrap();
    }

    /// **停机补发送达总线**：还在等 commission 的成交，在 IbkrPublicWs 停机时以 fee=0 直接
    /// 发到账户总线，并被（后停的）消费者收到 —— 漏掉会让本地持仓少记一笔并被下次基线固化
    #[tokio::test]
    async fn stop_flushes_pending_fill_to_account_bus() {
        let mut h = harness().await;
        feed(&h, trade("E9", None)).await;
        // 远短于 3 秒的 commission 超时：确保下面收到的是停机补发，而不是超时发布
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(h.rx.try_recv().is_err(), "等 commission 期间不应发布");

        h.root.shutdown().await.unwrap();
        let fill = expect_fill(next(&mut h.rx).await);
        assert_eq!((fill.order_id.as_str(), fill.fee), ("E9", 0.0));
        expect_filled_update(next(&mut h.rx).await);
    }

    /// run 里的超时分支：3 秒等不到 commission 即以 fee=0 发布（不等停机）
    #[tokio::test(start_paused = true)]
    async fn run_publishes_on_commission_timeout() {
        let mut h = harness().await;
        feed(&h, trade("E10", None)).await;
        let fill = expect_fill(next(&mut h.rx).await);
        assert_eq!((fill.order_id.as_str(), fill.fee), ("E10", 0.0));
        expect_filled_update(next(&mut h.rx).await);
        h.root.shutdown().await.unwrap();
    }
}
