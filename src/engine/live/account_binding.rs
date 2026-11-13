//! 策略实例绑定的账户，连同它出生时的持仓（投产握手，见 [`PositionBaseline`]）。
//!
//! 实盘实例只能带着 [`LiveBaselines`] 出生；[`LiveBaselines`] 只能由 [`snapshot`] 从 [`LiveLegs`]
//! 造出，[`LiveLegs`] 只能由 [`PlannedAccount::bind`] 给每条腿绑上该所账户造出。于是「实盘腿没有
//! 快照、只好补 0」写不出来，「实盘实例要交易的所没配凭证」只在绑定这一处判断 —— 此前这两件事
//! 靠投产时散在两处的运行时检查拦（#767 a800755、a4d4aea），漏检就是把真实持仓当空仓、或下单在
//! 出口被拒留单边敞口。
//!
//! 策略要在哪些所交易是它运行时声明的（`public_streams`），所以「这个所有没有账户」仍是运行时
//! 判断，不是编译期保证；但只有绑定这一处，之后的环节拿到的都是已经齐全的类型。

use crate::domain::{
    now_ms, AccountId, Exchange, ExchangeError, Position, Quantity, Symbol, Timestamp,
};
use crate::exchange::AccountClient;
use crate::messaging::PositionBaseline;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// 一条腿：(所, symbol)
pub type Leg = (Exchange, Symbol);

/// 实盘实例的出生持仓：作用域内**每条腿**一条，都来自交易所 REST 快照（交易所没列出的腿 = 交易所
/// 说它空仓，不是补的）。字段私有，只能由 [`snapshot`] 构造。
#[derive(Debug)]
pub struct LiveBaselines(Vec<PositionBaseline>);

impl LiveBaselines {
    /// 测试用：直接给定基线（不打交易所）
    #[cfg(test)]
    pub(crate) fn for_test(baselines: Vec<PositionBaseline>) -> Self {
        Self(baselines)
    }
}

/// 策略实例绑定的账户和它出生时的持仓
pub enum AccountBinding {
    /// 实盘账户：持仓来自启动时的交易所快照
    Live(LiveBaselines),
    /// 模拟账户（标签见 [`AccountId::Paper`]）：作用域内每条腿从 0 起步，不看也不该看真实持仓
    Paper { label: String, legs: HashSet<Leg> },
}

impl AccountBinding {
    pub fn account(&self) -> AccountId {
        match self {
            Self::Live(_) => AccountId::Live,
            Self::Paper { label, .. } => AccountId::Paper(label.clone()),
        }
    }

    /// 出生持仓：作用域内每条腿一条。缺一条，策略就能看到「这条腿持仓未知」这个对它毫无用处的
    /// 中间状态，逼每个调用点写一次分支；填满之后它在策略消费第一条事件前就不存在了。
    ///
    /// 模拟账户的基线取 `snapshot_req_ts = 0`（没有快照可言）。防双计判据是 `local_ts <= seeded_ts`，
    /// 所以 `local_ts == 0` 的 Fill 仍会被它吞掉；今天不可达（纸盘 Fill 取 `now_ms()`），由
    /// `paper_baseline_only_swallows_a_zero_timestamp_fill` 钉住这条边界。
    pub fn into_baselines(self) -> Vec<PositionBaseline> {
        match self {
            Self::Live(LiveBaselines(b)) => b,
            Self::Paper { legs, .. } => legs
                .into_iter()
                .map(|(exchange, symbol)| PositionBaseline {
                    position: Position {
                        exchange,
                        symbol,
                        size: Quantity::ZERO,
                    },
                    snapshot_req_ts: 0,
                })
                .collect(),
        }
    }
}

/// 一个实盘实例的腿，每条都已绑上该所的账户（配了凭证才有）。字段私有，只能由
/// [`PlannedAccount::bind`] 造出：投产第一步就绑，没凭证的所当场拒绝，此时还没有任何副作用。
pub(crate) struct LiveLegs(Vec<(Leg, Arc<dyn AccountClient>)>);

