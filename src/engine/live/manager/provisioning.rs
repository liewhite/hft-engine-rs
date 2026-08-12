//! 策略投产与撤下的编排。
//!
//! - **投产**（[`super::AddStrategies`]）：校验订阅可行性 → 撤遗留挂单 → 拉持仓基线
//!   （一次快照喂 executor / 对账镜像 / 观测镜像三类消费者）→ spawn（出生即带基线）
//!   → 注册 → 放行行情；任一步失败回滚到"什么都没发生"。
//! - **撤下**（[`RemoveStrategies`]）：停实例 → 撤遗留挂单 → 可选 reduce-only 平仓
//!   （经唯一下单出口 OrderGateway）；任何一环未完成都如实向调用方报错。
//!
//! # 为什么是 [`Provisioner`] 而不是 `impl ManagerActor`
//!
//! 编排必须经 manager 的邮箱串行化，所以它**不是**新 actor —— 两个 `Message` handler
//! 仍在 `ManagerActor` 上，只是把活委托下来。拆出独立类型是为了让**依赖面成为类型声明**
//! （`docs/architecture.md` P3，与 R2 给持仓账本换成 `PositionBook` 是同一手法）：
//!
//! 实测 manager 的 15 个字段里，有 7 个**只有投产编排用**（accounts / capabilities /
//! income_processor / metrics / exchange_actors / order_gateway / executors）。
//! 它们搬进本类型之后，编译器保证编排碰不到三条总线与停机分组，manager 也碰不到
//! executor 列表与下单出口。
//!
//! # 两个必须传进来的东西
//!
//! - `symbol_metas`：引擎级的合约目录，快照面（`GetAllSymbolMetas`）也要读，
//!   属于 manager 这个组合根，不该复制一份进来（P1）。
//! - `producers`：新建的 executor 要登记进**停机链的生产者组**。传的是
//!   [`ChildRegistrar`] 而不是 `&mut ChildGroup` —— 后者连 `shutdown()` 都能调，
//!   而"何时停、按什么顺序停"只由 `ManagerActor` 说了算。借出面上只有 `spawn` / `remove`。
//!
//! 如实说明能力面的边界：三个停机分组同为 `ChildGroup`，**"传的是哪一组"仍由调用点
//! 写对，编译器分不出来**；`ChildRegistrar` 限制的是能对它做什么，不是它是谁。

use super::{ManagerActor, StrategySpec};
use crate::actor_lifecycle::ChildRegistrar;
use crate::engine::live::{
    ExecutorActor, ExecutorArgs, GetPositions, PlaceVerdict, RegisterExecutor, RegisterSymbols,
    UnregisterExecutor,
};
use crate::domain::{
    now_ms, AccountId, Exchange, ExchangeError, Order, OrderType, OrderUpdate, Side, Symbol,
    SymbolMeta,
};
use crate::exchange::{AccountClient, SubscriptionKind};
use kameo::actor::{ActorId, ActorRef};
use kameo::message::{Context, Message};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// 等待一个 executor 有序停下的上限。
///
/// `ExecutorActor` 的 handler 是纯策略步进 + 往 unbounded 邮箱投递，都是亚毫秒级工作，
/// 正常远不到这个量级。留 5 秒是为了不因偶发调度抖动就走异常分支。
const EXECUTOR_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// 一个已注册的策略实例。
///
/// 住在本文件、且**私有**：只有编排读写它（投产时 push、撤下时 retain）。
struct RegisteredExecutor {
    executor: ActorRef<ExecutorActor>,
    account: AccountId,
    symbols: HashSet<Symbol>,
}

/// 策略投产与撤下的编排器。
///
/// 由 [`ManagerActor`] 持有并在自己的 handler 里调用（因而天然被邮箱串行化）；
/// 字段就是它的依赖声明，见模块文档。
pub(super) struct Provisioner {
    /// 各所的**账户私有**能力。键集 = 配了凭证的所；缺席即"该所只接公共行情"。
    accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    /// 各所的订阅能力查询（知识在适配层，随 ExchangeSetup 携带；投产校验用）
    capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool>,
    /// 事件分发层（注册 / 注销 executor 的订阅）
    income_processor: ActorRef<crate::engine::live::IncomeProcessorActor>,
    /// 观测镜像（投产时随 symbol 一起收基线）
    metrics: ActorRef<crate::engine::live::MetricsActor>,
    /// 对账镜像（同上；它是唯一被 REST 持续校验的那份折叠）
    position_ledger: ActorRef<crate::engine::live::PositionLedgerActor>,
    /// 策略信号总线（新建 executor 的产出去处）
    outcome_pubsub: ActorRef<crate::engine::live::OutcomePubSub>,
    /// 唯一的实盘下单出口 —— 降级平仓经此下单，与策略信号同一条路
    order_gateway: Arc<crate::engine::live::OrderGateway>,
    /// 各所适配层（放行 / 收回行情订阅）
    exchange_actors: HashMap<Exchange, Box<dyn super::ExchangeActorOps>>,
    /// 已注册的策略实例，供动态撤下使用
    executors: Vec<RegisteredExecutor>,
}

