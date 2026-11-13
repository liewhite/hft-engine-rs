//! Manager —— 引擎的组合根：任务树上 `main` 组里唯一的节点，三条总线的建造者。
//!
//! 职责边界（每块各居其位，manager 只做粘合）：
//! - **装配**：收下 bin 侧的 [`ExchangeSetup`]（见 [`super::assembly`]），预加载 metas、
//!   建三条总线、起全部核心子任务 —— 对"有哪些交易所"彻底无知；
//! - **投产编排**：见 [`provisioning`] 子模块（经本节点的收件箱串行化）；
//! - **监督**：由任务树承担 —— 子任务被要求停止后正常退出只收掉它自己那一支（投产回滚、
//!   外部观察者收工），出错或 panic 一律冒到根、整机受控停机，交由外部编排
//!   重启（本项目不重连、不降级运行；唯一例外是公共行情 WS 原地重连，见
//!   `crate::exchange::ws_loop::ReconnectingWs`）；
//! - **停机**：生产者 → 消费者两组依次停（框架按分组的声明顺序），组内并发、组间有序。
//!
//! 对外只经 [`ManagerHandle`]：类型化的投产方法、只读快照、往消费者组挂观察者的入口
//! 和三条总线。

use super::assembly::{ExchangeSetup, SpawnCtx};
use super::{
    AccountBus, AccountOutcome, IncomeProcessor, MarketBus, Metrics, MetricsArgs, OrderGateway, OutcomeBus,
    OutcomeProcessor, PaperCounter, PaperCounterArgs, PositionLedger, PositionLedgerArgs,
    PositionLedgerHandle, Clock, DEFAULT_MAX_CONSECUTIVE_MISMATCHES,
    DEFAULT_POSITION_POLL_INTERVAL_MS, DEFAULT_REPORT_INTERVAL_MS,
};
use crate::domain::{now_ms, AccountId, Exchange, ExchangeError, Position, Symbol, SymbolMeta};
use crate::exchange::{AccountClient, ExchangeClient, ExchangeHandle, SubscriptionKind};
use crate::messaging::{AccountEvent, CustomEvent, MarketData, MarketEvent};
use crate::strategy::Strategy;
use crate::tasks::{Addr, BusTap, Group, Inbox, Scope, Task};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::oneshot;

mod provisioning;
#[cfg(test)]
mod tests;

use provisioning::Provisioner;

/// 时钟节拍（毫秒）：Clock 事件驱动策略的定时逻辑与本地柜台的净值发布
const CLOCK_INTERVAL_MS: u64 = 1000;

/// [`Manager::spawn`] 的参数
pub struct ManagerArgs {
    /// 参与的交易所（在 bin 侧用各所的 `setup_*` 装配，见 [`super::assembly`]）。
    /// manager 对"有哪些所"彻底无知 —— 加新交易所零改 manager。
    pub exchanges: Vec<ExchangeSetup>,
    /// 本地柜台（模拟账户）的撮合与账本配置。
    ///
    /// 实盘与模拟**并行**：两个 outcome 消费者常驻，各按账户取自己的那份
    /// （见 [`crate::domain::AccountId`]）。因此不再有"整个进程的运行模式"这一概念 ——
    /// 一个 symbol 上完全可以模拟先跑、出信号后再拉起实盘，两者互不可见。
    pub paper: crate::sim::SimConfig,
}

/// 策略实例 + 它绑定的账户。
///
/// 同一份策略逻辑可以用不同账户注册两次：一份跑模拟账户、一份跑实盘账户，同时运行。
pub struct StrategySpec {
    pub strategy: Box<dyn Strategy>,
    pub account: AccountId,
}

impl StrategySpec {
    /// 绑定到实盘账户
    pub fn live(strategy: Box<dyn Strategy>) -> Self {
        Self {
            strategy,
            account: AccountId::Live,
        }
    }

    /// 绑定到指定标签的模拟账户（本项目按 symbol 分账）
    pub fn paper(strategy: Box<dyn Strategy>, label: impl Into<String>) -> Self {
        Self {
            strategy,
            account: AccountId::Paper(label.into()),
        }
    }
}

