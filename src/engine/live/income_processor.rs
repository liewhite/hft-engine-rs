//! IncomeProcessor —— 入向事件分发
//!
//! 订阅 MarketBus 与 AccountBus（全量），按订阅范围分发到对应的 [`Executor`](super::Executor)。
//! "这条事件归不归它"一律问 [`SubscriptionScope::accepts`]（账户归属、所、symbol 都在那里判），
//! 这里只管扇出：
//! - 行情（[`MarketEvent`]）：一份服务所有账户 —— 有 symbol 的经 `(所, symbol)` 索引定向，
//!   其余逐个问范围；
//! - 账户事件（[`AccountEvent`]）：经账户索引只看该账户的 executor，再问范围。
//!
//! # 路由索引
//!
//! 分发是热路径（每条 BBO 都经过），按 (exchange, symbol) 与账户建反向索引做 O(1)
//! 查找 —— 此前对每条事件线性扫描全部 executor，数百 symbol 的部署下每个行情 tick
//! 都是 O(n) 扫描。
//!
//! # 名单由编排维护
//!
//! 投产时 [`IncomeProcessorHandle::register`]，撤下/回滚时**先** [`IncomeProcessorHandle::unregister`]
//! （等它确认）**再**停 executor：两步都是 ask，返回即已生效，停掉的实例不会再被投递。

use super::executor::{ExecutorHandle, ExecutorId};
use crate::domain::{AccountId, Exchange, Symbol};
use crate::exchange::SubscriptionKind;
use crate::messaging::{
    AccountBus, AccountEvent, Delivery, IncomeEvent, MarketBus, MarketEvent, SubscriptionScope,
};
use crate::tasks::{Addr, ChildHandle, Gone, Group, Inbox, Scope, Task};
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;

/// Executor 订阅信息
struct ExecutorSubscription {
    executor: ExecutorHandle,
    /// 订阅范围（含绑定账户）。"这条事件归不归它"一律问 [`SubscriptionScope::accepts`]，
    /// 与回测驱动同一份判据 —— 下面的两个反向索引只是热路径的扇出优化。
    scope: SubscriptionScope,
}

/// IncomeProcessor 的收件箱消息
pub enum IncomeProcessorMsg {
    /// 行情总线（全量订阅）
    Market(MarketEvent),
    /// 账户总线（全量订阅）
    Account(AccountEvent),
    /// 见 [`IncomeProcessorHandle::register`]
    Register {
        executor: ExecutorHandle,
        subscriptions: HashSet<(Exchange, SubscriptionKind)>,
        account: AccountId,
        reply: oneshot::Sender<()>,
    },
    /// 见 [`IncomeProcessorHandle::unregister`]
    Unregister {
        executor: ExecutorId,
        reply: oneshot::Sender<()>,
    },
}

/// IncomeProcessor 的地址（可克隆）
#[derive(Clone, Debug)]
pub struct IncomeProcessorHandle(Addr<IncomeProcessorMsg>);

impl IncomeProcessorHandle {
    /// 注册 executor 及其订阅（该 executor 绑定 `account`）。返回即已生效：此后的事件按范围投给它。
    ///
    /// 幂等：同一 executor 重复注册会先摘净旧索引再按新范围登记。
    pub async fn register(
        &self,
        executor: ExecutorHandle,
        subscriptions: HashSet<(Exchange, SubscriptionKind)>,
        account: AccountId,
    ) -> Result<(), Gone> {
        self.0
            .ask(|reply| IncomeProcessorMsg::Register {
                executor,
                subscriptions,
                account,
                reply,
            })
            .await
    }

    /// 注销 executor（动态撤下策略实例时使用）。返回即已生效：此后不会再有事件投给它，
    /// 调用方可以放心停掉它。未注册是空操作（记 warn）。
    pub async fn unregister(&self, executor: ExecutorId) -> Result<(), Gone> {
        self.0
            .ask(|reply| IncomeProcessorMsg::Unregister { executor, reply })
            .await
    }
}