/// 投产期订阅可行性校验（纯函数，供单测）。
///
/// 逐条检查策略声明的 (exchange, kind)：交易所未配置（不在 `capabilities` 里）、适配层
/// 不支持该 kind（能力函数由各适配层提供、随 [`crate::engine::live::assembly::ExchangeSetup`] 携带）、或该 symbol
/// **没有 SymbolMeta**，都返回 Err。一次性汇总全部问题，不在第一条就停 —— 让策略作者
/// 一次看全。
///
/// # 为什么 SymbolMeta 缺失必须在这里拦
///
/// 各所的元数据加载都会剔除字段异常的合约（OKX 的 `filter_map`、Binance 的 step 解析）。
/// 被剔掉的 symbol 若正好是策略要交易的，故障会以三种毫不相干的面貌分散出现：下单被
/// "No SymbolMeta" 拒、私有回报因无法折算张数被丢弃（**持仓从此落后于交易所**）、行情
/// 订阅被忽略。三处都只是症状，根因是"配置要交易的 symbol 没有元数据"，而这一点在投产
/// 期一次就能查清 —— 此时尚无任何副作用，Err 即"什么都没发生"。
fn validate_subscriptions(
    subscriptions: &HashSet<(Exchange, SubscriptionKind)>,
    capabilities: &HashMap<Exchange, fn(&SubscriptionKind) -> bool>,
    symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    for (exchange, kind) in subscriptions {
        let Some(supports) = capabilities.get(exchange) else {
            problems.push(format!("{exchange} 未配置（{kind:?} 无从订阅）"));
            continue;
        };
        if !supports(kind) {
            problems.push(format!("{exchange} 的适配层不支持 {kind:?}"));
        } else if !symbol_metas.contains_key(&(*exchange, kind.symbol().clone())) {
            problems.push(format!(
                "{exchange}/{} 没有 SymbolMeta（元数据加载时被剔除或该合约不存在）\
                 —— 下单会被拒、私有回报会因无法折算张数被丢弃",
                kind.symbol()
            ));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    problems.sort(); // 确定的报错顺序
    Err(format!("策略订阅不可行，拒绝投产: {}", problems.join("; ")))
}


/// 一个 executor 出生时要写入的持仓基线：**作用域内每条 (所, symbol) 都有一条**。
///
/// # 为什么必须每条都有
///
/// 缺一条，策略就能观察到"这条腿的持仓未查询到"这个中间状态 —— 而那个状态对策略毫无
/// 用处：它既不能据此下单（缺的腿要么是模拟账户、要么是该所没凭证，两种都不可交易），
/// 又逼着每个调用点写一次 `Option` 分支。分支写对了是冗余，写错了（当它空仓照常开仓）
/// 是事故。
///
/// 把它填满之后，"未查询到"在策略消费第一条事件之前就已经不存在了 ——
/// 这是品味 1.1 的做法：让错误不可表达，优先于让错误被发现。
///
/// # 三种来源，各自是什么真值
///
/// | 情形 | 基线 | 它是什么的真值 |
/// |---|---|---|
/// | 实盘 + 该所配了凭证 | REST 快照 | 该账户在该所的**真实持仓** |
/// | 实盘 + 该所无凭证 | `0` | **本引擎**在该所的持仓 —— 下不了单也收不到私有流，恒为 0 |
/// | 模拟账户 | `0` | 模拟账户从零起步，不看也不该看真实持仓 |
///
/// 第二行是唯一需要留神的：`0` 说的是"本引擎没在这儿建过仓"，**不是**"这个账户在这儿
/// 没有持仓"（用户手工持有的仓位引擎看不见）。但引擎在该所既不能下单也收不到成交，
/// 这两个量对策略的可行动性没有差别 —— 而这也正是改动前的行为（无记录时
/// `position_size` 返回 `0`），本函数只是把它从"碰巧如此"变成"写明如此"。
///
/// # 补 0 的基线取 `snapshot_req_ts = 0`，以及这里的边界
///
/// 没有快照可言，所以不该过滤任何 Fill（防双计规则见 `SymbolPositions::apply_fill`）。
/// 但那条规则的判据是 `local_ts <= seeded_ts` 且 `Timestamp = u64` ——
/// **`local_ts == 0` 的 Fill 仍会被 ts=0 的基线丢掉**。说"不过滤任何 Fill"是不准确的。
///
/// 今天不可达，三条各自独立：实盘与纸盘的 Fill `local_ts` 取 `now_ms()`，恒远大于 0；
/// 回测不经投产、从不 seed；无凭证的所收不到任何 Fill。
/// 由 `synthesised_baseline_only_swallows_a_zero_timestamp_fill` 钉住这条边界 ——
/// 哪天有路径产生 `local_ts == 0` 的 Fill，那条测试会说明它会发生什么。
fn executor_baselines(
    account: &AccountId,
    scope: &HashSet<(Exchange, Symbol)>,
    rest: &HashMap<(Exchange, Symbol), crate::messaging::PositionBaseline>,
) -> Vec<crate::messaging::PositionBaseline> {
    let flat = |(exchange, symbol): &(Exchange, Symbol)| crate::messaging::PositionBaseline {
        position: crate::domain::Position {
            exchange: *exchange,
            symbol: symbol.clone(),
            size: 0.0,
        },
        snapshot_req_ts: 0,
    };
    scope
        .iter()
        .map(|key| match account {
            AccountId::Live => rest.get(key).cloned().unwrap_or_else(|| flat(key)),
            AccountId::Paper(_) => flat(key),
        })
        .collect()
}

impl Provisioner {
    /// 装配期构造。
    ///
    /// 参数多是因为编排的协作者就是这么多（每个都在字段注释里说明了用途）；
    /// 换成字段公开 + 结构体字面量能省下这个构造函数，但那样 manager 就能随手改写
    /// 编排的实例列表与下单出口 —— 拿边界换构造便利，不值。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
        capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool>,
        income_processor: ActorRef<crate::engine::live::IncomeProcessorActor>,
        metrics: ActorRef<crate::engine::live::MetricsActor>,
        position_ledger: ActorRef<crate::engine::live::PositionLedgerActor>,
        outcome_pubsub: ActorRef<crate::engine::live::OutcomePubSub>,
        order_gateway: Arc<crate::engine::live::OrderGateway>,
        exchange_actors: HashMap<Exchange, Box<dyn super::ExchangeActorOps>>,
    ) -> Self {
        Self {
            accounts,
            capabilities,
            income_processor,
            metrics,
            position_ledger,
            outcome_pubsub,
            order_gateway,
            exchange_actors,
            executors: Vec::new(),
        }
    }

    /// 有序撤下一个 executor：发出 Stop、并**等它真的停下**。
    ///
    /// # 为什么不能用 `kill()`
    ///
    /// `kill()` 产生 [`ActorStopReason::Killed`]，与"意外崩溃"在 reason 上无法区分 —— 而
    /// `ManagerActor::on_link_died` 会把异常终止判为"子 actor 挂了"并整机退出。**有意撤下
    /// 一个策略实例不该打死引擎**（降级正是这么做的）。`stop_gracefully()` 产生 `Normal`，
    /// `on_link_died` 据此放行，两种情形就此可分。
    ///
    /// # 为什么必须等
    ///
    /// `stop_gracefully()` 只是投递 Stop 信号；实例会先排空邮箱才停，期间**仍可能产出订单**。
    /// 不等就往下走（撤单 / 平仓），会出现"我们撤完单，它才把新单发出去"的竞态 —— 那张新单
    /// 又成了无人看管的遗留挂单。
    ///
    /// 返回 `Err(原因)` 供调用方计入"未完成"；超时不改用 `kill()`（那会打死引擎），而是如实
    /// 报出 —— 此时该实例已从 IncomeProcessor 注销，收不到新事件，危害有界。
    async fn stop_executor(
        &mut self,
        executor: &ActorRef<ExecutorActor>,
        producers: &mut ChildRegistrar<'_>,
    ) -> Result<(), String> {
        // 必须是 stop_gracefully 而非 kill：前者产生 `Normal`，`on_link_died` 据此放行；
        // kill 产生 `Killed`，会被判成事故、把整个引擎带走。
        if let Err(e) = executor.stop_gracefully().await {
            return Err(format!("发送 Stop 信号失败: {e}"));
        }
        let stopped = tokio::time::timeout(EXECUTOR_STOP_TIMEOUT, executor.wait_for_shutdown())
            .await
            .map_err(|_| format!("等待实例停止超时（{}s）", EXECUTOR_STOP_TIMEOUT.as_secs()));

        // **确认停下来了**才摘出停机链：撤下的实例不该再攒着强引用（晋升/降级反复
        // 几轮就是泄漏）。但没停下来的必须留着 —— 摘掉等于进程停机时没人再等它，
        // 那它的收尾就会被 runtime drop 砍断，而这正是这套停机链要防的事。
        // 留着的代价有界：它已从 IncomeProcessor 注销、收不到新事件，最坏是让停机
        // 多等一会儿，并由根部的总 deadline 如实报出来。
        if stopped.is_ok() {
            producers.remove(executor);
        }
        stopped
    }

    /// 该所交易所侧**可能有本引擎的残留**（挂单 / 持仓）时，返回清理所需的账户句柄。
    ///
    /// 判据只有一条：**有没有账户**。没有 [`AccountClient`] 就下不了单
    /// （`OrderGateway` 拿不到句柄）、也收不到私有流，因此交易所侧不可能有本引擎的
    /// 任何东西 —— 既无挂单，也无成交、故持仓恒为未 seed 的空。
    ///
    /// # 投产与撤下必须共用这一个判据
    ///
    /// 曾经两处各判各的：投产的撤残单遇到无账户所**跳过**，撤下的清理遇到同样情形
    /// **返回 `Err`**。同一事实两套结论，后果是无凭证配置（`adaptive_trade` 明文支持
    /// 「只跑模拟」）下一旦降级：executor 已停并移出登记，而 supervisor 收到 `Err`
    /// 会保持 `live = Some(..)` —— 记账与事实背离，该 symbol 此后晋升被拒、降级重试
    /// 永远撞同一个 `Err`，**永久卡死**。
    fn exchange_side_cleanup(
        accounts: &HashMap<Exchange, Arc<dyn AccountClient>>,
        exchange: Exchange,
    ) -> Option<&Arc<dyn AccountClient>> {
        accounts.get(&exchange)
    }

    /// 列出**本引擎**在该 (所, symbol) 上的挂单。
    ///
    /// 同一个交易所账户上可能还有人工下单或其他程序下的单，靠 client_order_id 的前缀识别
    /// （见 [`Exchange::owns_cli_order_id`]）。认不出归属的一律**保留** —— 撤错别人的单
    /// 不可接受，而漏撤会由调用方的复查兜住。
    async fn own_pending_orders(
        client: &Arc<dyn AccountClient>,
        exchange: Exchange,
        symbol: &Symbol,
    ) -> Result<Vec<OrderUpdate>, ExchangeError> {
        let all = client.fetch_pending_orders(symbol).await?;
        Ok(all
            .into_iter()
            .filter(|update| match update.client_order_id.as_deref() {
                Some(id) if exchange.owns_cli_order_id(id) => true,
                Some(id) => {
                    tracing::info!(
                        %exchange, %symbol, client_order_id = id,
                        "挂单不属于本引擎，保留不动"
                    );
                    false
                }
                None => {
                    tracing::info!(
                        %exchange, %symbol, order_id = %update.order_id,
                        "挂单无 client_order_id，无法确认归属，保留不动"
                    );
                    false
                }
            })
            .collect())
    }

    /// 撤掉本引擎在该 (所, symbol) 上遗留的挂单，并**复查确认撤净**。
    ///
    /// 单笔撤单失败不直接判死：它可能只是"撤之前那张单刚好成交或已被撤"这类良性竞态。
    /// 是否真的撤净由**复查**裁定 —— 比按各所错误码猜测"是不是 order not found"可靠得多，
    /// 也不需要为此在错误类型上增加变体。
    ///
    /// 复查后仍有遗留 → 返回 `Err` 拒绝启动。留着一张无人看管的挂单开跑，策略会在它旁边
    /// 重复挂单；而启动期失败是**最便宜的失败时机**（还没开始交易）。
    async fn cancel_leftover_orders(
        client: &Arc<dyn AccountClient>,
        exchange: Exchange,
        symbol: &Symbol,
    ) -> Result<(), ExchangeError> {
        let ours = Self::own_pending_orders(client, exchange, symbol).await?;
        if ours.is_empty() {
            return Ok(());
        }

        tracing::warn!(
            %exchange, %symbol, count = ours.len(),
            "[STARTUP] 发现本引擎遗留的挂单，全部撤掉（不接管）"
        );
        for order in &ours {
            if let Err(e) = client.cancel_order(symbol, &order.order_id).await {
                tracing::warn!(
                    %exchange, %symbol, order_id = %order.order_id, error = %e,
                    "撤单请求失败，由复查裁定是否真的还在"
                );
            }
        }

        let remaining = Self::own_pending_orders(client, exchange, symbol).await?;
        if !remaining.is_empty() {
            return Err(ExchangeError::Other(format!(
                "{exchange}/{symbol}: 撤单后复查仍有 {} 张本引擎遗留挂单，拒绝启动\
                 （留着无人看管的挂单开跑，策略会在旁边重复挂单）；order_id: {:?}",
                remaining.len(),
                remaining.iter().map(|o| &o.order_id).collect::<Vec<_>>()
            )));
        }
        Ok(())
    }

    /// 批量添加策略的内部实现
    pub(super) async fn add(
        &mut self,
        strategies: Vec<StrategySpec>,
        symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
        mut producers: ChildRegistrar<'_>,
        manager: &ActorRef<ManagerActor>,
    ) -> Result<(), ExchangeError> {
        if strategies.is_empty() {
            return Ok(());
        }

        let outcome_pubsub = self.outcome_pubsub.clone();

        // 1. 收集所有策略的订阅，并按交易所分组
        let mut all_subscriptions: HashSet<(Exchange, SubscriptionKind)> = HashSet::new();
        let mut strategy_subscriptions: Vec<HashSet<(Exchange, SubscriptionKind)>> = Vec::new();

        for spec in &strategies {
            let public_streams = spec.strategy.public_streams();
            let subscriptions: HashSet<(Exchange, SubscriptionKind)> = public_streams
                .iter()
                .flat_map(|(exchange, kinds)| {
                    kinds.iter().map(move |kind| (*exchange, kind.clone()))
                })
                .collect();
            all_subscriptions.extend(subscriptions.clone());
            strategy_subscriptions.push(subscriptions);
        }

        // 1.5 投产期校验订阅可行性：交易所未配置、或适配层不支持该 kind，都在此刻
        //     拒绝投产 —— 此前 subscribe 对这两种情况静默返回 Ok(())，策略上线后只能
        //     靠"一直没数据"事后发现（最难查的一类故障）。此时尚无任何副作用，Err 即
        //     "什么都没发生"。
        validate_subscriptions(&all_subscriptions, &self.capabilities, symbol_metas)
            .map_err(ExchangeError::Other)?;

        // 2. 按交易所分组订阅
        let mut exchange_subscriptions: HashMap<Exchange, Vec<SubscriptionKind>> = HashMap::new();
        for (exchange, kind) in &all_subscriptions {
            exchange_subscriptions
                .entry(*exchange)
                .or_default()
                .push(kind.clone());
        }

        // 3. 本次涉及的 (exchange, symbol) 对（下面多步共用）
        let exchange_symbols: HashSet<(Exchange, Symbol)> = all_subscriptions
            .iter()
            .map(|(exchange, kind)| (*exchange, kind.symbol().clone()))
            .collect();

        // 4. 撤掉本引擎在这些 symbol 上遗留的挂单（**不接管**）。
        //
        //    为什么撤而不是接管：策略的内存态重启后已经丢了 —— 比如 gamma_scalp 记着"哪张是
        //    当前对冲单"、以及自己的 cancelling 集合。接管一张挂单还得**伪造 tif**（交易所的
        //    订单更新不带这个信息），拿半真半假的字段去做 side_changed / qty_drift 判断。
        //    本项目也没有队列位置模型，撤掉重挂不损失任何东西。
        //
        //    不撤的后果是实打实的：本地 pending 为空 → gamma_scalp 的 maintain() 走 None
        //    分支 → 在旧单旁边**再挂一张**对冲单，而它的契约是"维护单张对冲单"。
        //
        //    **刻意放在创建 executor 之前**：这一步只要 client + symbol，不依赖 executor。
        //    提前的好处是它的失败**根本不需要回滚** —— 此时还没有任何实例被建出来，
        //    直接返回 Err 就等于"什么都没发生"。
        for (exchange, symbol) in &exchange_symbols {
            // 无账户 = 交易所侧不可能有残留，跳过（判据见 exchange_side_cleanup）
            let Some(account) = Self::exchange_side_cleanup(&self.accounts, *exchange) else {
                continue;
            };
            Self::cancel_leftover_orders(account, *exchange, symbol).await?;
        }

        // 5. 拉取持仓基线：**一次快照喂全部账本消费者**（executor / 对账镜像 / 观测镜像
        //    —— 三者同一份快照、同一 seed 语义、同一条防双计规则，口径天然一致）。
        //    此步只读 REST、无副作用，失败即"什么都没发生"。
        let baselines = self.fetch_baselines(&exchange_symbols).await?;

        // 6. 批量创建 ExecutorActors：实盘实例**出生即带基线**（在收到任何流事件之前写入，
        //    "Fill 早于基线、存量被丢"的竞态在结构上不可能）；模拟账户从零起步，不需要
        //    也不应该看到真实持仓。此刻它们还没被注册，收不到任何事件。
        let mut executor_refs = Vec::new();
        for (spec, subscriptions) in strategies.into_iter().zip(strategy_subscriptions.iter()) {
            let account = spec.account.clone();
            let scope: HashSet<(Exchange, Symbol)> = subscriptions
                .iter()
                .map(|(ex, kind)| (*ex, kind.symbol().clone()))
                .collect();
            let exec_baselines = executor_baselines(&account, &scope, &baselines);
            let executor_ref = producers
                .spawn::<ExecutorActor, _>(
                    manager,
                    "ExecutorActor",
                    ExecutorArgs {
                        strategy: spec.strategy,
                        account: spec.account,
                        symbol_metas: Arc::new(symbol_metas.clone()),
                        baselines: exec_baselines,
                        outcome_pubsub: outcome_pubsub.clone(),
                    },
                )
                .await;
            executor_refs.push((executor_ref, account));
        }

        // 7. 注册并投产。**从这里开始产生副作用**，故任一步失败都要回滚已注册的实例 ——
        //    两个调用方（bin 的 main、SupervisorActor::promote）都把 Err 理解成"什么都没
        //    发生"：main 会退出进程（残留无所谓），而 Supervisor 会保持 live=None、以为没
        //    晋升成功。若此时留着一个绑定 AccountId::Live 的实例在跑，它会收私有事件、会真
        //    下单，而 Supervisor 永远不会去 demote 它 —— 无人看管的实盘实例。
        if let Err(e) = self
            .activate_executors(
                &executor_refs,
                &strategy_subscriptions,
                &exchange_symbols,
                baselines.into_values().collect(),
                exchange_subscriptions,
            )
            .await
        {
            self.rollback_executors(&executor_refs, &mut producers).await;
            return Err(e);
        }

        tracing::info!(
            count = executor_refs.len(),
            "Strategies batch added, ExecutorActors created"
        );

        Ok(())
    }

    /// 拉取本批 (所, symbol) 的持仓基线（投产握手的载荷，见
    /// [`crate::messaging::PositionBaseline`]）：交易所返回的用真实数据，未返回的构造
    /// size=0 显式清零（保证消费方一定拿到初始值）。缺 client 的所不产基线（该腿从 0
    /// 起算、不参与对账）。
    ///
    /// 拉取失败即**致命**：持仓基线只有这一次机会，之后全程靠 Fill 累加，没有第二个
    /// 来源能纠正它。跳过的后果是策略从"仓位 0"起算——若账户实际有持仓，三道风控闸门
    /// （单边杠杆/账户杠杆/仓位上限）会全部基于错误基线放行。宁可拒绝启动。
    async fn fetch_baselines(
        &self,
        exchange_symbols: &HashSet<(Exchange, Symbol)>,
    ) -> Result<HashMap<(Exchange, Symbol), crate::messaging::PositionBaseline>, ExchangeError>
    {
        let mut baselines = HashMap::new();
        let exchanges: HashSet<Exchange> = exchange_symbols.iter().map(|(e, _)| *e).collect();
        for exchange in exchanges {
            let Some(client) = self.accounts.get(&exchange) else {
                continue;
            };
            // 快照**请求**时刻：在此之前已由私有流送达的 Fill，其成交必然已包含在快照里
            // —— 消费者 seed 之后据此过滤，避免"快照已含 + 再累加"的双计
            // （见 SymbolState::seed_position）。
            let snapshot_req_ts = now_ms();
            let positions = client.fetch_positions().await.map_err(|e| {
                ExchangeError::Other(format!(
                    "无法获取 {exchange} 的初始持仓基线，拒绝启动（基线只有一次机会，\
                     错了之后无从纠正）: {e}"
                ))
            })?;
            let position_map: HashMap<Symbol, crate::domain::Position> =
                positions.into_iter().map(|p| (p.symbol.clone(), p)).collect();
            for (ex, symbol) in exchange_symbols.iter().filter(|(e, _)| *e == exchange) {
                let pos = position_map.get(symbol).cloned().unwrap_or(crate::domain::Position {
                    exchange: *ex,
                    symbol: symbol.clone(),
                    size: 0.0,
                });
                tracing::info!(%exchange, %symbol, size = pos.size, "Initial position loaded");
                baselines.insert(
                    (*ex, symbol.clone()),
                    crate::messaging::PositionBaseline {
                        position: pos,
                        snapshot_req_ts,
                    },
                );
            }
        }
        Ok(baselines)
    }

    /// 把已创建的 executor 注册进事件流并投产：注册镜像（原子带基线）→ 注册订阅 →
    /// 放行行情。
    ///
    /// # 时序不变量：基线先于任何流事件抵达每个消费者 —— 且是**结构保证**
    ///
    /// - **executor**：出生即带基线（`ExecutorArgs`，见 `do_add_strategies` 第 6 步），
    ///   注册前收不到任何事件；
    /// - **镜像**（对账/观测）：`RegisterSymbols` 一次携带 symbol 与基线，注册与 seed
    ///   原子完成 —— "已注册、基线未到"的窗口不存在，无需缓冲重放。
    ///
    /// # 如实声明的残余窗口（记 t0=快照请求、t1=镜像注册完成、t2=executor 注册完成）
    ///
    /// 都只可能由投产瞬间该 symbol 的**外部/手动成交**触发（此刻没有本引擎实例在交易）：
    ///
    /// - **(t0, t1] 的成交**：镜像与 executor **同错同漏** → 对账检出、受控停机 ——
    ///   这是正杀（事件确实丢了）。对比旧的总线方案：镜像缓冲重放后正确、executor 照漏，
    ///   对账通过 → live 实例带错误持仓**静默**续跑。fail-visible 严格优于 silent-drift。
    /// - **(t1, t2] 的成交**：镜像吃到、executor 错过 → executor 静默漂移且对账不可见。
    ///   窗口 = 几次进程内邮箱投递（旧方案同类窗口为"快照到注册"的数秒）。
    /// - **再晋升**：executor 用新时刻的快照 seed，镜像沿用连续旧账本 —— 新快照请求到
    ///   注册完成之间的外部成交，同样造成 executor 静默漂移且对账不可见（镜像有、它没有），
    ///   与旧方案等宽。没有交易所侧序号无法根除。
    ///
    /// 拆成独立方法只为一件事：让 [`Self::do_add_strategies`] 能在这里失败时统一回滚，
    /// 而不必在每个 `?` 旁边重复一遍回滚代码。
    async fn activate_executors(
        &mut self,
        executor_refs: &[(ActorRef<ExecutorActor>, AccountId)],
        strategy_subscriptions: &[HashSet<(Exchange, SubscriptionKind)>],
        exchange_symbols: &HashSet<(Exchange, Symbol)>,
        baselines: Vec<crate::messaging::PositionBaseline>,
        exchange_subscriptions: HashMap<Exchange, Vec<SubscriptionKind>>,
    ) -> Result<(), ExchangeError> {
        // 7.1 注册观测层与对账层：symbol 范围 + 持仓基线一次送达。
        //     注册失败必须向上传播：对账层注册不上，该 symbol 的 Fill 会被镜像丢弃
        //     （is_tracked 为假），这条腿的对账通道**静默失效** —— 恰是本函数通篇要防的
        //     "错了没人知道"。观测层同理（盈亏基线丢失，会话口径失真）。用 `ask` 而非
        //     `tell`：必须确认注册完成后才放行事件流。
        {
            let symbols: Vec<Symbol> = exchange_symbols
                .iter()
                .map(|(_, symbol)| symbol.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            self.metrics
                .ask(RegisterSymbols {
                    symbols: symbols.clone(),
                    baselines: baselines.clone(),
                })
                .send()
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 MetricsActor 注册 symbol 失败（盈亏基线将丢失）: {e}"
                    ))
                })?;
            self.position_ledger
                .ask(RegisterSymbols { symbols, baselines })
                .send()
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 PositionLedgerActor 注册 symbol 失败（对账通道静默失效，\
                         且这批 symbol 不会出现在对外持仓快照里）: {e}"
                    ))
                })?;
        }

        // 7.2 向 ProcessorActor 注册各 Executor 的订阅（自此它们开始收流事件）
        for ((executor_ref, account), subscriptions) in
            executor_refs.iter().zip(strategy_subscriptions.iter())
        {
            // 注册失败必须**向上传播**，不能只打日志：注册不上的实例收不到任何事件
            // （行情、成交全无），却仍会被 push 进 self.executors 并让本函数返回 Ok ——
            // SupervisorActor 据此认定晋升成功、置 live=Some，此后既不会重试也不会 demote，
            // 留下一个"活着却永远收不到事件"的僵尸实例。与"返回 Ok 即已完整投产"的契约相悖。
            self.income_processor
                .tell(RegisterExecutor {
                    executor: executor_ref.clone(),
                    subscriptions: subscriptions.clone(),
                    account: account.clone(),
                })
                .send()
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 IncomeProcessor 注册策略实例失败（该实例将收不到任何事件）: {e}"
                    ))
                })?;
            self.executors.push(RegisteredExecutor {
                executor: executor_ref.clone(),
                account: account.clone(),
                symbols: subscriptions.iter().map(|(_, k)| k.symbol().clone()).collect(),
            });
        }

        // 7.3 批量向各 ExchangeActors 发送订阅请求（市场数据从此处开始流动）
        for (exchange, kinds) in exchange_subscriptions {
            if let Some(actor) = self.exchange_actors.get(&exchange) {
                actor
                    .subscribe_batch(kinds)
                    .await
                    .map_err(ExchangeError::Other)?;
            }
        }

        Ok(())
    }

    /// 撤下本次刚注册的 executor，使"`AddStrategies` 返回 `Err`"等价于"没有实例在跑"。
    ///
    /// # 回滚的语义边界：只保证不留下无人看管的实例，**不是**恢复原状
    ///
    /// 下列副作用**有意不回滚**，各有理由：
    ///
    /// - **已发出的 `RegisterSymbols`**：对账层比对的是**事件流镜像**，与 executor 无关。
    ///   即便策略没加上，持仓漂移仍然被监控 —— 撤掉反而是削弱安全网。观测层同理，多跟踪
    ///   几个 symbol 只是多几行日志。
    /// - **已推入的持仓基线**：同上，进的是对账层镜像；而被 kill 的 executor 的状态随它消失。
    /// - **已撤掉的遗留挂单**：不可逆，**也不该逆** —— 撤掉本身就是我们要的结果。
    /// - **已发出的行情订阅**：多订几路行情无害（没有订阅者就是丢弃）；而且别的策略实例可能
    ///   订了同一路，贸然退订会把它们的行情一起掐掉。
    ///
    /// 这条边界必须写明：否则后来者会把"回滚不彻底"当成缺陷去补，反而补出问题。
    async fn rollback_executors(
        &mut self,
        created: &[(ActorRef<ExecutorActor>, AccountId)],
        producers: &mut ChildRegistrar<'_>,
    ) {
        let ids: HashSet<ActorId> = created.iter().map(|(r, _)| r.id()).collect();
        for (executor, account) in created {
            if let Err(e) = self
                .income_processor
                .tell(UnregisterExecutor {
                    executor: executor.clone(),
                })
                .send()
                .await
            {
                tracing::error!(error = %e, "回滚时注销 executor 失败");
            }
            if let Err(reason) = self.stop_executor(executor, producers).await {
                tracing::error!(%account, %reason, "回滚时未能确认实例已停止");
            }
            tracing::warn!(%account, "投产失败，已撤下该策略实例");
        }
        // self.executors 里可能已被 6.1 推入，一并摘掉（未推入的 retain 是 no-op）
        self.executors
            .retain(|reg| !ids.contains(&reg.executor.id()));
    }
}

