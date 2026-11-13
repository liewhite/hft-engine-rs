//! 真 Manager 上的停机分组与构造失败级联。交易所节点用假实现注入（`ExchangeSetup` 的
//! `spawn_actor`），其余组件（分发层、柜台、观测、账本、时钟）都是真的。

use super::*;
use crate::domain::SymbolMeta;
use crate::exchange::{ExchangeCmd, ExchangeHandle};
use crate::messaging::{AccountData, ExchangeAccountData};
use crate::tasks::Root;
use async_trait::async_trait;
use rust_decimal_macros::dec;
use std::sync::Mutex;

type Log = Arc<Mutex<Vec<String>>>;

/// 只供 metas 预加载：只有一个 BTC 合约
struct FakeClient(Exchange);

#[async_trait]
impl ExchangeClient for FakeClient {
    fn exchange(&self) -> Exchange {
        self.0
    }
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        Ok(vec![SymbolMeta {
            exchange: self.0,
            symbol: "BTC".to_string(),
            price_rule: Arc::new(crate::domain::PriceStep::new(dec!(0.1))),
            size_step: dec!(0.001),
            min_order_size: dec!(0.001),
            min_notional: 0.0,
            contract_size: dec!(1.0),
        }])
    }
    async fn fetch_symbol_meta(&self, _: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        Ok(Vec::new())
    }
}

/// 假交易所节点：收尾时往账户总线补发最后一条回报（同 IBKR 补发等 commission 的成交）
struct FakeNode {
    inbox: Inbox<ExchangeCmd>,
    exchange: Exchange,
    account_bus: AccountBus,
    log: Log,
}

impl Task for FakeNode {
    async fn run(&mut self, _: &Scope) -> anyhow::Result<()> {
        while self.inbox.recv().await.is_some() {}
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("{} 节点收尾", self.exchange));
        self.account_bus.publish(final_report(self.exchange));
        Ok(())
    }
}

fn final_report(exchange: Exchange) -> AccountEvent {
    AccountEvent {
        account: AccountId::Live,
        exchange_ts: 1,
        local_ts: 1,
        data: AccountData::Exchange(ExchangeAccountData::AccountInfo {
            exchange,
            equity: 1.0,
            notional: 0.0,
        }),
    }
}

/// `fail` 为 Some 时节点装配失败（构造失败级联用）
fn fake_setup(exchange: Exchange, log: Log, fail: Option<&'static str>) -> ExchangeSetup {
    ExchangeSetup {
        exchange,
        client: Arc::new(FakeClient(exchange)),
        account: None,
        supports: |_| true,
        new_client_order_id: || "fake-cid".to_string(),
        settled_funding: None,
        account_streams: HashMap::new(),
        spawn_actor: Box::new(move |ctx: SpawnCtx| {
            Box::pin(async move {
                if let Some(msg) = fail {
                    return Err(ExchangeError::new(msg.to_string()));
                }
                let (_, addr) = ctx
                    .producers
                    .spawn_addressed(format!("{exchange}Node"), move |_scope, inbox| async move {
                        Ok(FakeNode {
                            inbox,
                            exchange,
                            account_bus: ctx.account_bus,
                            log,
                        })
                    })
                    .await
                    .map_err(|e| ExchangeError::new(format!("{e:#}")))?;
                Ok(ExchangeHandle::new(addr))
            })
        }),
    }
}

/// 外部观察者：收账户总线上的回报，停机时记一笔
struct Observer {
    inbox: Inbox<AccountEvent>,
    log: Log,
}

impl Task for Observer {
    async fn run(&mut self, _: &Scope) -> anyhow::Result<()> {
        while let Some(ev) = self.inbox.recv().await {
            if let AccountData::Exchange(ExchangeAccountData::AccountInfo { exchange, .. }) =
                ev.data
            {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("观察者收到 {exchange} 回报"));
            }
        }
        self.log.lock().unwrap().push("观察者收尾".into());
        Ok(())
    }
}

fn is_account_info(ev: &AccountEvent) -> bool {
    matches!(
        ev.data,
        AccountData::Exchange(ExchangeAccountData::AccountInfo { .. })
    )
}