/// 入向事件分发
pub struct IncomeProcessor {
    inbox: Inbox<IncomeProcessorMsg>,
    executors: HashMap<ExecutorId, ExecutorSubscription>,
    /// 反向索引：(exchange, symbol) -> 订阅者（热路径 O(1) 查找）
    by_symbol: HashMap<(Exchange, Symbol), Vec<ExecutorId>>,
    /// 反向索引：账户 -> 该账户的全部 executor
    by_account: HashMap<AccountId, Vec<ExecutorId>>,
}

impl IncomeProcessor {
    /// 在 `group`（consumers）下起分发器，构造时订阅两条入向总线的全量事件。
    pub async fn spawn(
        group: &Group,
        market_bus: &MarketBus,
        account_bus: &AccountBus,
    ) -> anyhow::Result<(ChildHandle, IncomeProcessorHandle)> {
        let (market_bus, account_bus) = (market_bus.clone(), account_bus.clone());
        let (child, addr) = group
            .spawn_addressed("IncomeProcessor", move |_scope: Scope, mut inbox| async move {
                inbox.subscribe(&market_bus, |_| true, IncomeProcessorMsg::Market);
                inbox.subscribe(&account_bus, |_| true, IncomeProcessorMsg::Account);
                tracing::info!("IncomeProcessor started");
                Ok(Self {
                    inbox,
                    executors: HashMap::new(),
                    by_symbol: HashMap::new(),
                    by_account: HashMap::new(),
                })
            })
            .await?;
        Ok((child, IncomeProcessorHandle(addr)))
    }

    fn handle(&mut self, msg: IncomeProcessorMsg) {
        // 回执发送失败 = 提问方已不再等，没有人需要这份确认
        match msg {
            IncomeProcessorMsg::Market(event) => self.on_market(event),
            IncomeProcessorMsg::Account(event) => self.on_account(event),
            IncomeProcessorMsg::Register {
                executor,
                subscriptions,
                account,
                reply,
            } => {
                self.register(executor, subscriptions, account);
                let _ = reply.send(());
            }
            IncomeProcessorMsg::Unregister { executor, reply } => {
                if self.executors.contains_key(&executor) {
                    self.remove_from_indexes(executor);
                } else {
                    tracing::warn!(%executor, "注销 executor：该 executor 未注册，忽略");
                }
                let _ = reply.send(());
            }
        }
    }

    fn register(
        &mut self,
        executor: ExecutorHandle,
        subscriptions: HashSet<(Exchange, SubscriptionKind)>,
        account: AccountId,
    ) {
        let id = executor.id();
        // 幂等：同一 executor 重复注册时先摘净旧索引 —— 否则 by_symbol/by_account 里
        // 出现重复 id，每条事件双份投递、Fill 双计仓位（危险侧、静默）。今天注册点单一
        // 且 ExecutorId 唯一，但这不该是巧合保护，而是结构保证。
        self.remove_from_indexes(id);

        let scope = SubscriptionScope::new(
            account,
            subscriptions
                .iter()
                .map(|(ex, kind)| (*ex, kind.symbol().clone())),
        );

        tracing::info!(executor = %id, scope = ?scope, "Executor registered");

        for (exchange, symbol) in scope.pairs() {
            self.by_symbol
                .entry((exchange, symbol.clone()))
                .or_default()
                .push(id);
        }
        self.by_account
            .entry(scope.account().clone())
            .or_default()
            .push(id);
        self.executors
            .insert(id, ExecutorSubscription { executor, scope });
    }

    fn remove_from_indexes(&mut self, id: ExecutorId) {
        if let Some(sub) = self.executors.remove(&id) {
            for (exchange, symbol) in sub.scope.pairs() {
                let key = (exchange, symbol.clone());
                if let Some(v) = self.by_symbol.get_mut(&key) {
                    v.retain(|x| *x != id);
                    if v.is_empty() {
                        self.by_symbol.remove(&key);
                    }
                }
            }
            if let Some(v) = self.by_account.get_mut(sub.scope.account()) {
                v.retain(|x| *x != id);
                if v.is_empty() {
                    self.by_account.remove(sub.scope.account());
                }
            }
        }
    }

