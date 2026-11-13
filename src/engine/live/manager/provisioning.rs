//! 策略投产的编排。
//!
//! - **投产**（[`super::ManagerHandle::add_strategies`]）：校验订阅可行性 → 撤遗留挂单 →
//!   拉持仓基线（一次快照喂 executor / 对账镜像 / 观测镜像三类消费者）→ spawn（出生即带
//!   基线）→ 注册 → 放行行情；任一步失败回滚到"什么都没发生"。
//!
//! 运行期撤下实例（按表现开关实盘的 Supervisor 与它的降级平仓）已在 #761 删除：只服务一个从未
//! 启用的单所入口，且把"撤下即市价平仓"这一策略决定写死在框架里。以后要运行中下架，按多所
//! 形态只做「停实例 + 撤挂单」，平不平交给策略。
//!
//! # 为什么是 [`Provisioner`] 而不是 `impl Manager`
//!
//! 编排必须经 manager 的收件箱串行化，所以它**不是**新组件 —— 请求仍由 `Manager`
//! 收，只是把活委托下来。拆出独立类型是为了让**依赖面成为类型声明**
//! （`docs/architecture.md` P3，与 R2 给持仓账本换成 `PositionBook` 是同一手法）：
//! 账户句柄、能力表、分发层、两个镜像、交易所节点**只有编排用**，它们全在本类型里。
//!
//! # 两件共享的东西
//!
//! - `symbol_metas`：引擎级的合约目录，快照面（`ManagerHandle::all_symbol_metas`）也要读。
//!   两边持的是同一个 `Arc`，不是副本（P1）。
//! - `producers`：新建的 executor 挂在 manager 的**生产者组**下（运行期往已声明的组里
//!   spawn）。组只提供 spawn 能力 —— "何时停、按什么顺序停"由任务树按分组声明顺序决定，
//!   编排改不了。实例退出后即从组里摘除，不需要编排记得去删。
//!
//!   如实说明能力面的边界：`producers` 和 `consumers` 同为 [`Group`]，**传的是哪一组仍由调用点
//!   写对，编译器分不出来**；`Group` 限制的是能对它做什么（只能 spawn、不能 shutdown），不是它是谁。

use super::StrategySpec;
use crate::engine::live::{
    ExecutorArgs, Executor, ExecutorHandle, ExecutorId, IncomeProcessorHandle, MetricsHandle,
    OutcomeBus, PositionLedgerHandle,
};
use crate::domain::{
    AccountId, Exchange, ExchangeError, OrderUpdate, Symbol, SymbolMeta,
};
use crate::engine::live::account_binding::{snapshot_live, Leg};
use crate::engine::live::assembly::StartAccountStream;
use crate::engine::live::AccountBinding;
use crate::exchange::{AccountClient, AccountStream, ExchangeHandle, SubscriptionKind};
use crate::messaging::AccountBus;
use crate::tasks::{ChildHandle, Gone, Group, StopFlag};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// 等待一个 executor 有序停下的上限。
///
/// `Executor` 的处理是纯策略步进 + 往总线发布，都是亚毫秒级工作，正常远不到这个量级。
/// 留 5 秒是为了不因偶发调度抖动就走异常分支。
const EXECUTOR_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// 本次投产刚起好的一个实例（尚未注册）
struct Spawned {
    child: ChildHandle,
    handle: ExecutorHandle,
    account: AccountId,
}

/// [`Provisioner::new`] 的参数：编排的全部协作者（每个的用途见 [`Provisioner`] 的字段注释）
pub(super) struct Collaborators {
    pub manager_stopping: StopFlag,
    pub producers: Group,
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    pub accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    pub capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool>,
    pub account_streams: AccountStreams,
    pub paper_counter: bool,
    pub income_processor: IncomeProcessorHandle,
    pub metrics: MetricsHandle,
    pub position_ledger: PositionLedgerHandle,
    pub outcome_bus: OutcomeBus,
    pub exchanges: HashMap<Exchange, ExchangeHandle>,
}

