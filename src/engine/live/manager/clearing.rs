//! 启动清场：引擎启动时撤掉各所账户上的全部挂单。
//!
//! 前提是账户由引擎独占（`docs/architecture.md`「账户独占」）：账户上的挂单都是本引擎上次运行留下的，
//! 策略的内存态重启后已丢，接管不了（交易所的挂单回报不带 tif 等下单信息），本项目也没有队列位置要保，
//! 所以一律撤掉。范围是**整个账户**、不限本次配置的币：配置删了某个币再重启，它的旧挂单同样要撤，
//! 否则它们会在无人跟踪时成交，开出没人管的仓位。
//!
//! 只在启动时做一次（[`super::Manager`] 构造时，任何策略投产之前）：投产可以发生多次，在投产时撤
//! 会撤掉已在运行的实例的挂单。

use crate::domain::{Exchange, ExchangeError};
use crate::exchange::AccountClient;
use std::collections::HashMap;
use std::sync::Arc;

/// 各所并行清场；任一所没撤净就报错（拒绝启动）
pub(super) async fn cancel_all_open_orders(
    accounts: &HashMap<Exchange, Arc<dyn AccountClient>>,
) -> Result<(), ExchangeError> {
    futures_util::future::try_join_all(
        accounts
            .iter()
            .map(|(exchange, client)| clear(*exchange, client.as_ref())),
    )
    .await?;
    Ok(())
}

/// 撤掉一所的全部挂单，并**复查确认撤净**。
///
/// 单笔撤单失败不直接判死：可能只是「那张单刚好成交或已被撤」这类良性竞态，是否真的撤净由复查裁定。
/// 复查后仍有挂单（如账户上有人手动下的单，前提被破坏）就拒绝启动：留着无人看管的挂单开跑，策略会在
/// 旁边重复挂单；而启动期失败是最便宜的失败时机（还没开始交易）。
async fn clear(exchange: Exchange, client: &dyn AccountClient) -> Result<(), ExchangeError> {
    let leftover = client.fetch_open_orders().await?;
    if leftover.is_empty() {
        return Ok(());
    }
    tracing::warn!(%exchange, count = leftover.len(), orders = ?leftover, "[STARTUP] 账户上有遗留挂单，全部撤掉（不接管）");
    for order in &leftover {
        if let Err(e) = client.cancel_open_order(order).await {
            tracing::warn!(%exchange, ?order, error = %e, "撤单请求失败，由复查裁定是否真的还在");
        }
    }
    let remaining = client.fetch_open_orders().await?;
    if remaining.is_empty() {
        return Ok(());
    }
    Err(ExchangeError::new(format!(
        "{exchange}: 撤单后复查仍有 {} 张挂单，拒绝启动\
         （留着无人看管的挂单开跑，策略会在旁边重复挂单；撤不掉的多半是账户上有人手动下的单）：{remaining:?}",
        remaining.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountInfo, OrderId, Position, Symbol};
    use crate::exchange::{ExchangeOrder, OpenOrder};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const EX: Exchange = Exchange::Binance;

    /// 已发出的撤单记录，与 client 共享一份，供断言读取
    type CancelLog = Arc<Mutex<Vec<OrderId>>>;

    /// 只实现清场用到的方法；其余按「不该被调用」处理 —— 被调用说明清场越界了
    struct FakeClient {
        /// 每次 `fetch_open_orders` 依次返回一份：模拟「撤单前 / 撤单后复查」两次调用
        rounds: Mutex<VecDeque<Vec<OpenOrder>>>,
        cancelled: CancelLog,
    }

    fn fake(rounds: Vec<Vec<OpenOrder>>) -> (FakeClient, CancelLog) {
        let cancelled: CancelLog = Arc::new(Mutex::new(Vec::new()));
        let client = FakeClient {
            rounds: Mutex::new(rounds.into()),
            cancelled: cancelled.clone(),
        };
        (client, cancelled)
    }

    #[async_trait::async_trait]
    impl AccountClient for FakeClient {
        fn exchange(&self) -> Exchange {
            EX
        }
        async fn fetch_open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
            Ok(self.rounds.lock().unwrap().pop_front().unwrap_or_default())
        }
        async fn cancel_open_order(&self, order: &OpenOrder) -> Result<(), ExchangeError> {
            self.cancelled.lock().unwrap().push(order.order_id.clone());
            Ok(())
        }
        async fn cancel_order(&self, _: &Symbol, _: &str) -> Result<(), ExchangeError> {
            unreachable!("清场按交易所订单号撤，不按客户端订单号")
        }
        async fn place_order(
            &self,
            _: ExchangeOrder,
        ) -> Result<OrderId, crate::domain::PlaceError> {
            unreachable!("清场不该下单")
        }
        async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError> {
            unreachable!("清场不该查账户")
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
            unreachable!("清场不该查持仓")
        }
    }

    fn order(instrument: &str, order_id: &str) -> OpenOrder {
        OpenOrder {
            instrument: instrument.to_string(),
            order_id: order_id.to_string(),
        }
    }

    /// 没有挂单时，一个撤单请求都不发
    #[tokio::test]
    async fn nothing_to_cancel_sends_no_request() {
        let (client, cancelled) = fake(vec![vec![]]);
        clear(EX, &client).await.expect("空列表应直接通过");
        assert!(cancelled.lock().unwrap().is_empty());
    }

    /// 账户上的挂单全部撤掉（含本次配置之外的币），复查撤净即通过
    #[tokio::test]
    async fn every_open_order_on_the_account_is_cancelled() {
        let (client, cancelled) = fake(vec![
            vec![order("BTCUSDT", "1"), order("DOGEUSDT", "2")],
            vec![],
        ]);
        clear(EX, &client).await.expect("撤净应通过");
        assert_eq!(*cancelled.lock().unwrap(), vec!["1", "2"]);
    }

    /// 复查仍有挂单 → 拒绝启动，并指明是哪张
    #[tokio::test]
    async fn still_present_after_cancel_refuses_startup() {
        let stuck = vec![order("BTCUSDT", "manual-9")];
        let (client, _) = fake(vec![stuck.clone(), stuck]);
        let err = clear(EX, &client).await.expect_err("撤不掉必须报错");
        assert!(err.to_string().contains("manual-9"), "{err}");
    }
}