/// 投产计划里一个实例的账户（还没拉快照）
pub(crate) enum PlannedAccount {
    Live(LiveLegs),
    Paper { label: String, legs: HashSet<Leg> },
}

impl PlannedAccount {
    /// 绑定账户。实盘实例要交易的所没配凭证即报错拒绝投产：没凭证的所下单在出口被拒，补腿补不上
    /// 就留单边敞口。
    pub(crate) fn bind(
        account: AccountId,
        legs: HashSet<Leg>,
        accounts: &HashMap<Exchange, Arc<dyn AccountClient>>,
    ) -> Result<Self, ExchangeError> {
        match account {
            AccountId::Paper(label) => Ok(Self::Paper { label, legs }),
            AccountId::Live => legs
                .into_iter()
                .map(|leg| match accounts.get(&leg.0) {
                    Some(client) => Ok((leg, client.clone())),
                    None => Err(ExchangeError::Other(format!(
                        "实盘策略要在 {0} 交易，但 {0} 没配凭证（下单会被拒、补腿补不上），拒绝投产",
                        leg.0
                    ))),
                })
                .collect::<Result<_, _>>()
                .map(|legs| Self::Live(LiveLegs(legs))),
        }
    }
}

/// 拉本批投产的 REST 持仓快照，把计划变成实例的 [`AccountBinding`]（与 `plans` 同序）。每个所只拉
/// 一次，executor / 对账镜像 / 观测镜像用同一份，口径一致。
///
/// `watched`：本批涉及的全部腿，有账户的也拉快照交给对账与观测镜像（没账户的所跳过：下不了单也
/// 收不到成交，没什么可对的），作为第二个返回值。
///
/// 拉取失败即拒绝投产：基线只有这一次机会，之后全程靠 Fill 累加，没有第二个来源能纠正它；
/// 从「仓位 0」起算会让杠杆、仓位上限等闸门基于错误的持仓放行。
pub(crate) async fn snapshot(
    plans: Vec<PlannedAccount>,
    watched: &HashSet<Leg>,
    accounts: &HashMap<Exchange, Arc<dyn AccountClient>>,
) -> Result<(Vec<AccountBinding>, Vec<PositionBaseline>), ExchangeError> {
    let mut wanted: HashMap<Exchange, Arc<dyn AccountClient>> = HashMap::new();
    for plan in &plans {
        if let PlannedAccount::Live(LiveLegs(legs)) = plan {
            for ((exchange, _), client) in legs {
                wanted.insert(*exchange, client.clone());
            }
        }
    }
    for (exchange, _) in watched {
        if let Some(client) = accounts.get(exchange) {
            wanted.insert(*exchange, client.clone());
        }
    }

    let mut snapshots: HashMap<Exchange, (Timestamp, HashMap<Symbol, Position>)> = HashMap::new();
    for (exchange, client) in wanted {
        // 快照**请求**时刻：在此之前已由私有流送达的 Fill，其成交必然已包含在快照里 —— 消费者
        // seed 之后据此过滤，避免「快照已含 + 再累加」的双计（见 SymbolState::seed_position）
        let snapshot_req_ts = now_ms();
        let positions = client.fetch_positions().await.map_err(|e| {
            ExchangeError::Other(format!(
                "无法获取 {exchange} 的初始持仓基线，拒绝启动（基线只有一次机会，错了之后无从纠正）: {e}"
            ))
        })?;
        let by_symbol = crate::exchange::client::index_positions(positions)
            .map_err(|e| ExchangeError::Other(format!("{e}，拒绝启动")))?;
        snapshots.insert(exchange, (snapshot_req_ts, by_symbol));
    }

    // 交易所没列出的腿 = 交易所说它空仓
    let baseline = |(exchange, symbol): &Leg| -> Option<PositionBaseline> {
        let (snapshot_req_ts, by_symbol) = snapshots.get(exchange)?;
        let position = by_symbol.get(symbol).cloned().unwrap_or(Position {
            exchange: *exchange,
            symbol: symbol.clone(),
            size: Quantity::ZERO,
        });
        Some(PositionBaseline {
            position,
            snapshot_req_ts: *snapshot_req_ts,
        })
    };
    let bindings = plans
        .into_iter()
        .map(|plan| match plan {
            // 上面已为每个实盘腿的所拉了快照
            PlannedAccount::Live(LiveLegs(legs)) => AccountBinding::Live(LiveBaselines(
                legs.iter()
                    .map(|(leg, _)| baseline(leg).expect("实盘腿的所已拉快照"))
                    .collect(),
            )),
            PlannedAccount::Paper { label, legs } => AccountBinding::Paper { label, legs },
        })
        .collect();
    let watched: Vec<PositionBaseline> = watched.iter().filter_map(baseline).collect();
    for b in &watched {
        tracing::info!(exchange = %b.position.exchange, symbol = %b.position.symbol, size = %b.position.size, "Initial position loaded");
    }
    Ok((bindings, watched))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountInfo, OrderId, OrderUpdate};
    use crate::exchange::ExchangeOrder;
    use rust_decimal_macros::dec;

    const SYM: &str = "BTC";
    const WITH_ACCOUNT: Exchange = Exchange::Binance;
    const NO_ACCOUNT: Exchange = Exchange::OKX;

    /// 只会报持仓的账户：BTC 7.0
    struct Holds7;

    #[async_trait::async_trait]
    impl AccountClient for Holds7 {
        fn exchange(&self) -> Exchange {
            WITH_ACCOUNT
        }
        async fn fetch_pending_orders(
            &self,
            _: &Symbol,
        ) -> Result<Vec<OrderUpdate>, ExchangeError> {
            unreachable!("拉快照不查挂单")
        }
        async fn cancel_order(&self, _: &Symbol, _: &str) -> Result<(), ExchangeError> {
            unreachable!("拉快照不撤单")
        }
        async fn place_order(
            &self,
            _: ExchangeOrder,
        ) -> Result<OrderId, crate::domain::PlaceError> {
            unreachable!("拉快照不下单")
        }
        async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError> {
            unreachable!("拉快照不查账户")
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
            Ok(vec![Position {
                exchange: WITH_ACCOUNT,
                symbol: SYM.to_string(),
                size: dec!(7.0),
            }])
        }
    }

    fn accounts() -> HashMap<Exchange, Arc<dyn AccountClient>> {
        HashMap::from([(WITH_ACCOUNT, Arc::new(Holds7) as Arc<dyn AccountClient>)])
    }

    fn leg(exchange: Exchange, symbol: &str) -> Leg {
        (exchange, symbol.to_string())
    }

    fn size_at(baselines: &[PositionBaseline], leg: &Leg) -> Option<Quantity> {
        baselines
            .iter()
            .find(|b| (b.position.exchange, &b.position.symbol) == (leg.0, &leg.1))
            .map(|b| b.position.size)
    }

    /// 实盘实例：作用域内每条腿都来自快照，交易所没列出的腿是 0
    #[tokio::test]
    async fn live_scope_gets_every_leg_from_the_snapshot() {
        let scope = HashSet::from([leg(WITH_ACCOUNT, SYM), leg(WITH_ACCOUNT, "ETH")]);
        let plan = PlannedAccount::bind(AccountId::Live, scope.clone(), &accounts()).unwrap();
        let (bindings, _) = snapshot(vec![plan], &scope, &accounts()).await.unwrap();
        let b = bindings.into_iter().next().unwrap().into_baselines();
        assert_eq!(b.len(), 2);
        assert_eq!(size_at(&b, &leg(WITH_ACCOUNT, SYM)), Some(dec!(7.0)));
        assert_eq!(size_at(&b, &leg(WITH_ACCOUNT, "ETH")), Some(dec!(0)));
        assert!(
            b.iter().all(|x| x.snapshot_req_ts > 0),
            "实盘基线带快照请求时刻"
        );
    }

    /// 实盘实例要交易的所没配凭证：拒绝投产（下单会被拒、补腿补不上）
    #[test]
    fn live_scope_on_an_exchange_without_account_is_rejected() {
        let scope = HashSet::from([leg(WITH_ACCOUNT, SYM), leg(NO_ACCOUNT, SYM)]);
        let Err(err) = PlannedAccount::bind(AccountId::Live, scope, &accounts()) else {
            panic!("应拒绝")
        };
        assert!(err.to_string().contains("OKX 没配凭证"), "{err}");
    }

    /// 只给镜像看的腿：没账户的所跳过，不报错
    #[tokio::test]
    async fn watched_legs_skip_exchanges_without_account() {
        let watched = HashSet::from([leg(WITH_ACCOUNT, SYM), leg(NO_ACCOUNT, SYM)]);
        let paper = PlannedAccount::bind(
            AccountId::Paper("x".to_string()),
            watched.clone(),
            &accounts(),
        )
        .unwrap();
        let (bindings, b) = snapshot(vec![paper], &watched, &accounts()).await.unwrap();
        assert!(
            matches!(&bindings[..], [AccountBinding::Paper { .. }]),
            "模拟账户不拿真实快照"
        );
        assert_eq!(b.len(), 1);
        assert_eq!(size_at(&b, &leg(WITH_ACCOUNT, SYM)), Some(dec!(7.0)));
    }

    fn paper(legs: &[Leg]) -> AccountBinding {
        AccountBinding::Paper {
            label: "x".to_string(),
            legs: legs.iter().cloned().collect(),
        }
    }

    /// 模拟账户：作用域内每条腿从 0 起步（不看真实持仓），基线时刻取 0
    #[test]
    fn paper_starts_flat_on_every_leg() {
        let b = paper(&[leg(WITH_ACCOUNT, SYM), leg(NO_ACCOUNT, SYM)]).into_baselines();
        assert_eq!(b.len(), 2);
        assert!(b
            .iter()
            .all(|x| x.position.size == Quantity::ZERO && x.snapshot_req_ts == 0));
    }

    /// **钉住 ts=0 的真实边界**：防双计判据是 `local_ts <= seeded_ts`，所以 `local_ts == 0` 的 Fill
    /// 照样会被模拟账户的 ts=0 基线吞掉。今天不可达（纸盘 Fill 取 `now_ms()`；回测不经投产），
    /// 哪天真有路径产生 `local_ts == 0` 的 Fill，读到这里就知道会发生什么。
    #[test]
    fn paper_baseline_only_swallows_a_zero_timestamp_fill() {
        use crate::messaging::{AccountData, IncomeEvent, StateManager, SymbolAccountData};

        let baselines = paper(&[leg(WITH_ACCOUNT, SYM)]).into_baselines();
        let fill_at = |local_ts: u64| {
            IncomeEvent::account(
                AccountId::Paper("x".to_string()),
                local_ts,
                local_ts,
                AccountData::Symbol(SymbolAccountData::Fill(crate::domain::Fill {
                    exchange: WITH_ACCOUNT,
                    symbol: SYM.to_string(),
                    side: crate::domain::Side::Long,
                    price: dec!(100.0),
                    size: dec!(1.0),
                    client_order_id: None,
                    order_id: "1".to_string(),
                    timestamp: local_ts,
                    fee: 0.0,
                    reason: crate::domain::FillReason::Normal,
                })),
            )
        };
        let size_after = |local_ts: u64| {
            let mut state = StateManager::new(&[SYM.to_string()]);
            state.seed_positions(&baselines);
            state.apply(&fill_at(local_ts)).unwrap();
            state
                .symbol_state(&SYM.to_string())
                .expect("state")
                .position_book()
                .position_size(WITH_ACCOUNT)
        };
        assert_eq!(
            size_after(0),
            dec!(0.0),
            "local_ts == 0 的 Fill 被判成「已含在快照里」而丢弃"
        );
        assert_eq!(size_after(1), dec!(1.0), "local_ts >= 1 照常累加");
    }
}