/// Manager 的收件箱消息：只有必须串行化的编排（投产）。只读快照与自定义事件注入
/// 不经收件箱，见 [`ManagerHandle`] 各方法。
enum ManagerMsg {
    AddStrategies(Vec<StrategySpec>, oneshot::Sender<Result<(), ExchangeError>>),
}

/// 经 [`ManagerHandle`] 发起的编排请求的失败
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// manager 已停止收件。manager 是 `main` 组里唯一的节点，它停止收件只可能是整机在停机
    /// （收到停机信号，或某个节点出错冒到了根）—— 请求没有执行，调用方按"引擎在停"处理。
    #[error("引擎正在停机（manager 已停止收件），请求未执行")]
    Stopping,
    /// 编排本身失败（语义见各方法）
    #[error(transparent)]
    Failed(#[from] ExchangeError),
}

/// 引擎的对外句柄（可克隆）。由 [`Manager::spawn`] 在构造完成时交出。
#[derive(Clone)]
pub struct ManagerHandle {
    addr: Addr<ManagerMsg>,
    /// manager 的消费者组：外部观察者挂在这里（见 [`Self::observers`]）
    consumers: Group,
    /// 完整的行情总线：只供 [`Self::publish_custom_event`] 发布；对外只给只订阅视图
    market_bus: MarketBus,
    account_bus: BusTap<AccountEvent>,
    outcome_bus: BusTap<AccountOutcome>,
    /// 引擎级合约目录：构造时预加载一次、此后不变，与编排共享同一份
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    position_ledger: PositionLedgerHandle,
}

