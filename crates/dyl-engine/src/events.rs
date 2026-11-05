//! 公开事件总线 — seq 单调递增，全部在控制线程上产生。

use crossbeam_channel::Sender;
use dyl_core::contract::{Event, EventEnvelope, SCHEMA_VERSION};

pub(crate) struct EventBus {
    tx: Sender<EventEnvelope>,
    seq: u64,
}

impl EventBus {
    pub(crate) fn new(tx: Sender<EventEnvelope>) -> Self {
        Self { tx, seq: 0 }
    }

    pub(crate) fn emit(&mut self, event: Event) {
        self.seq += 1;
        let _ = self.tx.send(EventEnvelope {
            schema: SCHEMA_VERSION,
            seq: self.seq,
            event,
        });
    }
}
