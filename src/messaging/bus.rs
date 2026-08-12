//! 两条**入向**事件总线的类型别名。
//!
//! # 为什么在这一层
//!
//! 别名承载的事件类型（[`MarketEvent`] / [`AccountEvent`]）就定义在本模块。别名此前住在
//! `engine::live`，而各所适配层要持 `ActorRef<MarketPubSub>` 才能发布行情 —— 于是
//! **适配层反向依赖了引擎层**（`docs/architecture.md` V9）。别名跟着它承载的类型走，
//! 这条反向边就消失了。
//!
//! # 出向总线不在这里，理由不同
//!
//! `OutcomePubSub` 承载的 `AccountOutcome` 包着 `strategy::OutcomeEvent`（策略产出的信号），
//! 而 messaging 不依赖 strategy —— 搬过来会造出一条新的反向边。它留在 `engine::live`，
//! 那也正是它的归属：**"哪个账户的信号"是引擎的编排概念，不是消息层的数据概念**。

use crate::messaging::{AccountEvent, MarketEvent};
use kameo_actors::pubsub::PubSub;

/// 公共行情总线：承载 [`MarketEvent`]（无账户归属，一份服务所有账户）。
pub type MarketPubSub = PubSub<MarketEvent>;

/// 账户私有事件总线：承载 [`AccountEvent`]（账户是必填结构字段）。
///
/// 实盘适配层（标 `AccountId::Live`）与本地柜台 `PaperCounterActor`（标 `Paper(x)`）
/// 发布**同一个类型**到这一条总线，消费者按 `account` 字段取自己的那份 ——
/// 账户隔离由类型与字段值保证，不靠总线拓扑，也不靠"来源即 Live"的推断。
pub type AccountPubSub = PubSub<AccountEvent>;