/// 策略投产的编排器。
///
/// 由 [`super::Manager`] 持有并在处理收件箱消息时调用（因而天然被串行化）；
/// 字段就是它的依赖声明，见模块文档。
pub(super) struct Provisioner {
    /// manager 是否已被请求停止（只读）：协作者失联时分清是停机途中还是事故
    manager_stopping: StopFlag,
    /// manager 的生产者组：新建的 executor 挂在这里
    producers: Group,
    /// 引擎级合约目录（与快照面共享同一个 Arc）
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 各所的**账户私有**能力。键集 = 配了凭证的所；缺席即"该所只接公共行情"。
    accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    /// 各所的订阅能力查询（知识在适配层，随 ExchangeSetup 携带；投产校验用）
    capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool>,
    /// 各所能起的账户类数据流与已起的（策略声明需要才起，见 [`AccountStreams`]）
    account_streams: AccountStreams,
    /// 起没起模拟柜台（`ManagerArgs::paper` 配了才起）：没起时绑定模拟账户的策略拒绝投产 ——
    /// 否则它的单没人撮合，信号石沉大海
    paper_counter: bool,
    /// 事件分发层（注册 / 注销 executor 的订阅）
    income_processor: IncomeProcessorHandle,
    /// 观测镜像（投产时随 symbol 一起收基线）
    metrics: MetricsHandle,
    /// 对账镜像（同上；它是唯一被 REST 持续校验的那份折叠）
    position_ledger: PositionLedgerHandle,
    /// 策略信号总线（新建 executor 的产出去处）
    outcome_bus: OutcomeBus,
    /// 各所交易所节点（放行 / 收回行情订阅）
    exchanges: HashMap<Exchange, ExchangeHandle>,
}

/// 账户类数据流（[`AccountStream`]）的登记与启动：装配处登记各所能起哪几种（只在配了凭证时
/// 登记，见 `ExchangeSetup::account_streams`），投产时按策略声明起，同一所同一种只起一份。
///
/// 起好的流挂在 manager 的生产者组下（与交易所节点同组：它们往账户总线发事件）。投产回滚时
/// 不停已起的流 —— 与已发出的行情订阅同理，多一份读数无害，别的实例可能也在用。
pub(crate) struct AccountStreams {
    starters: HashMap<(Exchange, AccountStream), StartAccountStream>,
    started: HashSet<(Exchange, AccountStream)>,
    account_bus: AccountBus,
}

impl AccountStreams {
    pub(crate) fn new(starters: HashMap<(Exchange, AccountStream), StartAccountStream>, account_bus: AccountBus) -> Self {
        Self { starters, started: HashSet::new(), account_bus }
    }

    /// 投产期校验：每个要的流都得有所登记过（一次列全）
    fn validate(&self, wanted: &HashSet<(Exchange, AccountStream)>) -> Result<(), String> {
        let mut missing: Vec<String> = wanted
            .iter()
            .filter(|key| !self.starters.contains_key(key))
            .map(|(ex, stream)| format!("{ex} 不提供 {stream:?}（适配层没有，或该所没配凭证）"))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        missing.sort();
        Err(format!("策略要的账户数据流不可行，拒绝投产: {}", missing.join("; ")))
    }

    /// 起还没起的那些（已校验过）
    fn start(&mut self, group: &Group, wanted: &HashSet<(Exchange, AccountStream)>) -> Result<(), ExchangeError> {
        for key in wanted {
            if self.started.contains(key) {
                continue;
            }
            let start = self.starters.get(key).expect("投产时已校验");
            start(group, self.account_bus.clone())
                .map_err(|e| ExchangeError::Other(format!("起 {} 的 {:?} 失败: {e}", key.0, key.1)))?;
            tracing::info!(exchange = %key.0, stream = ?key.1, "账户数据流已按策略需要启动");
            self.started.insert(*key);
        }
        Ok(())
    }
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


impl Provisioner {
    /// 装配期构造。协作者打包成 [`Collaborators`] 传入：字段不公开，manager 拿不到也改写
    /// 不了编排的实例列表与下单出口。
    pub(super) fn new(c: Collaborators) -> Self {
        Self {
            manager_stopping: c.manager_stopping,
            producers: c.producers,
            symbol_metas: c.symbol_metas,
            accounts: c.accounts,
            capabilities: c.capabilities,
            account_streams: c.account_streams,
            paper_counter: c.paper_counter,
            income_processor: c.income_processor,
            metrics: c.metrics,
            position_ledger: c.position_ledger,
            outcome_bus: c.outcome_bus,
            exchanges: c.exchanges,
        }
    }

