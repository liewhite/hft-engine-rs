//! 挂单对齐：把各所的 maker 挂单对到策略此刻想要的样子。
//!
//! 策略每一步只说「各所想挂什么」（[`Quote`]），这里负责做到：不符的撤、缺的挂、撤单失败隔一会再撤、
//! 下单失败后该所退避一阵再挂、价格偏得不多或挂得不久的单留着不动。这些是挂单本身的规则，与资金费决策无关；
//! 订单的状态（下单中、挂着、撤单中）由框架的订单列表维护，这里只读。

use crate::domain::{
    to_f64, Exchange, OrderIntent, OrderType, Price, Quantity, Side, Symbol, TimeInForce, Timestamp,
};
use crate::messaging::{CancelAttempt, OrderNotice, OrderNoticeKind, OrderStage, PendingOrder};
use crate::strategy::{Signal, SymbolView};
use std::collections::HashMap;

/// 想在某所挂的一张 maker 单
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Quote {
    pub exchange: Exchange,
    pub side: Side,
    pub price: Price,
    /// `None` = 这一刻定不出量（对手盘第一档太薄、或凑不齐两所都下得出的一手）：不新挂，
    /// 但已挂着的同向单按价格容差照常保留。盘口厚度一跳一跳地变，否则会挂了马上撤、撤了又挂
    /// （实盘 11-12 首轮 OKX INIT 一秒撤挂数次），白耗下单额度
    pub qty: Option<Quantity>,
    pub reduce_only: bool,
}

/// 挂单规则的参数（取自策略配置）
#[derive(Debug, Clone)]
pub(super) struct MakerRules {
    /// 挂单价偏离目标价超过目标价的这个比例才撤了重挂（= order_distance × requote_ratio）
    pub requote_tolerance: f64,
    /// 挂了不到这么久的单不因价格偏离而撤
    pub min_requote_ms: u64,
    /// 撤单失败后隔多久再撤
    pub cancel_retry_ms: u64,
    /// 下单失败后第一次退避多久
    pub reject_backoff_ms: u64,
}

/// 拒单退避的上限：持续拒单时每所每币最多 5 分钟试一次
const MAX_REJECT_BACKOFF_MS: u64 = 5 * 60_000;

/// 某所连续下单失败时的退避：第一次失败后等 `reject_backoff_ms` 再挂新的 maker，之后每连续失败一次等待
/// 翻倍，封顶 [`MAX_REJECT_BACKOFF_MS`]；该所有一张单被交易所接下（挂上或成交）就清零。
///
/// 不退避时，被拒的单离开挂单列表后下一个盘口事件就重挂：持续性拒单（保证金不足、
/// 杠杆档位、本地或交易所限频）会按 REST 往返的速度刷单，白耗下单次数。
/// post-only 会吃单被拒这类偶发拒单只等一个间隔，对资费套利无关紧要，故不按拒单原因区分。
/// 口径是该所本策略的任何单：市价补腿失败也会让该所的 maker 退避，补腿成交也会清零；
/// 但退避只拦新挂 maker，不拦补腿（补腿另有 `hedge_retry_ms` 的重试间隔）。失败从哪条路来、来几次，
/// 框架的订单列表都只报一次（见 [`OrderNotice`]）。
#[derive(Debug, Clone)]
struct RejectBackoff {
    /// 这之前不在该所挂新的 maker
    until: Timestamp,
    /// 本次等待时长（下次再失败就翻倍）
    delay_ms: u64,
}

/// 一个币在各所的挂单对齐
pub(super) struct Makers {
    symbol: Symbol,
    rules: MakerRules,
    backoff: HashMap<Exchange, RejectBackoff>,
}

impl Makers {
    pub fn new(symbol: Symbol, rules: MakerRules) -> Self {
        Self {
            symbol,
            rules,
            backoff: HashMap::new(),
        }
    }

    /// 按订单列表的结论更新退避：下单失败加长该所的退避，被接下清零
    pub fn on_notices(&mut self, notices: &[OrderNotice], now: Timestamp) {
        for n in notices {
            match &n.kind {
                OrderNoticeKind::PlaceFailed { reason } => {
                    let delay_ms = self
                        .backoff
                        .get(&n.exchange)
                        .map_or(self.rules.reject_backoff_ms, |b| {
                            (b.delay_ms * 2).min(MAX_REJECT_BACKOFF_MS)
                        });
                    tracing::warn!(symbol = %self.symbol, exchange = %n.exchange, client_order_id = %n.client_order_id, %reason, delay_ms, "下单失败，该所暂停挂新单");
                    self.backoff.insert(
                        n.exchange,
                        RejectBackoff {
                            until: now + delay_ms,
                            delay_ms,
                        },
                    );
                }
                OrderNoticeKind::Accepted => {
                    self.backoff.remove(&n.exchange);
                }
            }
        }
    }