/// 撤下某账户在指定 symbol 上的策略实例，可选立即平仓。
///
/// 用于降级：`live 持续亏损 -> 关闭 live`。两步都必要 ——
/// 只撤实例不平仓会留下无人看管的敞口；只平仓不撤实例会被策略立刻重新开仓。
pub struct RemoveStrategies {
    /// 目标账户（降级即 [`AccountId::Live`]）
    pub account: AccountId,
    /// 目标 symbol
    pub symbols: Vec<Symbol>,
    /// 平仓下到哪个交易所
    pub exchange: Exchange,
    /// 是否立即市价平掉存量仓位
    pub flatten: bool,
}

impl Message<RemoveStrategies> for ManagerActor {
    type Reply = Result<(), ExchangeError>;

    async fn handle(
        &mut self,
        msg: RemoveStrategies,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.provisioner
            .remove(msg, ChildRegistrar::new(&mut self.producers))
            .await
    }
}

impl Provisioner {
    /// 撤下一批实例并清理交易所侧残留，见 [`RemoveStrategies`]。
    pub(super) async fn remove(
        &mut self,
        msg: RemoveStrategies,
        mut producers: ChildRegistrar<'_>,
    ) -> Result<(), ExchangeError> {
        let targets: Vec<Symbol> = msg.symbols;
        // 交易所侧的残留（挂单 / 仓位）只有实盘账户才有：模拟账户的两者都在本地柜台里，
        // 随账户一起留存（供继续记账）
        let is_live = msg.account == AccountId::Live;
        let need_flatten = msg.flatten && is_live;

        // 1. 先撤实例，再平仓。顺序不能反：先平仓的话，策略在收到平仓回报后可能立刻重开。
        //
        //    平仓量取自 executor 的**本地持仓**，且必须在 `kill` 之前问 —— kill 之后它不再
        //    消费事件、状态就停更了。不走 REST：持仓由「基线 + Fill」维护，REST 在本项目里
        //    只作对账（见 PositionLedgerActor）；且本地值比 REST 快照更新（后者可能落后于
        //    最新到达的 Fill）。真出现本地与交易所不一致时，对账层会先致命退出，走不到这里。
        let mut removed = 0usize;
        let mut kept = Vec::new();
        // 按 (所, symbol) 去重：同一 symbol 若有多个实例，它们看到的是同一份交易所持仓，
        // 累加会把平仓量翻倍（reduce-only 虽能兜住，但下单量本身就该是对的）
        let mut to_flatten: HashMap<(Exchange, Symbol), f64> = HashMap::new();
        // 未能完成的部分：**必须向调用方传播**。SupervisorActor 收到 Ok 就会把 live 置为
        // None、认为已干净降级，而实盘可能仍有未平的仓位且此后无人看管 —— 那正是
        // supervisor 自己写下的"谎报已关闭比撤下失败更危险"。
        let mut incomplete: Vec<String> = Vec::new();
        for reg in std::mem::take(&mut self.executors) {
            let hit = reg.account == msg.account
                && targets.iter().any(|s| reg.symbols.contains(s));
            if !hit {
                kept.push(reg);
                continue;
            }
            // **拒绝部分覆盖**：实例覆盖的 symbol 必须完全落在 targets 内。
            //
            // 撤下一个实例就是停掉它管的**所有** symbol，而本次只会为 targets 撤单、平仓。
            // 若实例还管着 targets 之外的 symbol，它们的挂单不撤、仓位不平，却再无实例维护
            // —— 恰好制造出本次改动通篇要消灭的"无人看管的仓位"。而且对账层也发现不了：
            // 基线已建、Fill 不再到达，只要交易所侧不动就永远不告警。
            //
            // 扩大范围去撤/平那些 symbol 同样不对：调用方没要求动它们，静默清仓更意外。
            // 故拒绝并如实报错，让调用方把该实例的 symbol 一次性降完。
            let uncovered: Vec<&Symbol> =
                reg.symbols.iter().filter(|s| !targets.contains(s)).collect();
            if !uncovered.is_empty() {
                tracing::error!(
                    account = %reg.account,
                    ?targets,
                    ?uncovered,
                    "拒绝部分降级：该实例还管着 targets 之外的 symbol，撤下它会让那些 symbol \
                     无人看管；请一次性降完该实例的全部 symbol"
                );
                incomplete.push(format!(
                    "实例覆盖 {uncovered:?} 不在本次 targets 内，拒绝部分降级"
                ));
                kept.push(reg);
                continue;
            }
            if need_flatten {
                match reg
                    .executor
                    .ask(GetPositions(targets.clone()))
                    .send()
                    .await
                {
                    Ok(positions) => {
                        for pos in positions {
                            to_flatten.insert((pos.exchange, pos.symbol), pos.size);
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "取 executor 本地持仓失败，该实例的仓位无法平掉"
                        );
                        incomplete.push(format!("取本地持仓失败: {e}"));
                    }
                }
            }
            if let Err(e) = self
                .income_processor
                .tell(UnregisterExecutor {
                    executor: reg.executor.clone(),
                })
                .send()
                .await
            {
                tracing::error!(error = %e, "Failed to unregister executor");
            }
            // 有序停下并**等它真的停**：后面要撤单 / 平仓，不能让它在排空邮箱时又下新单
            if let Err(reason) = self.stop_executor(&reg.executor, &mut producers).await {
                tracing::error!(
                    account = %reg.account,
                    %reason,
                    "未能确认实例已停止 —— 它可能仍在产出订单"
                );
                incomplete.push(format!("实例未确认停止: {reason}"));
            }
            removed += 1;
        }
        self.executors = kept;
        tracing::info!(
            account = %msg.account,
            symbols = ?targets,
            removed,
            "Removed strategy instances"
        );

        // 模拟账户到此为止：它的挂单与仓位都在本地柜台里，随账户一起留存（供继续记账），
        // 交易所侧没有任何残留需要清理。
        //
        // **但仍要走 incomplete 检查**：撤下阶段本身可能已经失败（拒绝部分降级、实例未确认
        // 停止），在这里直接 `Ok(())` 会把"实例还在跑"谎报成"已干净撤下" —— 正是本分支通篇
        // 要消灭的那类谎报。
        if !is_live {
            return Self::finish_removal(&msg.account, msg.exchange, incomplete);
        }
        // 没有账户的所：交易所侧不可能有本引擎的任何残留 —— 下不了单（`OrderGateway` 拿不到
        // `AccountClient`），也收不到私有流，故既无挂单也无成交、持仓恒为未 seed 的空。
        // 与投产路径的撤残单同一口径（见本文件"没有账户的所不可能有本引擎的挂单"）。
        //
        // **不能返回 `Err`**：此刻 executor 已经停下并移出 `self.executors`，而 supervisor
        // 收到 `Err` 会保持 `live = Some(..)`（"实盘可能仍在运行"）。记账与事实就此背离 ——
        // 此后晋升被 `live.is_some()` 拒绝、降级重试永远撞同一个 `Err`，该 symbol 永久卡死。
        // 无凭证运行是 `adaptive_trade` 明文支持的配置（只跑模拟时的期望状态），可达。
        //
        // 仍走 `finish_removal`：撤下阶段本身可能已经失败（拒绝部分降级、实例未确认停止），
        // 那些必须如实上报，理由同上面的模拟账户分支。
        let Some(client) = Self::exchange_side_cleanup(&self.accounts, msg.exchange).cloned()
        else {
            tracing::info!(
                account = %msg.account,
                exchange = %msg.exchange,
                "该所未配置凭证，交易所侧无残留可清理，撤下就此收尾"
            );
            return Self::finish_removal(&msg.account, msg.exchange, incomplete);
        };

        // 2. 撤掉遗留挂单。**位置是关键**：
        //
        //    - 必须在 `kill` **之后**：实例还活着时撤单，它会收到 Cancelled 回报，
        //      gamma_scalp 那类"看到 pending 为空就补挂"的策略会立刻重新挂上来
        //      （与上面"先撤实例再平仓"是同一个理由）；
        //    - 必须在平仓**之前**：否则 reduce-only 平仓单下去之后，那张遗留的开仓单还可能
        //      成交，又开出一个新仓位。
        //
        //    与 `flatten` 标志无关，只要是实盘账户就撤：不平仓可能是"想留着仓位人工处理"，
        //    但留一张**会自己再开仓**的挂单没有任何合理理由 —— 那正是降级要消除的东西。
        for symbol in &targets {
            if let Err(e) = Self::cancel_leftover_orders(&client, msg.exchange, symbol).await {
                tracing::error!(
                    exchange = %msg.exchange,
                    %symbol,
                    error = %e,
                    "[DEMOTE] 遗留挂单未撤净 —— 它可能成交并重新开出无人看管的仓位"
                );
                incomplete.push(format!("{symbol}: 遗留挂单未撤净 ({e})"));
            }
        }

        // 3. 平仓。`to_flatten` 只在 `need_flatten` 为真时才被填充，故此处不再判一次；
        //    用断言把这条不变量钉住 —— 将来若有人为别的目的（如日志）无条件填充它，
        //    平仓就会在没被要求时触发。
        debug_assert!(
            need_flatten || to_flatten.is_empty(),
            "未要求平仓却收集了待平仓位"
        );
        for ((exchange, symbol), size) in to_flatten {
            let pos = crate::domain::Position {
                exchange,
                symbol,
                size,
                // 平仓只需要方向与数量；均价/浮盈在此无意义，不伪造
            };
            // 只平下到本次指定交易所的那些腿；空仓判据用 domain 的口径，不另写一份比较
            if exchange != msg.exchange || pos.is_empty() {
                continue;
            }
            // 反向 reduce-only 市价单：撮合层强制不反向开仓，量算多了也只会平到 0
            let side = if pos.size > 0.0 { Side::Short } else { Side::Long };
            let order = Order {
                id: String::new(),
                exchange: msg.exchange,
                symbol: pos.symbol.clone(),
                side,
                order_type: OrderType::Market,
                quantity: pos.size.abs(),
                reduce_only: true,
                client_order_id: msg.exchange.new_cli_order_id(),
            };
            tracing::warn!(
                exchange = %msg.exchange,
                symbol = %pos.symbol,
                position = pos.size,
                %side,
                "[DEMOTE] Flattening live position with reduce-only market order"
            );
            // 经**唯一下单出口**下发（缺 meta、残量不足最小可交易量等失败都从这里
            // 如实返回），按结构化结论裁断 —— 见 [`PlaceVerdict`]。
            let symbol = pos.symbol.clone();
            match self.order_gateway.place(order, "demote_flatten").await {
                Ok(PlaceVerdict::Accepted
                | PlaceVerdict::DryRun
                | PlaceVerdict::ReduceOnlyAlreadyClosed) => {}
                // 交易所对平仓单表过态但**内容未知**（可能成交、也可能拒单）。executor
                // 已撤、回报无人消费，此处无从判定 —— 如实计入未完成（假告警可接受，
                // 假成功不可接受：谎报已关闭会让 supervisor 置 live=None，敞口无人看管）。
                // 若实际已成交，supervisor 下个节拍重试 demote 时实例已不在、无仓可平，
                // 第二轮自然收敛为 Ok。
                Ok(PlaceVerdict::ExchangeSpoke) => {
                    tracing::error!(
                        %symbol,
                        position = pos.size,
                        "平仓单已被交易所回报但结论未知（可能已成交也可能被拒），计入未完成待复核"
                    );
                    incomplete.push(format!(
                        "{symbol}: 平仓单结论未知（交易所已回报，REST 失败），需复核"
                    ));
                }
                Err(reason) => {
                    tracing::error!(
                        %symbol,
                        position = pos.size,
                        %reason,
                        "平仓下单失败 —— 实盘仍有敞口"
                    );
                    incomplete.push(format!("{symbol}: 平仓下单失败 ({reason})"));
                }
            }
        }

        Self::finish_removal(&msg.account, msg.exchange, incomplete)
    }