    /// 协作者已停止收件时的处置（一对一请求方，来路与处理见 `docs/architecture.md` 3.1
    /// 「Gone 的来路与处理」）：manager 在停机途中记 warn 当作已完成，否则如实报错。
    fn collaborator_gone(&self, what: &str) -> Result<(), String> {
        if self.manager_stopping.is_stopping() {
            tracing::warn!("{what}：对方已停止收件（引擎正在停机）");
            Ok(())
        } else {
            Err(format!("{what}：对方已停止收件（它已出错退出）"))
        }
    }

    /// 把一个 executor 从事件流上摘下：先移出持仓对账名单，再注销订阅。
    ///
    /// 顺序不能反：注销之后它不再收 Fill，持仓停更，若还在名单里会被误判为分叉、
    /// 打死引擎（停下它最多要等 [`EXECUTOR_STOP_TIMEOUT`]，够对账攒满次数）。持仓账本与
    /// 分发层是两个收件箱，两步都是 ask：确认已移出名单再注销，确认已注销再往下走。
    async fn detach_executor(&self, executor: ExecutorId) -> Result<(), String> {
        if let Err(Gone) = self.position_ledger.unwatch(executor).await {
            self.collaborator_gone(&format!("把 executor {executor} 移出持仓对账名单"))?;
        }
        if let Err(Gone) = self.income_processor.unregister(executor).await {
            self.collaborator_gone(&format!("注销 executor {executor} 的事件订阅"))?;
        }
        Ok(())
    }