    /// 行情事件：一份服务所有账户。有 symbol 的定向投给订阅者，无 symbol 的按范围投。
    fn on_market(&mut self, msg: MarketEvent) {
        let event = IncomeEvent::Market(msg);
        let mut gone = Vec::new();
        match event.delivery() {
            Delivery::Symbol(exchange, symbol) => {
                // 定向：索引 O(1)。索引的键由 `SubscriptionScope::pairs()` 灌，与判据同源，
                // 所以这里不再问一遍 accepts —— 但那是**结构约定**，不是编译期保证，
                // 故留一条 debug 断言：索引一旦与范围失配，回测与测试里会立刻响。
                if let Some(ids) = self.by_symbol.get(&(exchange, symbol.clone())) {
                    for id in ids {
                        if let Some(sub) = self.executors.get(id) {
                            debug_assert!(
                                sub.scope.accepts(&event),
                                "by_symbol 索引与订阅范围失配：事件被投给了范围外的 executor"
                            );
                            Self::forward(sub, &event, &mut gone);
                        }
                    }
                }
            }
            // 所级公共读数（ExchangeStatus / ExchangeRate）只投给订了该所的；
            // 与交易所无关的全局事件（Clock / 无 scope 的 Custom）广播。
            // 两档都由同一个判据回答，不在这里重写一遍。
            Delivery::Exchange(_) | Delivery::Broadcast => {
                for sub in self.executors.values() {
                    if sub.scope.accepts(&event) {
                        Self::forward(sub, &event, &mut gone);
                    }
                }
            }
        }
        self.drop_gone(gone);
    }

    /// 账户事件：经账户索引只看**该账户**的 executor，再按订阅范围过滤。
    fn on_account(&mut self, msg: AccountEvent) {
        let account = msg.account.clone();
        let event = IncomeEvent::Account(msg);
        let Some(ids) = self.by_account.get(&account) else {
            return;
        };
        let mut gone = Vec::new();
        for id in ids {
            let Some(sub) = self.executors.get(id) else {
                continue;
            };
            if !sub.scope.accepts(&event) {
                // 该账户的私有事件、但不在这个 executor 的订阅范围内。
                // - Live：分桶部署的常态（账户级全量私有流含其他桶的 symbol），静默跳过；
                // - Paper：柜台只为本进程策略的订单产回报，被过滤即异常 —— 策略对未订阅
                //   symbol 下了单，回报回不来，订单会停在 Created、超时清理后反复重挂。
                //   丢弃点必须可见。
                //
                // **只对币级事件 warn，所级刻意不 warn**：Paper 的所级事件只有柜台按
                // (账户, 所) 发的 AccountInfo，它被丢弃的两种来路都不值得报 —— 多策略共享
                // 同一模拟账户时丢的是别人的所（纯噪声）；策略真在范围外的所下了单时，
                // 那张单的 OrderUpdate 是币级、已经在下面 warn 过了，可行动的信号没丢。
                if account != AccountId::Live {
                    if let Delivery::Symbol(exchange, symbol) = event.delivery() {
                        tracing::warn!(
                            %account,
                            %exchange,
                            %symbol,
                            "模拟账户的私有回报被订阅范围过滤丢弃 —— 策略在未订阅的 symbol 上下了单？"
                        );
                    }
                }
                continue;
            }
            Self::forward(sub, &event, &mut gone);
        }
        self.drop_gone(gone);
    }

    /// 投给一个 executor；对方已停止收件则记进 `gone`（由 [`Self::drop_gone`] 处理）。
    fn forward(sub: &ExecutorSubscription, event: &IncomeEvent, gone: &mut Vec<ExecutorId>) {
        if sub.executor.deliver(event.clone()).is_err() {
            gone.push(sub.executor.id());
        }
    }

