//! 策略投产的编排。
//!
//! - **投产**（[`super::Provisioning::add_strategies`]）：校验订阅可行性 →
//!   拉持仓基线（一次快照喂 executor / 对账镜像 / 观测镜像三类消费者）→ 注册镜像 →
//!   spawn（出生即带基线）→ 注册 → 放行行情。
//!
//! **前提：一个进程只在启动时投产一次，投产失败整机停机。** 三个入口（各 bin）都是这样用的；
//! 所以投产失败不回滚（半起的实例随整机停机一起收掉）；第二次投产写不出来：编排器只装在不可克隆的
//! 投产凭证 [`super::Provisioning`] 里，投产时连凭证一起消耗（#826）。
//!
//! 运行期撤下实例（按表现开关实盘的 Supervisor 与它的降级平仓）已在 #761 删除：只服务一个从未
//! 启用的单所入口，且把"撤下即市价平仓"这一策略决定写死在框架里。以后要运行中下架，按多所
//! 形态只做「停实例 + 撤挂单」，平不平交给策略。
//!
//! # 为什么是一张凭证而不是 manager 的方法
//!
//! [`Provisioning`] 不是新组件，不在树上、没有收件箱：投产只此一次，在调用方的任务里直接跑，
//! 没有要串行化的并发请求。它做成独立类型有两层用意：
//! - **只能用一次**：不可克隆、投产方法拿走 `self`，第二次投产编译不过；
//! - **依赖面成为类型声明**（`docs/architecture.md` P3，与 R2 给持仓账本换成 `PositionBook`
//!   是同一手法）：账户句柄、能力表、分发层、两个镜像、交易所节点**只有编排用**，它们全在
//!   本类型里，[`super::ManagerHandle`] 碰不到。
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
use crate::domain::{AccountId, Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::engine::live::account_binding::{snapshot, PlannedAccount};
use crate::engine::live::assembly::StartAccountStream;
use crate::engine::live::{
    Executor, ExecutorArgs, IncomeProcessorHandle, MetricsHandle, OutcomeBus, PositionLedgerHandle,
};
use crate::engine::{subscription_scope, NewClientOrderId};
use crate::exchange::{AccountClient, AccountStream, ExchangeHandle, SubscriptionKind};
use crate::messaging::{AccountBus, SubscriptionScope};
use crate::tasks::{FailHandle, Group};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// [`Provisioning::new`] 的参数：编排的全部协作者（每个的用途见 [`Provisioning`] 的字段注释）
pub(super) struct Collaborators {
    pub fail: FailHandle,
    pub producers: Group,
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    pub accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    pub capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool>,
    pub client_order_ids: Arc<HashMap<Exchange, NewClientOrderId>>,
    pub account_streams: AccountStreams,
    pub paper_counter: bool,
    pub income_processor: IncomeProcessorHandle,
    pub metrics: MetricsHandle,
    pub position_ledger: PositionLedgerHandle,
    pub outcome_bus: OutcomeBus,
    pub exchanges: HashMap<Exchange, ExchangeHandle>,
}

/// 投产凭证：**一个进程只投产一次**，在启动时（#814 定的前提）。
///
/// 由 [`super::Manager::spawn`] 交出唯一一张；不可克隆，投产方法拿走它（`self`），所以第二次
/// 投产的代码编译不过（#826）。字段就是编排的依赖声明，见模块文档。
#[must_use = "不投产，引擎就只空跑行情、没有任何策略"]
pub struct Provisioning {
    /// manager 节点的报错入口：投产失败经它让 manager 出错退出 —— 冒到根、整机停机
    fail: FailHandle,
    /// manager 的生产者组：新建的 executor 挂在这里
    producers: Group,
    /// 引擎级合约目录（与快照面共享同一个 Arc）
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 各所的**账户私有**能力。键集 = 配了凭证的所；缺席即"该所只接公共行情"。
    accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    /// 各所的订阅能力查询（知识在适配层，随 ExchangeSetup 携带；投产校验用）
    capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool>,
    /// 各所的订单号生成函数（知识在适配层，随 ExchangeSetup 携带；新建 executor 时交给它）
    client_order_ids: Arc<HashMap<Exchange, NewClientOrderId>>,
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
/// 起好的流挂在 manager 的生产者组下（与交易所节点同组：它们往账户总线发事件）。
pub(crate) struct AccountStreams {
    starters: HashMap<(Exchange, AccountStream), StartAccountStream>,
    account_bus: AccountBus,
}

impl AccountStreams {
    pub(crate) fn new(
        starters: HashMap<(Exchange, AccountStream), StartAccountStream>,
        account_bus: AccountBus,
    ) -> Self {
        Self {
            starters,
            account_bus,
        }
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
        Err(format!(
            "策略要的账户数据流不可行，拒绝投产: {}",
            missing.join("; ")
        ))
    }

    /// 起要的那些（已校验过；只投产一次，故每种只起一份）
    fn start(
        self,
        group: &Group,
        wanted: &HashSet<(Exchange, AccountStream)>,
    ) -> Result<(), ExchangeError> {
        let mut starters = self.starters;
        for key in wanted {
            let start = starters.remove(key).expect("投产时已校验");
            start(group, self.account_bus.clone()).map_err(|e| {
                ExchangeError::new(format!("起 {} 的 {:?} 失败: {e}", key.0, key.1))
            })?;
            tracing::info!(exchange = %key.0, stream = ?key.1, "账户数据流已按策略需要启动");
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

impl Provisioning {
    /// 装配期构造。协作者打包成 [`Collaborators`] 传入：字段不公开，manager 拿不到也改写
    /// 不了编排的实例列表与下单出口。
    pub(super) fn new(c: Collaborators) -> Self {
        Self {
            fail: c.fail,
            producers: c.producers,
            symbol_metas: c.symbol_metas,
            accounts: c.accounts,
            capabilities: c.capabilities,
            client_order_ids: c.client_order_ids,
            account_streams: c.account_streams,
            paper_counter: c.paper_counter,
            income_processor: c.income_processor,
            metrics: c.metrics,
            position_ledger: c.position_ledger,
            outcome_bus: c.outcome_bus,
            exchanges: c.exchanges,
        }
    }

    /// 投产一个策略实例，见 [`Self::add_strategies`]
    pub async fn add_strategy(self, spec: StrategySpec) -> Result<(), ExchangeError> {
        self.add_strategies(vec![spec]).await
    }

    /// 批量投产：校验订阅 → 拉持仓基线 → 起实例 → 注册 → 放行行情。
    ///
    /// 失败返回 `Err`，同时经 manager 节点整机停机（半起的实例不回滚，随整机收掉）。
    pub async fn add_strategies(self, specs: Vec<StrategySpec>) -> Result<(), ExchangeError> {
        let fail = self.fail.clone();
        let result = self.add(specs).await;
        if let Err(e) = &result {
            fail.fail(anyhow::anyhow!("投产失败: {e}"));
        }
        result
    }

    async fn add(self, strategies: Vec<StrategySpec>) -> Result<(), ExchangeError> {
        // 1. 收集所有策略的订阅；各实例的订阅范围（腿）在这里算一次，同一份交给它的 runner 和分发层
        let mut all_subscriptions: HashSet<(Exchange, SubscriptionKind)> = HashSet::new();
        let mut scopes: Vec<SubscriptionScope> = Vec::new();

        for spec in &strategies {
            let public_streams = spec.strategy.public_streams();
            all_subscriptions.extend(public_streams.iter().flat_map(|(exchange, kinds)| {
                kinds.iter().map(move |kind| (*exchange, kind.clone()))
            }));
            scopes.push(subscription_scope(spec.account.clone(), &public_streams));
        }

        // 1.5 投产期校验订阅可行性：交易所未配置、或适配层不支持该 kind，都在此刻
        //     拒绝投产 —— 否则策略上线后只能靠"一直没数据"事后发现（最难查的一类故障）。
        validate_subscriptions(&all_subscriptions, &self.capabilities, &self.symbol_metas)
            .map_err(ExchangeError::new)?;
        let account_streams: HashSet<(Exchange, AccountStream)> = strategies
            .iter()
            .flat_map(|spec| spec.strategy.account_streams())
            .flat_map(|(ex, streams)| streams.into_iter().map(move |s| (ex, s)))
            .collect();
        self.account_streams
            .validate(&account_streams)
            .map_err(ExchangeError::new)?;
        if !self.paper_counter {
            if let Some(spec) = strategies
                .iter()
                .find(|s| matches!(s.account, AccountId::Paper(_)))
            {
                return Err(ExchangeError::new(format!(
                    "策略绑定了模拟账户 {}，但没配模拟柜台（ManagerArgs::paper），它的单没人撮合，拒绝投产",
                    spec.account
                )));
            }
        }
        //     各实例绑定账户：实盘实例要交易的所没配凭证在此拒绝（见 `PlannedAccount::bind`）
        let plans: Vec<PlannedAccount> = scopes
            .iter()
            .map(|scope| {
                let legs = scope.pairs().map(|(ex, sym)| (ex, sym.clone())).collect();
                PlannedAccount::bind(scope.account().clone(), legs, &self.accounts)
            })
            .collect::<Result<_, _>>()?;

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

        // 4. 拉取持仓基线：**一次快照喂全部账本消费者**（executor / 对账镜像 / 观测镜像
        //    —— 三者同一份快照、同一 seed 语义、同一条防双计规则，口径天然一致）。遗留挂单已在引擎启动时
        //    撤净（见 `super::clearing`），快照之后不会再有它们的成交。
        let (bindings, baselines) = snapshot(plans, &exchange_symbols, &self.accounts).await?;

        // 5. 注册观测层与对账层：symbol 范围 + 持仓基线一次送达（注册与 seed 原子完成，
        //    "已注册、基线未到"的窗口不存在）。注册不上，该 symbol 的 Fill 会被镜像丢弃、
        //    对账通道静默失效，所以失败必须报出；必须确认注册完成后才放行事件流。
        let symbols: Vec<Symbol> = exchange_symbols
            .iter()
            .map(|(_, symbol)| symbol.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        self.metrics
            .register_symbols(symbols.clone(), baselines.clone())
            .await
            .map_err(|e| ExchangeError::new(format!("向 Metrics 注册 symbol 失败: {e}")))?;
        self.position_ledger
            .register_symbols(symbols, baselines)
            .await
            .map_err(|e| ExchangeError::new(format!("向 PositionLedger 注册 symbol 失败: {e}")))?;

        // 6. 起策略要的账户数据流（每种一份）
        self.account_streams
            .start(&self.producers, &account_streams)?;

        // 7. 起实例并注册：实盘实例**出生即带基线**（在收到任何流事件之前写入，"Fill 早于
        //    基线、存量被丢"的竞态在结构上不可能）；模拟账户从零起步。注册到分发层后它才开始
        //    收事件；实盘实例的持仓是下单依据，纳入对账（#694）。
        //    残余窗口：快照之后、注册之前该 symbol 若有不在跟踪表里的成交（强平/ADL 等），
        //    镜像与 executor 会漏掉或分歧，由对账检出、受控停机（fail-visible）。没有交易所侧
        //    序号无法根除。
        let count = bindings.len();
        for ((spec, binding), scope) in strategies.into_iter().zip(bindings).zip(scopes) {
            let account = binding.account();
            let (_, handle) = Executor::spawn(
                &self.producers,
                ExecutorArgs {
                    strategy: spec.strategy,
                    binding,
                    scope: scope.clone(),
                    symbol_metas: self.symbol_metas.clone(),
                    client_order_ids: self.client_order_ids.clone(),
                    outcome_bus: self.outcome_bus.clone(),
                    metrics: self.metrics.clone(),
                },
            )
            .await
            .map_err(|e| ExchangeError::new(format!("起策略实例失败: {e:#}")))?;
            self.income_processor
                .register(handle.clone(), scope)
                .await
                .map_err(|e| {
                    ExchangeError::new(format!("向 IncomeProcessor 注册策略实例失败: {e}"))
                })?;
            if account == AccountId::Live {
                self.position_ledger
                    .watch(handle)
                    .await
                    .map_err(|e| ExchangeError::new(format!("把实盘实例纳入持仓对账失败: {e}")))?;
            }
        }

        // 8. 向各交易所节点发送行情订阅（市场数据从此处开始流动）
        for (exchange, kinds) in exchange_subscriptions {
            if let Some(node) = self.exchanges.get(&exchange) {
                node.subscribe_batch(kinds).map_err(|e| {
                    ExchangeError::new(format!("向 {exchange} 节点发送行情订阅失败: {e}"))
                })?;
            }
        }

        tracing::info!(count, "策略已投产");
        Ok(())
    }
}

#[cfg(test)]
mod subscription_validation_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn bbo(ex: Exchange) -> (Exchange, SubscriptionKind) {
        (
            ex,
            SubscriptionKind::BBO {
                symbol: "BTC".to_string(),
            },
        )
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
            price_rule: Arc::new(crate::domain::PriceStep::new(
                crate::domain::PositiveDecimal::new(dec!(0.1)).unwrap(),
            )),
            size_step: crate::domain::PositiveDecimal::new(dec!(0.001)).unwrap(),
            min_order_size: crate::domain::PositiveDecimal::new(dec!(0.001)).unwrap(),
            min_notional: 0.0,
            contract_size: crate::domain::PositiveDecimal::new(dec!(1.0)).unwrap(),
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
        let err = validate_subscriptions(
            &[bbo(Exchange::OKX)].into_iter().collect(),
            &capabilities,
            &metas,
        )
        .expect_err("未配置的所必须拒绝");
        assert!(err.contains("未配置"), "got: {err}");
        // Binance 不支持 Candle
        let err = validate_subscriptions(
            &[candle(Exchange::Binance)].into_iter().collect(),
            &capabilities,
            &metas,
        )
        .expect_err("不支持的 kind 必须拒绝");
        assert!(err.contains("不支持"), "got: {err}");
        // 合法订阅放行
        validate_subscriptions(
            &[bbo(Exchange::Binance)].into_iter().collect(),
            &capabilities,
            &metas,
        )
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
            SubscriptionKind::BBO {
                symbol: "ETH".to_string(),
            },
        );
        let err = validate_subscriptions(
            &[eth].into_iter().collect(),
            &binance_only(),
            &metas_with_btc(),
        )
        .expect_err("缺 SymbolMeta 的订阅必须在投产期拒绝，而不是上线后三处分别失效");
        assert!(err.contains("SymbolMeta"), "错误应指明根因: {err}");
    }

    /// 能力查询的**手写例外**（派生部分直接问各所 kind 映射函数，无需测试同步）：
    /// OKX 的 Candle 由 business WS 承接 —— 这条不在映射函数里，
    /// 靠本测试钉住（能力函数已下放各适配层，随 ExchangeSetup 携带）。
    #[test]
    fn hand_written_capability_exception_holds() {
        let candle_kind = SubscriptionKind::Candle {
            symbol: "BTC".to_string(),
            interval: crate::domain::CandleInterval::Min1,
        };
        assert!(
            crate::exchange::okx::supports_subscription(&candle_kind),
            "OKX 的 Candle 由 business WS 承接，能力查询必须为真"
        );
    }
}