    /// 有序停下一个 executor（投产回滚用）：请求它停止、并**等它真的停下**。
    ///
    /// 被要求停止后正常退出只收掉它自己这一支，**不会打死引擎**；
    /// 它自己出错退出才会冒到根、整机停机 —— 两种情形由任务树区分，不靠调用方选对 API。
    ///
    /// # 为什么必须等
    ///
    /// 停止请求发出后，实例会先把收件箱里已有的事件处理完才停，期间**仍可能产出订单**。
    /// 不等就报回"投产失败、没有实例在跑"，会出现调用方以为已干净、它却把新单发出去的竞态。
    ///
    /// 返回 `Err(原因)` 供调用方记录；超时不强杀，而是如实报出 —— 此时该实例已从
    /// IncomeProcessor 注销，收不到新事件，危害有界。它仍挂在生产者组里（实例退出时才从组里
    /// 摘除），进程停机时照样会被等到收尾，最坏是让停机多等一会儿，并由根部的总期限如实报出。
    async fn stop_executor(child: &ChildHandle) -> Result<(), String> {
        tokio::time::timeout(EXECUTOR_STOP_TIMEOUT, child.stop())
            .await
            .map_err(|_| format!("等待实例停止超时（{}s）", EXECUTOR_STOP_TIMEOUT.as_secs()))
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
            // own_pending_orders 只留本引擎的单，必有 client_order_id
            let cid = order.client_order_id.as_deref().expect("本引擎的挂单带 client_order_id");
            if let Err(e) = client.cancel_order(symbol, cid).await {
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
    pub(super) async fn add(&mut self, strategies: Vec<StrategySpec>) -> Result<(), ExchangeError> {
        if strategies.is_empty() {
            return Ok(());
        }

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
        validate_subscriptions(&all_subscriptions, &self.capabilities, &self.symbol_metas)
            .map_err(ExchangeError::Other)?;
        let account_streams: HashSet<(Exchange, AccountStream)> = strategies
            .iter()
            .flat_map(|spec| spec.strategy.account_streams())
            .flat_map(|(ex, streams)| streams.into_iter().map(move |s| (ex, s)))
            .collect();
        self.account_streams.validate(&account_streams).map_err(ExchangeError::Other)?;
        if !self.paper_counter {
            if let Some(spec) = strategies.iter().find(|s| matches!(s.account, AccountId::Paper(_))) {
                return Err(ExchangeError::Other(format!(
                    "策略绑定了模拟账户 {}，但没配模拟柜台（ManagerArgs::paper），它的单没人撮合，拒绝投产",
                    spec.account
                )));
            }
        }

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
            // 无账户 = 下不了单、也收不到私有流，交易所侧不可能有本引擎的挂单，跳过
            let Some(account) = self.accounts.get(exchange) else {
                continue;
            };
            Self::cancel_leftover_orders(account, *exchange, symbol).await?;
        }

        // 5. 拉取持仓基线：**一次快照喂全部账本消费者**（executor / 对账镜像 / 观测镜像
        //    —— 三者同一份快照、同一 seed 语义、同一条防双计规则，口径天然一致）。实盘实例要交易的
        //    所没配凭证在这里报错（见 `snapshot_live`）。此步只读 REST、无副作用，失败即"什么都没发生"。
        let scopes: Vec<HashSet<Leg>> = strategy_subscriptions
            .iter()
            .map(|subs| subs.iter().map(|(ex, kind)| (*ex, kind.symbol().clone())).collect())
            .collect();
        let live_scopes: Vec<&HashSet<Leg>> = strategies
            .iter()
            .zip(&scopes)
            .filter(|(spec, _)| spec.account == AccountId::Live)
            .map(|(_, legs)| legs)
            .collect();
        let (live_baselines, baselines) = snapshot_live(&self.accounts, &live_scopes, &exchange_symbols).await?;

        // 6. 批量创建 Executor：实盘实例**出生即带基线**（在收到任何流事件之前写入，
        //    "Fill 早于基线、存量被丢"的竞态在结构上不可能）；模拟账户从零起步，不需要
        //    也不应该看到真实持仓。此刻它们还没被注册，收不到任何事件。
        let mut spawned: Vec<Spawned> = Vec::new();
        let mut live_baselines = live_baselines.into_iter();
        for (spec, legs) in strategies.into_iter().zip(scopes) {
            let binding = match spec.account {
                AccountId::Live => AccountBinding::Live(live_baselines.next().expect("snapshot_live 按 live_scopes 同序给出")),
                AccountId::Paper(label) => AccountBinding::Paper { label, legs },
            };
            let account = binding.account();
            let created = Executor::spawn(
                &self.producers,
                ExecutorArgs {
                    strategy: spec.strategy,
                    binding,
                    symbol_metas: self.symbol_metas.clone(),
                    outcome_bus: self.outcome_bus.clone(),
                    metrics: self.metrics.clone(),
                },
            )
            .await;
            match created {
                Ok((child, handle)) => spawned.push(Spawned { child, handle, account }),
                Err(e) => {
                    // 起实例失败（构造出错会自己冒到根、整机停机，或生产者组已在停机）：
                    // 已起好的同批实例尚未注册、收不到事件，停掉它们再报错
                    self.rollback_executors(&spawned).await;
                    return Err(ExchangeError::Other(format!("起策略实例失败: {e:#}")));
                }
            }
        }

        // 7. 注册并投产。**从这里开始产生副作用**，故任一步失败都要回滚已注册的实例 ——
        //    调用方把 Err 理解成"什么都没发生"。若此时留着一个绑定 AccountId::Live 的实例在跑，
        //    它会收私有事件、会真下单，却没人知道它存在 —— 无人看管的实盘实例。
        if let Err(e) = self
            .activate_executors(
                &spawned,
                &strategy_subscriptions,
                &exchange_symbols,
                baselines,
                exchange_subscriptions,
                &account_streams,
            )
            .await
        {
            self.rollback_executors(&spawned).await;
            return Err(e);
        }

        tracing::info!(count = spawned.len(), "Strategies batch added, Executors created");

        Ok(())
    }

    /// 把已创建的 executor 注册进事件流并投产：注册镜像（原子带基线）→ 注册订阅 →
    /// 放行行情。
    ///
    /// # 时序不变量：基线先于任何流事件抵达每个消费者 —— 且是**结构保证**
    ///
    /// - **executor**：出生即带基线（`ExecutorArgs`，见 [`Self::add`] 第 6 步），
    ///   注册前收不到任何事件；
    /// - **镜像**（对账/观测）：`register_symbols` 一次携带 symbol 与基线，注册与 seed
    ///   原子完成 —— "已注册、基线未到"的窗口不存在，无需缓冲重放。
    ///
    /// # 如实声明的残余窗口（记 t0=快照请求、t1=镜像注册完成、t2=executor 注册完成）
    ///
    /// 都只可能由投产瞬间该 symbol 的**外部/手动成交**触发（前提：此刻没有本引擎实例在交易
    /// 该 symbol；若动态给已有实盘实例在交易的 symbol 再加实例，老实例在窗口内的成交同样
    /// 落进这些窗口，新实例持仓错、由 #694 的比对检出停机）：
    ///
    /// - **(t0, t1] 的成交**：镜像与 executor **同错同漏** → 对账检出、受控停机 ——
    ///   这是正杀（事件确实丢了）。对比旧的总线方案：镜像缓冲重放后正确、executor 照漏，
    ///   对账通过 → live 实例带错误持仓**静默**续跑。fail-visible 严格优于 silent-drift。
    /// - **(t1, t2] 的成交**：镜像吃到、executor 错过 → executor 与镜像分歧。
    ///   窗口 = 几次进程内收件箱投递（旧方案同类窗口为"快照到注册"的数秒）。
    /// - **同一批 symbol 再次投产**：executor 用新时刻的快照 seed，镜像沿用连续旧账本 —— 新快照请求到
    ///   注册完成之间的外部成交，同样造成 executor 与镜像分歧（镜像有、它没有）。
    ///   没有交易所侧序号无法根除。
    ///
    /// 后两种分歧此前对账不可见（executor 静默带着错的持仓下单）；实盘 executor 纳入对账
    /// 后（#694，见 [`PositionLedgerHandle::watch`]），差值稳定 N 次即受控停机，同样变成
    /// fail-visible。
    ///
    /// 拆成独立方法只为一件事：让 [`Self::add`] 能在这里失败时统一回滚，
    /// 而不必在每个 `?` 旁边重复一遍回滚代码。
    async fn activate_executors(
        &mut self,
        spawned: &[Spawned],
        strategy_subscriptions: &[HashSet<(Exchange, SubscriptionKind)>],
        exchange_symbols: &HashSet<(Exchange, Symbol)>,
        baselines: Vec<crate::messaging::PositionBaseline>,
        exchange_subscriptions: HashMap<Exchange, Vec<SubscriptionKind>>,
        account_streams: &HashSet<(Exchange, AccountStream)>,
    ) -> Result<(), ExchangeError> {
        // 7.1 注册观测层与对账层：symbol 范围 + 持仓基线一次送达。
        //     注册失败必须向上传播：对账层注册不上，该 symbol 的 Fill 会被镜像丢弃
        //     （is_tracked 为假），这条腿的对账通道**静默失效** —— 恰是本函数通篇要防的
        //     "错了没人知道"。观测层同理（盈亏基线丢失，会话口径失真）。两者都是 ask：
        //     必须确认注册完成后才放行事件流。
        {
            let symbols: Vec<Symbol> = exchange_symbols
                .iter()
                .map(|(_, symbol)| symbol.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            self.metrics
                .register_symbols(symbols.clone(), baselines.clone())
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 Metrics 注册 symbol 失败（盈亏基线将丢失）: {e}"
                    ))
                })?;
            self.position_ledger
                .register_symbols(symbols, baselines)
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 PositionLedger 注册 symbol 失败（对账通道静默失效，\
                         且这批 symbol 不会出现在对外持仓快照里）: {e}"
                    ))
                })?;
        }

        // 7.2 向 IncomeProcessor 注册各 Executor 的订阅（自此它们开始收流事件）
        for (s, subscriptions) in spawned.iter().zip(strategy_subscriptions.iter()) {
            // 注册失败必须**向上传播**，不能只打日志：注册不上的实例收不到任何事件
            // （行情、成交全无），本函数却返回 Ok —— 留下一个"活着却永远收不到事件"的僵尸
            // 实例。与"返回 Ok 即已完整投产"的契约相悖。
            self.income_processor
                .register(s.handle.clone(), subscriptions.clone(), s.account.clone())
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 IncomeProcessor 注册策略实例失败（该实例将收不到任何事件）: {e}"
                    ))
                })?;
            // 实盘实例的持仓是下单依据，纳入对账（#694）。注册失败必须传播：否则这份
            // 持仓错了没人知道 —— 与 7.1 同理。模拟账户的持仓与实盘无关，不进名单。
            if s.account == AccountId::Live {
                self.position_ledger
                    .watch(s.handle.clone())
                    .await
                    .map_err(|e| {
                        ExchangeError::Other(format!(
                            "把实盘实例纳入持仓对账失败（它的持仓将无人校验）: {e}"
                        ))
                    })?;
            }
        }

        // 7.3 批量向各交易所节点发送订阅请求（市场数据从此处开始流动）
        for (exchange, kinds) in exchange_subscriptions {
            if let Some(node) = self.exchanges.get(&exchange) {
                node.subscribe_batch(kinds).map_err(|e| {
                    ExchangeError::Other(format!("向 {exchange} 节点发送行情订阅失败: {e}"))
                })?;
            }
        }
        // 7.4 起策略要的账户数据流（同所同种只起一份）
        self.account_streams.start(&self.producers, account_streams)?;

        Ok(())
    }

    /// 撤下本次刚创建的 executor，使"投产返回 `Err`"等价于"没有实例在跑"。
    ///
    /// # 回滚的语义边界：只保证不留下无人看管的实例，**不是**恢复原状
    ///
    /// 下列副作用**有意不回滚**，各有理由：
    ///
    /// - **已发出的 `register_symbols`**：对账层比对的是**事件流镜像**，与 executor 无关。
    ///   即便策略没加上，持仓漂移仍然被监控 —— 撤掉反而是削弱安全网。观测层同理，多跟踪
    ///   几个 symbol 只是多几行日志。
    /// - **已推入的持仓基线**：同上，进的是对账层镜像；而被停掉的 executor 的状态随它消失。
    /// - **已撤掉的遗留挂单**：不可逆，**也不该逆** —— 撤掉本身就是我们要的结果。
    /// - **已发出的行情订阅**：多订几路行情无害（没有订阅者就是丢弃）；而且别的策略实例可能
    ///   订了同一路，贸然退订会把它们的行情一起掐掉。
    ///
    /// 这条边界必须写明：否则后来者会把"回滚不彻底"当成缺陷去补，反而补出问题。
    async fn rollback_executors(&mut self, created: &[Spawned]) {
        for s in created {
            if let Err(reason) = self.detach_executor(s.handle.id()).await {
                tracing::error!(account = %s.account, %reason, "回滚时未能把实例从事件流上摘下");
            }
            if let Err(reason) = Self::stop_executor(&s.child).await {
                tracing::error!(account = %s.account, %reason, "回滚时未能确认实例已停止");
            }
            tracing::warn!(account = %s.account, "投产失败，已撤下该策略实例");
        }
    }
}