/// 生产者组先停、消费者组后停：交易所节点收尾时补发的回报，挂在消费者组的观察者收得到
#[tokio::test]
async fn producers_stop_before_consumers_and_their_last_reports_arrive() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let root = Root::new("engine");
    let handle = Manager::spawn(
        &root.group("main"),
        ManagerArgs {
            exchanges: vec![fake_setup(Exchange::Binance, log.clone(), None)],
            paper: None,
        },
    )
    .await
    .expect("Manager 起不来");
    let account_bus = handle.account_bus().clone();
    let observer_log = log.clone();
    handle
        .observers()
        .spawn("observer", move |scope| async move {
            let (_, mut inbox) = scope.inbox::<AccountEvent>();
            inbox.subscribe(&account_bus, is_account_info, |e| e);
            Ok(Observer {
                inbox,
                log: observer_log,
            })
        })
        .await
        .unwrap();

    root.shutdown().await.expect("正常停机");
    assert_eq!(
        *log.lock().unwrap(),
        ["Binance 节点收尾", "观察者收到 Binance 回报", "观察者收尾"]
    );
}

/// 一所节点装配失败：Manager::spawn 返回带根因的 Err，同一错误冒到根；已经起来的
/// 另一所节点与消费者照样被停下收尾
#[tokio::test]
async fn exchange_startup_failure_cascades_to_the_root() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let root = Root::new("engine");
    let result = Manager::spawn(
        &root.group("main"),
        ManagerArgs {
            exchanges: vec![
                fake_setup(Exchange::Binance, log.clone(), None),
                fake_setup(Exchange::OKX, log.clone(), Some("OKX 私有订阅被拒")),
            ],
            paper: None,
        },
    )
    .await;
    let Err(err) = result else {
        panic!("一所起不来，Manager 必须构造失败")
    };
    assert!(format!("{err:#}").contains("OKX 私有订阅被拒"), "{err:#}");

    root.stopping().await;
    let root_err = format!("{:#}", root.shutdown().await.unwrap_err());
    assert!(root_err.contains("OKX 私有订阅被拒"), "{root_err}");
    assert_eq!(
        *log.lock().unwrap(),
        ["Binance 节点收尾"],
        "已起来的节点必须被停下收尾"
    );
}

/// 只声明要哪些账户数据流、别的什么都不做的策略
struct NeedsStreams(HashMap<Exchange, HashSet<crate::exchange::AccountStream>>);

impl Strategy for NeedsStreams {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        HashMap::new()
    }
    fn account_streams(&self) -> HashMap<Exchange, HashSet<crate::exchange::AccountStream>> {
        self.0.clone()
    }
    fn on_event(
        &mut self,
        _: &crate::messaging::IncomeEvent,
        _: crate::strategy::StrategyView<'_>,
    ) -> Vec<crate::strategy::Signal> {
        Vec::new()
    }
}

fn greeks_on(exchange: Exchange) -> Box<dyn Strategy> {
    Box::new(NeedsStreams(HashMap::from([(
        exchange,
        HashSet::from([crate::exchange::AccountStream::Greeks]),
    )])))
}