    /// 汇总撤下结果：只要有任何一环没完成，就**向上报错**。
    ///
    /// 调用方（`SupervisorActor::demote`）收到 `Ok` 就会把 `live` 置为 `None`、认为已干净
    /// 降级；若此时实例还在跑、或仓位没平、或遗留挂单没撤掉，那部分敞口就此无人看管 ——
    /// 正是 supervisor 自己写下的"谎报已关闭比撤下失败更危险"。
    fn finish_removal(
        account: &AccountId,
        exchange: Exchange,
        incomplete: Vec<String>,
    ) -> Result<(), ExchangeError> {
        if incomplete.is_empty() {
            return Ok(());
        }
        Err(ExchangeError::Other(format!(
            "撤下未完成（账户 {account}，交易所 {exchange}），需人工核对: {}",
            incomplete.join("; ")
        )))
    }
}


#[cfg(test)]
mod leftover_order_tests {
    use super::*;
    use crate::domain::{AccountInfo, OrderId, OrderStatus, Position};
    use crate::exchange::ExchangeOrder;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    /// 只实现本组测试用到的方法；其余按"不该被调用"处理 —— 若被调用说明被测逻辑越界了，
    /// 让它显式失败比返回一个假值更好。
    /// 已发出的撤单记录，与 client 共享一份，供断言读取
    type CancelLog = Arc<Mutex<Vec<OrderId>>>;

