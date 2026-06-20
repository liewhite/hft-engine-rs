use crate::domain::{Price, Side, BBO};

/// 撮合判定 (纯函数)。挂单成交判定即「价格越过挂单价」。
///
/// resting 单是否被 BBO 越过：买单在最优卖价跌破挂单价时成交，卖单在最优买价升破挂单价时成交。
/// 到达撮合时的「可成交性 (marketable)」判定与此完全一致 —— 保证「不可成交即 resting、
/// 下个 BBO 再撮合」自洽。
pub fn crosses(side: Side, limit_price: Price, bbo: &BBO) -> bool {
    match side {
        Side::Long => bbo.ask_price <= limit_price,
        Side::Short => bbo.bid_price >= limit_price,
    }
}

/// resting 单是否被一笔**真实成交**越过 (trade-print 撮合，严格不含相等)：
/// 买单在成交价**跌破**挂单价时成交、卖单在成交价**升破**挂单价时成交。
///
/// 相等不算 (价格只触及挂单价时通常排在队尾，未真正穿过)，是更保守的下界模型。
/// 注：本模型仅比价格、不看 trade 主动方向 (`is_buyer_maker`)，与 ox-demo 一致。
pub fn trade_crosses(side: Side, limit_price: Price, trade_price: Price) -> bool {
    match side {
        Side::Long => trade_price < limit_price,
        Side::Short => trade_price > limit_price,
    }
}

/// 主动成交 (taker) 的对手价：买单吃最优卖价，卖单吃最优买价。
pub fn touch_price(side: Side, bbo: &BBO) -> Price {
    match side {
        Side::Long => bbo.ask_price,
        Side::Short => bbo.bid_price,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Exchange;

    fn bbo(bid: Price, ask: Price) -> BBO {
        BBO {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".to_string(),
            bid_price: bid,
            bid_qty: 1.0,
            ask_price: ask,
            ask_qty: 1.0,
            timestamp: 0,
        }
    }

    #[test]
    fn crosses_buy_when_ask_drops_sell_when_bid_rises() {
        let b = bbo(100.0, 101.0);
        assert!(!crosses(Side::Long, 99.0, &b)); // ask 101 > 99, 不成交
        assert!(crosses(Side::Long, 101.0, &b)); // ask 101 <= 101, 成交
        assert!(!crosses(Side::Short, 102.0, &b)); // bid 100 < 102, 不成交
        assert!(crosses(Side::Short, 100.0, &b)); // bid 100 >= 100, 成交
    }

    #[test]
    fn touch_price_buy_takes_ask_sell_takes_bid() {
        let b = bbo(100.0, 101.0);
        assert_eq!(touch_price(Side::Long, &b), 101.0);
        assert_eq!(touch_price(Side::Short, &b), 100.0);
    }

    #[test]
    fn trade_crosses_strict() {
        // 买单 @100: 成交价 99 跌破 -> 成交; 100 相等 -> 不成交
        assert!(trade_crosses(Side::Long, 100.0, 99.0));
        assert!(!trade_crosses(Side::Long, 100.0, 100.0));
        // 卖单 @100: 成交价 101 升破 -> 成交; 100 相等 -> 不成交
        assert!(trade_crosses(Side::Short, 100.0, 101.0));
        assert!(!trade_crosses(Side::Short, 100.0, 100.0));
    }
}
