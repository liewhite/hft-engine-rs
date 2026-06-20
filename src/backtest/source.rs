use crate::messaging::IncomeEvent;

/// 回测行情数据源：产出**全局按时间戳升序**的市场事件 (BBO / MarketTrade / MarkPrice / ...)。
///
/// 这是回测与"数据从哪来、怎么缓存"之间的唯一缝隙。[`crate::backtest::BacktestEngine`] 只消费
/// 有序事件流，不关心是币安历史文件、内存假数据还是别的来源。
pub trait MarketDataSource {
    fn events(&self) -> Box<dyn Iterator<Item = IncomeEvent> + '_>;
}

/// 让 `Box<dyn MarketDataSource>` 也满足该 trait，便于组装入口擦除具体类型 (含/不含装饰器)。
impl MarketDataSource for Box<dyn MarketDataSource> {
    fn events(&self) -> Box<dyn Iterator<Item = IncomeEvent> + '_> {
        (**self).events()
    }
}