    /// 投递失败（`Gone`）= 该 executor 已停止收件，只有三种来路，都不是需要分发层上冒的事故：
    /// - 整机停机：生产者组（executor 与交易所节点）先停，交易所节点收尾时发出的最后几条
    ///   事件到这里时 executor 已关箱 —— 正常；
    /// - executor 自己出错退出：它的错误已经自行上冒、整机在停；
    /// - 编排没先注销就停了它（违反「先注销再停」的约定）：它本就要停，丢的事件没人需要。
    ///
    /// 所以记 warn 并把它摘出名单（之后不会再为它报第二次），分发器照常运行。
    fn drop_gone(&mut self, gone: Vec<ExecutorId>) {
        for id in gone {
            tracing::warn!(executor = %id, "executor 已停止收件，事件未送达，移出分发名单（停机途中属正常）");
            self.remove_from_indexes(id);
        }
    }
}

impl Task for IncomeProcessor {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        while let Some(msg) = self.inbox.recv().await {
            self.handle(msg);
        }
        tracing::info!("IncomeProcessor stopped");
        Ok(())
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::domain::{Exchange, Fill, FillReason, MarketTrade, Side};
    use crate::messaging::{SymbolAccountData, AccountData, MarketData};

    fn fill_event(account: AccountId) -> AccountEvent {
        AccountEvent {
            account,
            exchange_ts: 1,
            local_ts: 1,
            data: AccountData::Symbol(SymbolAccountData::Fill(Fill {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                side: Side::Long,
                price: 100.0,
                size: 1.0,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: 1,
                fee: 0.0,
                reason: FillReason::Normal,
            })),
        }
    }

    fn trade_event() -> MarketEvent {
        MarketEvent {
            exchange_ts: 1,
            local_ts: 1,
            data: MarketData::MarketTrade(MarketTrade {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                price: 100.0,
                qty: 1.0,
                is_buyer_maker: false,
                timestamp: 1,
            }),
        }
    }

    /// 账户隔离的核心：账户事件的归属是**结构字段**，路由按它分道 —— "实盘私有事件
    /// 广播给模拟策略"在类型上已不可表达，这里钉住路由推导本身。
    #[test]
    fn account_event_carries_its_owner_and_routes_by_symbol() {
        let live = fill_event(AccountId::Live);
        assert_eq!(live.account, AccountId::Live);
        let ev = IncomeEvent::Account(live);
        assert_eq!(
            ev.delivery(),
            Delivery::Symbol(Exchange::Binance, &"BTC".to_string()),
            "Fill 有 symbol，应定向路由"
        );
    }

    /// 三档投递范围各取一个代表，钉住分发层与 `accepts` 共用的这份推导。
    ///
    /// 尤其是中间那档：**所级账户读数按所定向，不是广播**。写成广播的那版里，
    /// 策略能读到自己压根没订的所的净值，而杠杆闸门就是拿净值算的。
    #[test]
    fn delivery_scope_matches_the_events_locating_precision() {
        let ev = IncomeEvent::Market(trade_event());
        assert_eq!(
            ev.delivery(),
            Delivery::Symbol(Exchange::Binance, &"BTC".to_string()),
            "有 symbol 的行情按 (所, symbol) 定向"
        );

        let account_info = IncomeEvent::account(
            AccountId::Live,
            0,
            0,
            crate::messaging::AccountData::Exchange(crate::messaging::ExchangeAccountData::AccountInfo {
                exchange: Exchange::Binance,
                equity: 1.0,
                notional: 0.0,
            }),
        );
        assert_eq!(
            account_info.delivery(),
            Delivery::Exchange(Exchange::Binance),
            "所级账户读数按所定向 —— 广播会把净值泄漏给没订这个所的策略"
        );

        let clock = IncomeEvent::market(0, 0, MarketData::Clock);
        assert_eq!(clock.delivery(), Delivery::Broadcast, "Clock 与交易所无关，广播");
    }
}

/// 组件层：注册后按范围投递、注销后不再投递、对方停止收件不打死分发器。用 `tasks::Root` 搭树测。
#[cfg(test)]
mod task_tests {
    use super::*;
    use crate::domain::{Fill, FillReason, MarketTrade, Side};
    use crate::engine::live::executor::ExecutorMsg;
    use crate::messaging::{AccountData, MarketData, SymbolAccountData};
    use crate::tasks::Root;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn btc_bbo() -> HashSet<(Exchange, SubscriptionKind)> {
        HashSet::from([(
            Exchange::Binance,
            SubscriptionKind::BBO {
                symbol: "BTC".to_string(),
            },
        )])
    }

