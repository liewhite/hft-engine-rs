//! 基于 tokio 的组件运行时：任务树（生命周期与监督）、收件箱、事件总线。
//!
//! 取代 kameo。只做我们用到的三件事，每件都是显式的：
//! - [`tree`]：组件挂在树上，出错向上冒、停机自上而下传信号自下而上收尾；
//! - [`inbox`]：组件收消息的地方，生死只跟着任务走，停机时排空；
//! - [`bus`]：一对多扇出，订阅凭证 drop 即退订。

pub mod bus;
pub mod inbox;
pub mod periodic;
pub mod tree;

pub use bus::{Bus, Subscription};
pub use inbox::{Addr, Gone, Inbox};
pub use periodic::Periodic;
pub use tree::{ChildHandle, FailHandle, Group, GroupClosed, Root, Scope, Task};
