use crate::domain::Symbol;
use crate::messaging::event::SymbolEvent;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Per-symbol 事件队列统计
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub capacity: usize,
    pub dropped_count: u64,
}

/// 单个 Symbol 的事件队列
struct SymbolQueue {
    sender: mpsc::Sender<SymbolEvent>,
    dropped_counter: AtomicU64,
}

/// Symbol 事件总线 - 管理 per-symbol 的有界队列
pub struct SymbolEventBus {
    queues: HashMap<Symbol, SymbolQueue>,
    capacity: usize,
}

impl SymbolEventBus {
    /// 创建事件总线，为每个 symbol 创建一个有界队列
    pub fn new(
        symbols: &[Symbol],
        capacity: usize,
    ) -> (Arc<Self>, HashMap<Symbol, mpsc::Receiver<SymbolEvent>>) {
        let mut queues = HashMap::new();
        let mut receivers = HashMap::new();

        for symbol in symbols {
            let (tx, rx) = mpsc::channel(capacity);
            queues.insert(
                symbol.clone(),
                SymbolQueue {
                    sender: tx,
                    dropped_counter: AtomicU64::new(0),
                },
            );
            receivers.insert(symbol.clone(), rx);
        }

        (Arc::new(Self { queues, capacity }), receivers)
    }

    /// 发布事件到对应 symbol 的队列 (非阻塞，满时丢弃)
    pub fn publish(&self, event: SymbolEvent) {
        if let Some(symbol) = event.symbol() {
            if let Some(queue) = self.queues.get(symbol) {
                // 使用 try_send 实现 sliding window 语义
                if queue.sender.try_send(event).is_err() {
                    queue.dropped_counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// 获取队列统计
    pub fn stats(&self) -> HashMap<Symbol, QueueStats> {
        self.queues
            .iter()
            .map(|(symbol, queue)| {
                (
                    symbol.clone(),
                    QueueStats {
                        capacity: self.capacity,
                        dropped_count: queue.dropped_counter.load(Ordering::Relaxed),
                    },
                )
            })
            .collect()
    }

    /// 获取单个 symbol 的丢弃统计
    pub fn dropped_count(&self, symbol: &Symbol) -> u64 {
        self.queues
            .get(symbol)
            .map(|q| q.dropped_counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}