impl ManagerHandle {
    async fn call(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), ExchangeError>>) -> ManagerMsg,
    ) -> Result<(), ManagerError> {
        self.addr
            .ask(make)
            .await
            .map_err(|_| ManagerError::Stopping)?
            .map_err(ManagerError::Failed)
    }

    /// 添加一个策略实例，见 [`Self::add_strategies`]
    pub async fn add_strategy(&self, spec: StrategySpec) -> Result<(), ManagerError> {
        self.add_strategies(vec![spec]).await
    }

    /// 批量投产：校验订阅 → 撤遗留挂单 → 拉持仓基线 → 起实例 → 注册 → 放行行情。
    /// 返回 `Ok` 即已完整投产；`Err` 即"没有实例在跑"（见 `provisioning` 模块文档）。
    pub async fn add_strategies(&self, specs: Vec<StrategySpec>) -> Result<(), ManagerError> {
        self.call(|reply| ManagerMsg::AddStrategies(specs, reply)).await
    }

    /// 外部向策略注入自定义事件（[`CustomEvent`] 的**入向**入口）。
    ///
    /// 事件进入行情总线后按 scope 路由：带 `(exchange, symbol)` 的只投递给订阅了
    /// 该 symbol 的策略，无 scope 的广播 —— 与行情同一套分发，无第二条通道。策略在
    /// `on_event` 里 match `MarketData::Custom` 并按类型 `get::<T>()` 消费。
    ///
    /// 自定义事件没有账户归属（如同行情）：实盘与模拟账户的策略都会收到。总线上的
    /// 非 executor 订阅者（观测镜像等）同样可见。时间戳取注入时刻。
    ///
    /// 直接发到行情总线（同步、不会失败），不经 manager 的收件箱。
    pub fn publish_custom_event(&self, event: CustomEvent) {
        let ts = now_ms();
        // 注入留痕：事件"发了没人收"（scope 拼错、订阅缺失、类型不符）时，这是唯一的定位起点
        tracing::debug!(name = event.name, scope = ?event.scope(), "注入自定义事件");
        self.market_bus.publish(MarketEvent {
            exchange_ts: ts,
            local_ts: ts,
            data: MarketData::Custom(event),
        });
    }

    // ------------------------------------------------------------------------
    // 快照面（拉）—— 契约见 `docs/external-data-access.md`：纯内存读、不打 REST。
    // 新增一类快照 = 新增一个方法，不合并成 `query(kind)` 那种按 match 分发的假抽象。
    // ------------------------------------------------------------------------

    /// 所有交易所的 SymbolMeta（启动时预加载的那份，此后不变）
    pub fn all_symbol_metas(&self) -> HashMap<Exchange, Vec<SymbolMeta>> {
        let mut result: HashMap<Exchange, Vec<SymbolMeta>> = HashMap::new();
        for ((exchange, _), meta) in self.symbol_metas.iter() {
            result.entry(*exchange).or_default().push(meta.clone());
        }
        result
    }

    /// 实盘持仓真值快照，取自持仓账本（唯一被 REST 持续校验的那份折叠 —— 外部读到的数字与
    /// 风控守卫看到的数字必须是同一个）。
    ///
    /// 应答为**基线已写入**的全部腿（含 `size == 0` 的空仓腿）；未 seed 的腿不在结果里 ——
    /// 那是"未知"而非"空仓"。
    ///
    /// 账本不可达时返回 `Err` 而**不是空 vec**：同一条"未知不得伪装成空仓"的规矩，对单条腿成立
    /// 就对整份快照成立。空 vec 在下游会被当成"全部已平仓"，而账本不可达时真实持仓可能是任意值。
    pub async fn live_positions(&self) -> Result<Vec<Position>, String> {
        self.position_ledger.live_positions().await.map_err(|e| {
            tracing::error!(error = %e, "持仓账本不可达，快照查询失败（取不到 ≠ 没有持仓）");
            format!("持仓账本不可达: {e}")
        })
    }

    // ------------------------------------------------------------------------
    // 流面（推）：外部观察者
    // ------------------------------------------------------------------------

    /// 外部观察者（bin 里的轮询器、各 bin 的观测组件）挂到这个组下：`observers().spawn(..)`
    /// / `spawn_addressed(..)` / `spawn_periodic(..)`。
    ///
    /// 这是 manager 的**消费者组**：观察者与引擎内部的消费者同一批停，在全部生产者之后 ——
    /// 生产者收尾时补发的最后一批事件观察者照样收得到。挂在树上的观察者出错即整机停机，
    /// 被要求停止（自己的 `ChildHandle::stop`）后正常退出只收掉它自己。
    ///
    /// 要收事件的观察者在自己的构造闭包里 `inbox.subscribe(handle.market_bus(), ..)`：
    /// 收件箱只能从树上的 `Scope` 建，能订阅的一定是有人监督的组件。过滤器是 `fn` 指针，
    /// 只看事件本身，不通过的事件连 clone 都省掉、订阅者也不会被唤醒。
    ///
    /// 挂在这里就得按「出错即整机停机」来写：可容忍的失败（偶发的 REST 超时、某一轮取不到数）
    /// 应由组件在构造与运行内部自己消化（记日志、返回 `Ok`），不要抛 `Err` 指望上层不当回事
    /// —— 任何一层上冒的 `Err` 都会停掉整台引擎。
    pub fn observers(&self) -> &Group {
        &self.consumers
    }

    // 三条总线对外只给只订阅视图：观察者往引擎总线里发事件（伪造行情、成交或策略信号）在
    // 类型上不可表达。外部唯一的入向入口是 [`Self::publish_custom_event`]。

    /// 公共行情总线（含 Clock 与自定义事件），只订阅
    pub fn market_bus(&self) -> &BusTap<MarketEvent> {
        self.market_bus.as_ref()
    }

    /// 账户私有事件总线（实盘 + 全部模拟账户，按事件自带的 `account` 字段区分），只订阅
    pub fn account_bus(&self) -> &BusTap<AccountEvent> {
        &self.account_bus
    }

    /// 出向总线：策略信号（带账户归属），只订阅
    pub fn outcome_bus(&self) -> &BusTap<AccountOutcome> {
        &self.outcome_bus
    }
}

/// Manager 节点：收件箱 + 投产编排器。总线、分组、各子组件的生死都归任务树与句柄，
/// 不在这里存副本。
pub struct Manager {
    inbox: Inbox<ManagerMsg>,
    /// 策略投产的编排器（字段归属即职责声明，见 `provisioning.rs`）
    provisioner: Provisioner,
}

impl Manager {
    /// 在 `group` 下起 manager，等它构造完成（metas 已加载、全部子任务已起、各所私有订阅已
    /// 确认），返回对外句柄。任何一步失败 → 返回 `Err`，同时错误照常冒到根（启动期受控退出，
    /// 不重试、不重连）。
    pub async fn spawn(group: &Group, args: ManagerArgs) -> anyhow::Result<ManagerHandle> {
        let (handle_tx, handle_rx) = oneshot::channel();
        group
            .spawn("Manager", move |scope: Scope| async move {
                let (manager, handle) = Self::build(scope, args).await?;
                // 接收端在本函数里等着，构造成功前不会被丢弃
                let _ = handle_tx.send(handle);
                Ok(manager)
            })
            .await?;
        handle_rx
            .await
            .map_err(|_| anyhow::anyhow!("Manager 构造成功却没有交出句柄（不该发生）"))
    }