/// 起一个只有 Binance 与 OKX 的 manager；OKX 登记了 Greeks 流，起一次计一次数
async fn manager_with_okx_greeks(
    root: &Root,
) -> (ManagerHandle, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let started = Arc::new(AtomicUsize::new(0));
    let mut okx = fake_setup(Exchange::OKX, log.clone(), None);
    let counter = started.clone();
    okx.account_streams.insert(
        crate::exchange::AccountStream::Greeks,
        Box::new(move |_: &Group, _: AccountBus| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    );
    let handle = Manager::spawn(
        &root.group("main"),
        ManagerArgs {
            exchanges: vec![fake_setup(Exchange::Binance, log, None), okx],
            paper: None,
        },
    )
    .await
    .expect("Manager 起不来");
    assert_eq!(started.load(Ordering::SeqCst), 0, "没有策略要，就不该起");
    (handle, started)
}

/// 投产失败：调用方拿到原因，同一错误冒到根、整机停机
async fn assert_provisioning_halts(root: Root, err: ManagerError, cause: &str) {
    assert!(err.to_string().contains(cause), "{err}");
    root.stopping().await;
    let root_err = format!("{:#}", root.shutdown().await.unwrap_err());
    assert!(root_err.contains(cause), "{root_err}");
}

/// 账户数据流按策略声明才起（#757）；同一投产里两个实例要同一种流只起一份
#[tokio::test]
async fn account_streams_start_on_demand_once() {
    use std::sync::atomic::Ordering;
    let root = Root::new("engine");
    let (handle, started) = manager_with_okx_greeks(&root).await;
    handle
        .add_strategies(vec![
            StrategySpec::live(greeks_on(Exchange::OKX)),
            StrategySpec::live(greeks_on(Exchange::OKX)),
        ])
        .await
        .unwrap();
    assert_eq!(started.load(Ordering::SeqCst), 1, "同所同种只起一份");
    root.shutdown().await.expect("正常停机");
}

/// 要的账户数据流该所没登记：拒绝投产、整机停机
#[tokio::test]
async fn unavailable_account_stream_halts_provisioning() {
    use std::sync::atomic::Ordering;
    let root = Root::new("engine");
    let (handle, started) = manager_with_okx_greeks(&root).await;
    let err = handle
        .add_strategy(StrategySpec::live(greeks_on(Exchange::Binance)))
        .await
        .unwrap_err();
    assert_eq!(started.load(Ordering::SeqCst), 0);
    assert_provisioning_halts(root, err, "Binance 不提供 Greeks").await;
}

/// 只投产一次：第二次投产报错、整机停机
#[tokio::test]
async fn second_provisioning_halts() {
    let root = Root::new("engine");
    let (handle, _) = manager_with_okx_greeks(&root).await;
    handle
        .add_strategy(StrategySpec::live(greeks_on(Exchange::OKX)))
        .await
        .unwrap();
    let err = handle
        .add_strategy(StrategySpec::live(greeks_on(Exchange::OKX)))
        .await
        .unwrap_err();
    assert_provisioning_halts(root, err, "重复投产").await;
}

/// 只订行情、别的什么都不做的策略
struct WatchesBbo(Exchange);

impl Strategy for WatchesBbo {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        HashMap::from([(
            self.0,
            HashSet::from([SubscriptionKind::BBO {
                symbol: "BTC".to_string(),
            }]),
        )])
    }
    fn on_event(
        &mut self,
        _: &crate::messaging::IncomeEvent,
        _: crate::strategy::StrategyView<'_>,
    ) -> Vec<crate::strategy::Signal> {
        Vec::new()
    }
}

/// 实盘策略要交易的所没配凭证：拒绝投产（下单会被拒、补腿补不上；#767）
#[tokio::test]
async fn live_strategy_is_rejected_on_exchange_without_credentials() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let root = Root::new("engine");
    let handle = Manager::spawn(
        &root.group("main"),
        ManagerArgs {
            exchanges: vec![fake_setup(Exchange::Binance, log.clone(), None)],
            paper: None,
        },
    )
    .await
    .expect("Manager 起不来");
    let err = handle
        .add_strategy(StrategySpec::live(Box::new(WatchesBbo(Exchange::Binance))))
        .await
        .unwrap_err();
    assert_provisioning_halts(root, err, "Binance 没配凭证").await;
}

/// 没配模拟柜台时，绑定模拟账户的策略拒绝投产（它的单没人撮合）
#[tokio::test]
async fn paper_strategy_is_rejected_without_paper_counter() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let root = Root::new("engine");
    let handle = Manager::spawn(
        &root.group("main"),
        ManagerArgs {
            exchanges: vec![fake_setup(Exchange::Binance, log.clone(), None)],
            paper: None,
        },
    )
    .await
    .expect("Manager 起不来");
    let err = handle
        .add_strategy(StrategySpec::paper(
            Box::new(NeedsStreams(HashMap::new())),
            "p",
        ))
        .await
        .unwrap_err();
    assert_provisioning_halts(root, err, "没配模拟柜台").await;
}