    struct FakeClient {
        /// 每次 `fetch_pending_orders` 依次返回一份；用于模拟"撤单前 / 撤单后复查"两次调用
        pending: Mutex<VecDeque<Vec<OrderUpdate>>>,
        cancelled: CancelLog,
    }

    impl FakeClient {
        fn new(rounds: Vec<Vec<OrderUpdate>>) -> (Arc<dyn AccountClient>, CancelLog) {
            let cancelled: CancelLog = Arc::new(Mutex::new(Vec::new()));
            let client = Arc::new(Self {
                pending: Mutex::new(rounds.into()),
                cancelled: cancelled.clone(),
            });
            (client, cancelled)
        }
    }

    #[async_trait::async_trait]
    impl AccountClient for FakeClient {
        fn exchange(&self) -> Exchange {
            EX
        }
        async fn fetch_pending_orders(
            &self,
            _symbol: &Symbol,
        ) -> Result<Vec<OrderUpdate>, ExchangeError> {
            Ok(self.pending.lock().unwrap().pop_front().unwrap_or_default())
        }
        async fn cancel_order(
            &self,
            _symbol: &Symbol,
            order_id: &OrderId,
        ) -> Result<(), ExchangeError> {
            self.cancelled.lock().unwrap().push(order_id.clone());
            Ok(())
        }
        async fn place_order(&self, _order: ExchangeOrder) -> Result<OrderId, ExchangeError> {
            unreachable!("撤单流程不该下单")
        }
        async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError> {
            unreachable!("撤单流程不该查账户")
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
            unreachable!("撤单流程不该查持仓")
        }
    }