    /// 预加载所有交易所的 symbol metas
    async fn preload_all_symbol_metas(
        clients: &HashMap<Exchange, Arc<dyn ExchangeClient>>,
    ) -> Result<HashMap<(Exchange, Symbol), SymbolMeta>, ExchangeError> {
        let mut symbol_metas = HashMap::new();

        for (exchange, client) in clients {
            let metas = client.fetch_all_symbol_metas().await?;
            let count = metas.len();
            for meta in metas {
                symbol_metas.insert((*exchange, meta.symbol.clone()), meta);
            }
            tracing::info!(%exchange, count, "Preloaded symbol metas");
        }

        Ok(symbol_metas)
    }

    /// 提取指定交易所的 symbol metas
    fn get_symbol_metas_for(
        symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
        exchange: Exchange,
    ) -> Arc<HashMap<Symbol, SymbolMeta>> {
        Arc::new(
            symbol_metas
                .iter()
                .filter(|((e, _), _)| *e == exchange)
                .map(|((_, s), m)| (s.clone(), m.clone()))
                .collect(),
        )
    }

    async fn build(scope: Scope, args: ManagerArgs) -> anyhow::Result<(Self, ManagerHandle)> {
        let (addr, inbox) = scope.inbox::<ManagerMsg>();

        // 1. 收下 bin 侧装配好的交易所（阶段 1 的 client 已建好；阶段 2 的节点装配
        //    延迟到 metas 就绪后并发执行）。manager 只做收集与循环，不知道具体是哪些所。
        let setups = args.exchanges;

        //    同所重复是装配错误，必须在此拦下：Vec 让"同一交易所装配两次"成为可表达
        //    状态（旧 Option 字段在类型上排除了它），放行的后果是两套交易所节点向
        //    总线发布**双份**行情与私有事件 —— 最难查的静默双计。
        {
            let mut seen = HashSet::new();
            for setup in &setups {
                if !seen.insert(setup.exchange) {
                    anyhow::bail!(
                        "{} 被装配了两次（bin 侧重复 push 同一交易所的 setup），拒绝启动",
                        setup.exchange
                    );
                }
            }
        }

        //    模拟盘也需要 client：symbol metas 走公共 REST，与是否有凭证无关
        let clients: HashMap<Exchange, Arc<dyn ExchangeClient>> =
            setups.iter().map(|s| (s.exchange, s.client.clone())).collect();
        //    账户私有能力：装配处只在配了凭证时才装进 `setup.account`，故此表的键集
        //    就是"有账户的所"—— 不另存 authed 标记（见 ExchangeSetup::account）
        let accounts: HashMap<Exchange, Arc<dyn AccountClient>> = setups
            .iter()
            .filter_map(|s| s.account.clone().map(|a| (s.exchange, a)))
            .collect();
        // 能力表：投产校验用（知识在各适配层的 supports_subscription，随 setup 携带）
        let capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool> =
            setups.iter().map(|s| (s.exchange, s.supports)).collect();

        // 2. 预加载所有交易所的 symbol metas（引擎级合约目录，此后不变，各组件共享这一份）
        let symbol_metas = Arc::new(Self::preload_all_symbol_metas(&clients).await?);

        // 3. 三条总线：行情 / 账户 / 信号。普通值，克隆给各组件；不属于任何停机分组 ——
        //    订阅跟着订阅者的收件箱走，收件箱关闭即退订（见 `crate::tasks::bus`）。
        //    账户事件（实盘适配层标 Live + 本地柜台标 Paper）走同一条账户总线，
        //    账户隔离由事件自带的 account 字段保证 —— 不靠总线拓扑，也不靠来源推断。
        let market_bus = MarketBus::new("market");
        let account_bus = AccountBus::new("account");
        let outcome_bus = OutcomeBus::new("outcome");

        // 4. 两个停机分组。**声明顺序即停机顺序**：生产者 → 消费者，组内并发、组间有序。
        //
        // 顺序是事件在管线里的流向，反了就会丢掉最后一批事件：
        // - **生产者**（交易所节点、时钟、策略实例）先停 —— 它们收尾时还会往总线发东西
        //   （IBKR 要补发等 commission 的成交），此刻消费者必须还在收件；
        // - **消费者**（两个 Processor、模拟柜台、观测与对账、外部观察者）后停 —— 生产者
        //   补发的都已进了它们的收件箱（总线在发布者自己的任务里同步投递，没有中间队列），
        //   它们关闭收件箱后再把箱里的排空。
        //
        // 组内并发的一处语义收窄：executors 与交易所节点同时收到停止信号，故**不保证
        // executor 能消费到交易所补发的那最后一批回报**。无实害 —— executor 收尾不持久化
        // 任何状态，持仓下次启动走 REST 基线重建。
        //
        // # 如实声明：两段线性模型盖不住一条回灌边
        //
        // 出向总线的两个消费者（本地柜台、`OrderGateway`）会**倒回来**往账户总线发，而它们的
        // 订阅者与它们同在消费者组、同时停：停机瞬间的这批回报会记 ERROR「事件丢失」。各自在
        // 停机时的实际行为（在途下单请求等完才停、丢的只是合成回报）与为什么不再切一刀分组，
        // 只写在 `docs/architecture.md` V5 一处。
        let producers = scope.group("producers");
        let consumers = scope.group("consumers");

        // 5. 分发层：订阅两条入向总线，按订阅关系/账户分发给 executor
        let (_, income_processor) =
            IncomeProcessor::spawn(&consumers, &market_bus, &account_bus).await?;

        // 6. 出向总线的两个消费者**常驻并存**：实盘账户的订单发往交易所，模拟账户的
        //    订单落本地柜台。二者互不知情，各按 AccountOutcome 上的账户取自己的那份 ——
        //    这使得同一 symbol 上模拟与实盘可以并行（模拟先跑，出信号后再拉起实盘）。
        //    分发判据只有 `outlet_for` 一处（两个订阅过滤器 to_live_outlet / to_paper_outlet
        //    都调它），新增账户类型时它编译失败。
        //
        //    OrderGateway 是**唯一**的实盘下单出口，只交给出向处理器（OutcomeProcessor）：
        //    in_flight 判据、失败反馈、dry_run 语义对一切下单一致，系统不存在第二个下单出口。
        let order_gateway = Arc::new(OrderGateway::new(
            accounts.clone(),
            account_bus.clone(),
            // 出向单位折算在此完成（见 ExchangeOrder）；策略与回测全程币本位
            symbol_metas.clone(),
            /* dry_run */ false,
            // 下单结果不确定时停机：经本节点上报错误，整棵树受控停机
            scope.fail_handle(),
            // 整机一开始停机（任何原因）就不再发新单：此时消费者组还在排空信号，
            // 发出去的单在进程退出后无人看管（#710）
            scope.machine_stop_flag(),
        ));
        //    出向处理器还订交易所回报（只订 OrderUpdate），用来作废迟到的 REST 失败结论
        //    （见 OrderGateway::in_flight）
        OutcomeProcessor::spawn(&consumers, order_gateway, &outcome_bus, &account_bus)
            .await?;

        //    本地柜台：订阅信号总线（认领 Paper 账户的订单）与行情总线（撮合 + Clock 发净值），
        //    回报以 Paper 标签发布到账户总线 —— 与实盘适配层同一条总线、同一个类型
        PaperCounter::spawn(
            &consumers,
            PaperCounterArgs {
                config: args.paper,
                symbol_metas: symbol_metas.clone(),
                market_bus: market_bus.clone(),
                outcome_bus: outcome_bus.clone(),
                account_bus: account_bus.clone(),
            },
        )
        .await?;

        // 7. 观测层：订阅两条入向总线（只读事件流，不参与决策）。账户事件（实盘 + 全部模拟
        //    账户）一次订阅，按事件自带的账户分账，绝不与实盘视图混淆
        let metrics = Metrics::spawn(
            &consumers,
            MetricsArgs {
                interval_ms: DEFAULT_REPORT_INTERVAL_MS,
                // PUSHGATEWAY_URL 环境变量启用，未设置 = 只输出日志
                push: crate::observability::MetricsPushConfig::from_env(),
                market_bus: market_bus.clone(),
                account_bus: account_bus.clone(),
            },
        )
        .await?;

        // 8. 持仓对账（通道 B）：REST 读数 vs 本地「基线 + Fill」。
        //    与 Metrics 分开是刻意的——观测层故障不该拖垮交易，而对账确认漂移必须
        //    致命退出（本地持仓不可信时，策略的三道风控闸门全部失效）。
        //
        //    它同时是快照面 `live_positions` 的数据源 —— 它是唯一被持续校验的那份折叠，
        //    外部读到的数字与风控守卫看到的数字必须是同一个。
        //
        //    读数由它**自行轮询**：读数只有这一个消费者，不上总线。收 `accounts` 而非
        //    全部 client —— "没有账户的所不必轮询"由类型表达，无需过滤。镜像的增量输入
        //    只订实盘 Fill（模拟成交绝不进实盘镜像），订阅在它自己构造时完成。
        let (_, position_ledger) = PositionLedger::spawn(
            &consumers,
            PositionLedgerArgs {
                accounts: accounts.clone(),
                symbol_metas: symbol_metas.clone(),
                account_bus: account_bus.clone(),
                interval_ms: DEFAULT_POSITION_POLL_INTERVAL_MS,
                max_consecutive_mismatches: DEFAULT_MAX_CONSECUTIVE_MISMATCHES,
            },
        )
        .await?;

        // 9. 时钟（发布 Clock 事件到行情总线）
        Clock::spawn(&producers, CLOCK_INTERVAL_MS, market_bus.clone())?;

        // 10. 并发装配各所的交易所节点：各 setup 的 future 里等节点构造完成（耗时的 IO 部分，
        //     含私有订阅确认），join_all 让各所并发。任一失败 -> 向上传播（启动期受控退出，
        //     不重试/不重连）。
        let spawn_futures: Vec<_> = setups
            .into_iter()
            .map(|setup| {
                let ctx = SpawnCtx {
                    producers: producers.clone(),
                    market_bus: market_bus.clone(),
                    account_bus: account_bus.clone(),
                    symbol_metas: Self::get_symbol_metas_for(&symbol_metas, setup.exchange),
                };
                let exchange = setup.exchange;
                async move { (setup.spawn_actor)(ctx).await.map(|handle| (exchange, handle)) }
            })
            .collect();
        let mut exchanges: HashMap<Exchange, ExchangeHandle> = HashMap::new();
        for result in futures_util::future::join_all(spawn_futures).await {
            let (exchange, handle) = result?;
            tracing::info!(%exchange, "交易所节点就绪");
            exchanges.insert(exchange, handle);
        }

        tracing::info!("Manager started with all child tasks");

        let handle = ManagerHandle {
            addr,
            consumers,
            market_bus,
            account_bus: account_bus.tap(),
            outcome_bus: outcome_bus.tap(),
            symbol_metas: symbol_metas.clone(),
            position_ledger: position_ledger.clone(),
        };
        let manager = Self {
            inbox,
            provisioner: Provisioner::new(provisioning::Collaborators {
                manager_stopping: scope.stop_flag(),
                producers,
                symbol_metas,
                accounts,
                capabilities,
                income_processor,
                metrics,
                position_ledger,
                outcome_bus,
                exchanges,
            }),
        };
        Ok((manager, handle))
    }
}

impl Task for Manager {
    /// 逐条处理编排请求（收件箱即串行化：同一时刻只有一个投产在进行）。
    /// 被要求停止后交完箱里已有的请求再返回；子任务由框架按分组顺序收尾。
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                ManagerMsg::AddStrategies(specs, reply) => {
                    let result = self.provisioner.add(specs).await;
                    reply_to(reply, result, "投产");
                }
            }
        }
        tracing::info!("Manager inbox drained, stopping child tasks");
        Ok(())
    }
}

/// 交回编排结果。调用方已不在（丢弃了等待）时结果无人读：失败的那份记 ERROR，不静默丢。
fn reply_to(
    reply: oneshot::Sender<Result<(), ExchangeError>>,
    result: Result<(), ExchangeError>,
    what: &str,
) {
    if let Err(Err(e)) = reply.send(result) {
        tracing::error!(error = %e, "{what}失败，但调用方已不在等待结果");
    }
}