    fn trade(symbol: &str) -> MarketEvent {
        MarketEvent {
            exchange_ts: 1,
            local_ts: 1,
            data: MarketData::MarketTrade(MarketTrade {
                exchange: Exchange::Binance,
                symbol: symbol.to_string(),
                price: 100.0,
                qty: 1.0,
                is_buyer_maker: false,
                timestamp: 1,
            }),
        }
    }

    fn live_fill() -> AccountEvent {
        AccountEvent {
            account: AccountId::Live,
            exchange_ts: 1,
            local_ts: 1,
            data: AccountData::Symbol(SymbolAccountData::Fill(Fill {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                side: Side::Long,
                price: 100.0,
                size: 1.0,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: 1,
                fee: 0.0,
                reason: FillReason::Normal,
            })),
        }
    }

    /// 代替 executor 的收件端：记下收到的事件条数
    async fn fake_executor(root: &Root) -> (ChildHandle, ExecutorHandle, Arc<Mutex<usize>>) {
        let got = Arc::new(Mutex::new(0usize));
        let (addr_tx, addr_rx) = oneshot::channel();
        let sink = got.clone();
        let child = root
            .group("producers")
            .spawn_fn("fake-executor", move |scope| async move {
                let (addr, mut inbox) = scope.inbox::<ExecutorMsg>();
                addr_tx.send(addr).unwrap();
                while let Some(msg) = inbox.recv().await {
                    if let ExecutorMsg::Event(_) = msg {
                        *sink.lock().unwrap() += 1;
                    }
                }
                Ok(())
            })
            .unwrap();
        (child, ExecutorHandle::for_test(addr_rx.await.unwrap()), got)
    }

    /// 让分发器处理完之前进箱的事件（同一收件箱 FIFO：一次 ask 应答 = 前面的都已处理）
    async fn settle(p: &IncomeProcessorHandle) {
        p.unregister(ExecutorId::for_test(u64::MAX)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn routes_by_scope_and_stops_after_unregister() {
        let root = Root::new("t");
        let (market, account) = (MarketBus::new("market"), AccountBus::new("account"));
        let (_c, processor) = IncomeProcessor::spawn(&root.group("consumers"), &market, &account)
            .await
            .unwrap();
        let (_e, executor, got) = fake_executor(&root).await;
        let id = executor.id();
        processor
            .register(executor, btc_bbo(), AccountId::Live)
            .await
            .unwrap();

        market.publish(trade("BTC"));
        market.publish(trade("ETH")); // 范围外
        account.publish(live_fill());
        settle(&processor).await;
        assert_eq!(*got.lock().unwrap(), 2, "BTC 行情与该账户的 BTC 成交各一条，ETH 不投");

        processor.unregister(id).await.unwrap();
        market.publish(trade("BTC"));
        settle(&processor).await;
        assert_eq!(*got.lock().unwrap(), 2, "注销返回后不再投递");
        root.shutdown().await.unwrap();
    }

    /// executor 已停止收件（停机途中的常态）：分发器记 warn、摘掉它，自己照常运行
    #[tokio::test]
    async fn a_gone_executor_never_halts_the_processor() {
        let root = Root::new("t");
        let (market, account) = (MarketBus::new("market"), AccountBus::new("account"));
        let (_c, processor) = IncomeProcessor::spawn(&root.group("consumers"), &market, &account)
            .await
            .unwrap();
        let (child, executor, _got) = fake_executor(&root).await;
        processor
            .register(executor, btc_bbo(), AccountId::Live)
            .await
            .unwrap();
        child.stop().await;

        market.publish(trade("BTC"));
        market.publish(trade("BTC"));
        settle(&processor).await;
        assert!(!root.is_stopping(), "executor 停止收件不是分发层的事故");
        root.shutdown().await.unwrap();
    }
}