    /// **回归防线（Critical）**：投产与撤下对"无账户所"必须得出同一个结论。
    ///
    /// 曾经两处各判各的 —— 投产跳过、撤下返回 `Err`。无凭证配置下一旦降级，executor 已停
    /// 并移出登记，而 supervisor 收到 `Err` 会保持 `live = Some(..)`：记账与事实背离，
    /// 该 symbol 此后晋升被拒、降级重试永远撞同一个 `Err`，永久卡死。
    ///
    /// 判据收敛到 [`ManagerActor::exchange_side_cleanup`] 之后，两条路径不可能再分叉 ——
    /// 本测试钉住判据本身。
    ///
    /// **未覆盖**：`do_remove_strategies` 的完整路径。`ManagerActor::on_start` 要联网建
    /// client、拉 symbol metas，单测里起不来（同 `stop_semantics_tests` 的取舍）。
    #[test]
    fn provisioning_and_removal_agree_on_an_exchange_without_account() {
        let mut accounts: HashMap<Exchange, Arc<dyn AccountClient>> = HashMap::new();

        // 无账户：两条路径都拿到 None，即"交易所侧无残留可清理"
        assert!(
            Provisioner::exchange_side_cleanup(&accounts, EX).is_none(),
            "没有账户的所必须判为无残留 —— 撤下路径据此干净收尾，而不是报错卡死"
        );

        // 有账户：两条路径都拿到句柄，照常清理
        let (client, _log) = FakeClient::new(vec![]);
        accounts.insert(EX, client);
        assert!(
            Provisioner::exchange_side_cleanup(&accounts, EX).is_some(),
            "有账户的所必须照常清理"
        );
        // 判据是 per-exchange 的：别的所有账户不代表本所有
        assert!(
            Provisioner::exchange_side_cleanup(&accounts, Exchange::Hyperliquid).is_none(),
            "判据必须按所独立"
        );
    }

