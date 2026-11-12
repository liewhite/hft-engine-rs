//! 真 Manager 上的停机分组与构造失败级联。交易所节点用假实现注入（`ExchangeSetup` 的
//! `spawn_actor`），其余组件（分发层、柜台、观测、账本、时钟）都是真的。

use super::*;
use crate::domain::SymbolMeta;
use crate::exchange::{ExchangeCmd, ExchangeHandle};
use crate::messaging::{AccountData, ExchangeAccountData};
use crate::tasks::Root;
use async_trait::async_trait;
use std::sync::Mutex;

type Log = Arc<Mutex<Vec<String>>>;

/// 只供 metas 预加载：一个合约都没有
struct FakeClient(Exchange);

#[async_trait]
impl ExchangeClient for FakeClient {
    fn exchange(&self) -> Exchange {
        self.0
    }
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        Ok(Vec::new())
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
        self.log.lock().unwrap().push(format!("{} 节点收尾", self.exchange));
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
        spawn_actor: Box::new(move |ctx: SpawnCtx| {
            Box::pin(async move {
                if let Some(msg) = fail {
                    return Err(ExchangeError::Other(msg.to_string()));
                }
                let (_, addr) = ctx
                    .producers
                    .spawn_addressed(format!("{exchange}Node"), move |_scope, inbox| async move {
                        Ok(FakeNode { inbox, exchange, account_bus: ctx.account_bus, log })
                    })
                    .await
                    .map_err(|e| ExchangeError::Other(format!("{e:#}")))?;
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
            if let AccountData::Exchange(ExchangeAccountData::AccountInfo { exchange, .. }) = ev.data {
                self.log.lock().unwrap().push(format!("观察者收到 {exchange} 回报"));
            }
        }
        self.log.lock().unwrap().push("观察者收尾".into());
        Ok(())
    }
}

fn is_account_info(ev: &AccountEvent) -> bool {
    matches!(ev.data, AccountData::Exchange(ExchangeAccountData::AccountInfo { .. }))
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
            paper: crate::sim::SimConfig::default(),
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
            Ok(Observer { inbox, log: observer_log })
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
            paper: crate::sim::SimConfig::default(),
        },
    )
    .await;
    let Err(err) = result else { panic!("一所起不来，Manager 必须构造失败") };
    assert!(format!("{err:#}").contains("OKX 私有订阅被拒"), "{err:#}");

    root.stopping().await;
    let root_err = format!("{:#}", root.shutdown().await.unwrap_err());
    assert!(root_err.contains("OKX 私有订阅被拒"), "{root_err}");
    assert_eq!(*log.lock().unwrap(), ["Binance 节点收尾"], "已起来的节点必须被停下收尾");
}