#[cfg(test)]
mod leftover_order_tests {
    use rust_decimal_macros::dec;
    use super::*;
    use crate::domain::{AccountInfo, OrderId, OrderStatus, Position, Side};
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
            client_order_id: &str,
        ) -> Result<(), ExchangeError> {
            self.cancelled.lock().unwrap().push(client_order_id.to_string());
            Ok(())
        }
        async fn place_order(&self, _order: ExchangeOrder) -> Result<OrderId, crate::domain::PlaceError> {
            unreachable!("撤单流程不该下单")
        }
        async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError> {
            unreachable!("撤单流程不该查账户")
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
            unreachable!("撤单流程不该查持仓")
        }
    }

    fn order(client_order_id: &str, order_id: &str) -> OrderUpdate {
        OrderUpdate {
            order_id: order_id.to_string(),
            client_order_id: Some(client_order_id.to_string()),
            exchange: EX,
            symbol: SYM.to_string(),
            side: Side::Long,
            status: OrderStatus::Pending,
            quantity: dec!(1.0),
            filled_quantity: dec!(0.0),
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
            vec![own.clone()],
            "只应撤掉本引擎的那一张"
        );
    }

    /// 复查仍有本引擎的挂单 -> 拒绝启动。
    ///
    /// 留着无人看管的挂单开跑，策略会在它旁边重复挂单，它还可能成交、开出一个没人管的仓位。
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
    use rust_decimal_macros::dec;
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
            price_rule: Arc::new(crate::domain::PriceStep::new(dec!(0.1))),
            size_step: dec!(0.001),
            min_order_size: dec!(0.001),
            min_notional: 0.0,
            contract_size: dec!(1.0),
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
}