    fn order(client_order_id: &str, order_id: &str) -> OrderUpdate {
        OrderUpdate {
            order_id: order_id.to_string(),
            client_order_id: Some(client_order_id.to_string()),
            exchange: EX,
            symbol: SYM.to_string(),
            side: Side::Long,
            status: OrderStatus::Pending,
            quantity: 1.0,
        }
    }

    /// 无 client_order_id 的挂单（人工 UI 单 / 交易所系统单）
    fn anonymous_order(order_id: &str) -> OrderUpdate {
        OrderUpdate {
            client_order_id: None,
            ..order("", order_id)
        }
    }

    /// 没有本引擎的挂单时，一个撤单请求都不该发
    #[tokio::test]
    async fn nothing_to_cancel_sends_no_request() {
        let (client, cancelled) = FakeClient::new(vec![vec![]]);
        Provisioner::cancel_leftover_orders(&client, EX, &SYM.to_string())
            .await
            .expect("空列表应直接通过");
        assert!(cancelled.lock().unwrap().is_empty());
    }

    /// **只撤本引擎的单**：人工单 / 其他程序的单 / 无 cloid 的单都不能动。
    /// 撤错别人的挂单是不可接受的。
    #[tokio::test]
    async fn only_own_orders_are_cancelled() {
        let own = EX.new_cli_order_id();
        let foreign = vec![
            order("web_1a2b", "ex-foreign-1"),
            order("android_9f8e", "ex-foreign-2"),
            anonymous_order("ex-anon"),
        ];
        let mut first = foreign.clone();
        first.push(order(&own, "ex-own"));

        // 复查时只剩外部单 —— 本引擎的那张已撤掉
        let (client, cancelled) = FakeClient::new(vec![first, foreign]);
        Provisioner::cancel_leftover_orders(&client, EX, &SYM.to_string())
            .await
            .expect("外部单残留不该导致失败");

        assert_eq!(
            *cancelled.lock().unwrap(),
            vec!["ex-own".to_string()],
            "只应撤掉本引擎的那一张"
        );
    }

    /// 复查仍有本引擎的挂单 -> 拒绝启动/降级。
    ///
    /// 留着无人看管的挂单开跑，策略会在它旁边重复挂单；对降级而言它还可能成交、
    /// 重新开出一个没人管的仓位。
    #[tokio::test]
    async fn still_present_after_cancel_is_an_error() {
        let own = EX.new_cli_order_id();
        let leftover = vec![order(&own, "ex-own")];
        // 两次都返回同一张 —— 模拟撤不掉
        let (client, _cancelled) = FakeClient::new(vec![leftover.clone(), leftover]);

        let err = Provisioner::cancel_leftover_orders(&client, EX, &SYM.to_string())
            .await
            .expect_err("撤不掉必须报错");
        assert!(err.to_string().contains("ex-own"), "错误应指明是哪张单: {err}");
    }
}


#[cfg(test)]
mod subscription_validation_tests {
    use super::*;

    fn bbo(ex: Exchange) -> (Exchange, SubscriptionKind) {
        (ex, SubscriptionKind::BBO { symbol: "BTC".to_string() })
    }
    fn candle(ex: Exchange) -> (Exchange, SubscriptionKind) {
        (
            ex,
            SubscriptionKind::Candle {
                symbol: "BTC".to_string(),
                interval: crate::domain::CandleInterval::Min1,
            },
        )
    }

    /// 构造只含 (Binance, BTC) 的元数据表（订阅校验的第三个判据）
    fn metas_with_btc() -> HashMap<(Exchange, Symbol), SymbolMeta> {
        let meta = SymbolMeta {
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            price_formatter: Arc::new(crate::domain::StepFormatter::new(0.1)),
            size_step: 0.001,
            min_order_size: 0.001,
            contract_size: 1.0,
        };
        [((Exchange::Binance, "BTC".to_string()), meta)]
            .into_iter()
            .collect()
    }

    /// 能力表：只配置了 Binance（能力函数与真实装配同一出处 —— 各适配层的
    /// supports_subscription，随 ExchangeSetup 携带）
    fn binance_only() -> HashMap<Exchange, fn(&SubscriptionKind) -> bool> {
        [(
            Exchange::Binance,
            crate::exchange::binance::supports_subscription as fn(&SubscriptionKind) -> bool,
        )]
        .into_iter()
        .collect()
    }

    /// 未配置的所、不支持的 kind，都在投产期拒绝 —— 不能等上线后"一直没数据"才发现
    #[test]
    fn unavailable_exchange_and_unsupported_kind_are_rejected() {
        let capabilities = binance_only();
        let metas = metas_with_btc();
        // OKX 未配置
        let err = validate_subscriptions(&[bbo(Exchange::OKX)].into_iter().collect(), &capabilities, &metas)
            .expect_err("未配置的所必须拒绝");
        assert!(err.contains("未配置"), "got: {err}");
        // Binance 不支持 Candle
        let err = validate_subscriptions(&[candle(Exchange::Binance)].into_iter().collect(), &capabilities, &metas)
            .expect_err("不支持的 kind 必须拒绝");
        assert!(err.contains("不支持"), "got: {err}");
        // 合法订阅放行
        validate_subscriptions(&[bbo(Exchange::Binance)].into_iter().collect(), &capabilities, &metas)
            .expect("已配置且支持的订阅应放行");
    }