    /// 让 `exchanges` 各所的挂单与 `quotes` 一致：不符的撤，缺的挂（退避中的所不挂）。
    /// 不在 `quotes` 里的所 = 那里不该有单
    pub fn align(
        &self,
        sv: SymbolView<'_>,
        exchanges: &[Exchange],
        quotes: &[Quote],
        now: Timestamp,
    ) -> Vec<Signal> {
        let mut out = Vec::new();
        for &ex in exchanges {
            let want = quotes.iter().find(|q| q.exchange == ex);
            let pendings: Vec<&PendingOrder> = sv
                .pending_orders()
                .filter(|p| p.order.exchange == ex)
                .collect();
            if pendings.is_empty() {
                if let Some((q, qty)) = want.and_then(|q| q.qty.map(|n| (q, n))) {
                    if !self.backing_off(ex, now) {
                        out.push(self.place(q, qty));
                    }
                }
                continue;
            }
            let mut kept = false;
            for p in pendings {
                if !kept && want.is_some_and(|q| self.matches(p, q, now)) {
                    kept = true;
                } else {
                    out.extend(self.cancel(p, now));
                }
            }
        }
        out
    }

    /// 撤掉全部开着的 maker
    pub fn cancel_all(&self, sv: SymbolView<'_>, now: Timestamp) -> Vec<Signal> {
        sv.pending_orders()
            .filter(|p| matches!(p.order.order_type, OrderType::Limit { .. }))
            .filter_map(|p| self.cancel(p, now))
            .collect()
    }

    fn backing_off(&self, ex: Exchange, now: Timestamp) -> bool {
        self.backoff.get(&ex).is_some_and(|b| now < b.until)
    }

    /// 挂着的单是否可以留着：方向与 reduce_only 一致，且价格在容差内或挂得还不够久
    fn matches(&self, p: &PendingOrder, q: &Quote, now: Timestamp) -> bool {
        let OrderType::Limit { price, .. } = p.order.order_type else {
            return false;
        };
        let young = now.saturating_sub(p.created_at) < self.rules.min_requote_ms;
        p.order.side == q.side
            && p.order.reduce_only == q.reduce_only
            && (young
                || to_f64((price - q.price).abs())
                    <= to_f64(q.price) * self.rules.requote_tolerance)
    }

    fn place(&self, q: &Quote, qty: Quantity) -> Signal {
        Signal::PlaceOrders {
            orders: vec![OrderIntent {
                exchange: q.exchange,
                symbol: self.symbol.clone(),
                side: q.side,
                order_type: OrderType::Limit {
                    price: q.price,
                    tif: TimeInForce::PostOnly,
                },
                quantity: qty,
                reduce_only: q.reduce_only,
            }],
            comment: format!(
                "funding_arb maker | {} {:?} qty={} px={} reduce_only={}",
                q.exchange, q.side, qty, q.price, q.reduce_only
            ),
        }
    }

    /// 撤一张开着的单。下单请求还没结论的，框架丢弃这次撤单、受理的结论到了再撤；
    /// 撤单在途时不重发，撤单失败（多半是单刚成交、终态马上就到）后隔 `cancel_retry_ms` 再撤。
    fn cancel(&self, p: &PendingOrder, now: Timestamp) -> Option<Signal> {
        // 哪些单不该发撤单（下单中、撤单在途）由框架挡；这里只定撤单失败后隔多久重撤
        if let OrderStage::Cancelling {
            attempt: CancelAttempt::Failed { at, reason },
            ..
        } = p.stage()
        {
            if now.saturating_sub(*at) < self.rules.cancel_retry_ms {
                return None;
            }
            tracing::warn!(symbol = %self.symbol, exchange = %p.order.exchange, client_order_id = %p.order.client_order_id, %reason, "撤单失败且单还开着，重撤");
        }
        Some(Signal::CancelOrder {
            symbol: self.symbol.clone(),
            client_order_id: p.order.client_order_id.clone(),
        })
    }
}