    /// **Critical 回归防线**：订阅了没有 SymbolMeta 的 symbol 必须拒绝投产。
    ///
    /// 各所的元数据加载都会剔除字段异常的合约。被剔掉的 symbol 若正好是策略要交易的，
    /// 故障会以三种不相干的面貌分散出现：下单被 "No SymbolMeta" 拒、私有回报因无法折算
    /// 张数被丢弃（持仓从此落后于交易所）、行情订阅被忽略。根因只有一个，且在此刻可查。
    #[test]
    fn subscription_without_symbol_meta_is_rejected() {
        let eth = (
            Exchange::Binance,
            SubscriptionKind::BBO { symbol: "ETH".to_string() },
        );
        let err = validate_subscriptions(
            &[eth].into_iter().collect(),
            &binance_only(),
            &metas_with_btc(),
        )
        .expect_err("缺 SymbolMeta 的订阅必须在投产期拒绝，而不是上线后三处分别失效");
        assert!(err.contains("SymbolMeta"), "错误应指明根因: {err}");
    }

    /// 能力查询的两个**手写例外**（派生部分直接问各所 kind 映射函数，无需测试同步）：
    /// OKX 的 Candle 由 business WS 承接、IBKR 只实现 BBO —— 这两条不在映射函数里，
    /// 靠本测试钉住（能力函数已下放各适配层，随 ExchangeSetup 携带）。
    #[test]
    fn hand_written_capability_exceptions_hold() {
        let candle_kind = SubscriptionKind::Candle {
            symbol: "BTC".to_string(),
            interval: crate::domain::CandleInterval::Min1,
        };
        assert!(
            crate::exchange::okx::supports_subscription(&candle_kind),
            "OKX 的 Candle 由 business WS 承接，能力查询必须为真"
        );
        assert!(crate::exchange::ibkr::supports_subscription(
            &SubscriptionKind::BBO { symbol: "AAPL".to_string() }
        ));
        assert!(
            !crate::exchange::ibkr::supports_subscription(
                &SubscriptionKind::Trades { symbol: "AAPL".to_string() }
            ),
            "IBKR 只实现了 BBO"
        );
    }

    // ===== 投产基线：作用域内每条腿都要有 =====

    mod executor_baseline_tests {
        use super::*;

        const SYM: &str = "BTC";
        const WITH_ACCOUNT: Exchange = Exchange::Binance;
        const NO_ACCOUNT: Exchange = Exchange::OKX;

        fn scope() -> HashSet<(Exchange, Symbol)> {
            HashSet::from([
                (WITH_ACCOUNT, SYM.to_string()),
                (NO_ACCOUNT, SYM.to_string()),
            ])
        }

        /// 只有 WITH_ACCOUNT 能拉到 REST 快照；NO_ACCOUNT 没配凭证，不产基线
        fn rest() -> HashMap<(Exchange, Symbol), crate::messaging::PositionBaseline> {
            HashMap::from([(
                (WITH_ACCOUNT, SYM.to_string()),
                crate::messaging::PositionBaseline {
                    position: crate::domain::Position {
                        exchange: WITH_ACCOUNT,
                        symbol: SYM.to_string(),
                        size: 7.0,
                    },
                    snapshot_req_ts: 1_000,
                },
            )])
        }

        fn size_at(
            baselines: &[crate::messaging::PositionBaseline],
            exchange: Exchange,
        ) -> Option<f64> {
            baselines
                .iter()
                .find(|b| b.position.exchange == exchange)
                .map(|b| b.position.size)
        }

        /// **核心不变量**：无论账户类型，作用域内每条腿都拿到基线 ——
        /// 于是策略永远观察不到"这条腿的持仓未查询到"。
        #[test]
        fn every_leg_in_scope_gets_a_baseline() {
            for account in [AccountId::Live, AccountId::Paper("x".to_string())] {
                let b = executor_baselines(&account, &scope(), &rest());
                assert_eq!(b.len(), scope().len(), "{account}: 作用域内的腿必须全部有基线");
                assert!(size_at(&b, WITH_ACCOUNT).is_some());
                assert!(size_at(&b, NO_ACCOUNT).is_some());
            }
        }

        /// 实盘用 REST 真值；拉不到的（该所无凭证）补 0
        #[test]
        fn live_takes_the_rest_snapshot_and_zeroes_the_rest() {
            let b = executor_baselines(&AccountId::Live, &scope(), &rest());
            assert_eq!(size_at(&b, WITH_ACCOUNT), Some(7.0), "有凭证的所用 REST 真值");
            assert_eq!(size_at(&b, NO_ACCOUNT), Some(0.0), "无凭证的所补 0");
        }

        /// **模拟账户绝不能看到真实持仓**：即便 REST 快照就在手边，也一律从 0 起步
        #[test]
        fn paper_never_sees_real_positions() {
            let b = executor_baselines(&AccountId::Paper("x".to_string()), &scope(), &rest());
            assert_eq!(size_at(&b, WITH_ACCOUNT), Some(0.0), "模拟账户不得继承实盘存量");
            assert_eq!(size_at(&b, NO_ACCOUNT), Some(0.0));
        }

        /// 补 0 的基线取 `snapshot_req_ts = 0`
        #[test]
        fn synthesised_baselines_use_a_zero_snapshot_timestamp() {
            let b = executor_baselines(&AccountId::Paper("x".to_string()), &scope(), &rest());
            assert!(b.iter().all(|x| x.snapshot_req_ts == 0));
        }

        /// **钉住 ts=0 的真实边界**：它并非"不过滤任何 Fill" —— 防双计判据是
        /// `local_ts <= seeded_ts`，所以 `local_ts == 0` 的 Fill 照样会被吞掉。
        ///
        /// 今天不可达（实盘/纸盘的 `local_ts` 取 `now_ms()`；回测不经投产、从不 seed；
        /// 无凭证的所收不到 Fill），但注释里曾写成"不过滤任何 Fill"，那是不准确的声明。
        /// 这条测试让边界成为可执行的事实：哪天真有路径产生 `local_ts == 0` 的 Fill，
        /// 读到这里就知道会发生什么，而不是被那句话误导。
        #[test]
        fn synthesised_baseline_only_swallows_a_zero_timestamp_fill() {
            use crate::messaging::{AccountData, IncomeEvent, StateManager};

            let baselines = executor_baselines(&AccountId::Paper("x".to_string()), &scope(), &rest());
            let fill_at = |local_ts: u64| {
                IncomeEvent::account(
                    AccountId::Live,
                    local_ts,
                    local_ts,
                    AccountData::Fill(crate::domain::Fill {
                        exchange: WITH_ACCOUNT,
                        symbol: SYM.to_string(),
                        side: crate::domain::Side::Long,
                        price: 100.0,
                        size: 1.0,
                        client_order_id: None,
                        order_id: "1".to_string(),
                        timestamp: local_ts,
                        fee: 0.0,
                        reason: crate::domain::FillReason::Normal,
                    }),
                )
            };

            let mut swallowed = StateManager::new(&[SYM.to_string()], 0);
            swallowed.seed_positions(&baselines);
            swallowed.apply(&fill_at(0));
            assert_eq!(
                swallowed
                    .symbol_state(&SYM.to_string())
                    .expect("state")
                    .position_size(WITH_ACCOUNT),
                0.0,
                "local_ts == 0 的 Fill 会被 ts=0 的基线判成「已含在快照里」而丢弃"
            );

            let mut kept = StateManager::new(&[SYM.to_string()], 0);
            kept.seed_positions(&baselines);
            kept.apply(&fill_at(1));
            assert_eq!(
                kept.symbol_state(&SYM.to_string())
                    .expect("state")
                    .position_size(WITH_ACCOUNT),
                1.0,
                "local_ts >= 1 照常累加 —— 实盘与纸盘取 now_ms()，恒落在这一侧"
            );
        }
    }
}
