//! Seek 与在途数据 — 给 pipeline 通道载荷打上 generation 标记
//!
//! seek 会重置 demuxer 和解码器，但已经排在有界通道里的旧 packet/frame 无法被撤销。
//! 不区分新旧的话，seek 之后仍会把旧位置的画面、声音、字幕播出来（画面回跳、
//! position 倒退、一小段旧音频）。因此每个消费线程丢弃早于当前 generation 的项。

use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// 带 seek generation 的通道载荷
#[derive(Debug)]
pub struct Stamped<T> {
    /// 发送方在读取该数据时观察到的 `Pipeline::seek_generation`
    pub generation: u64,
    pub item: T,
}

impl<T> Stamped<T> {
    pub fn new(generation: u64, item: T) -> Self {
        Self { generation, item }
    }

    /// 是否属于更早的 seek generation（即 seek 之前入队的残留数据）
    ///
    /// 只判定"更旧"，不判定"不同"：消费线程在 `recv_current` 里阻塞时
    /// 若发生 seek，随后入队的包带着更新的 generation。相对消费线程的快照
    /// 它是"未来"数据，但确实是 seek 之后想要的内容，丢弃会白白损失
    /// 一次 seek 窗口内的包（约一个 timeout 时长）。
    pub fn is_stale(&self, current_generation: u64) -> bool {
        self.generation < current_generation
    }
}

/// 取出下一个未被 seek 作废的项，丢弃其前的残留项
///
/// 超时/断开语义与 `Receiver::recv_timeout` 一致。
pub fn recv_current<T>(
    rx: &Receiver<Stamped<T>>,
    current_generation: u64,
    timeout: Duration,
) -> Result<Stamped<T>, RecvTimeoutError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(stamped) if stamped.is_stale(current_generation) => {
                tracing::trace!(
                    "drop stale item from seek generation {}",
                    stamped.generation
                );
                continue;
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    #[test]
    fn is_stale_compares_generation() {
        let item = Stamped::new(3, "pkt");
        assert!(!item.is_stale(3));
        // seek 只会前进：更早 generation 的残留视为陈旧
        assert!(item.is_stale(4));
        // 比消费线程快照更新的 generation 是 seek 之后的数据，不得当作陈旧丢弃
        assert!(!item.is_stale(2));
    }

    /// 回归：seek 之前入队的残留数据必须被丢弃，seek 之后的数据不得被误丢
    #[test]
    fn recv_current_skips_stale_items() {
        let (tx, rx) = bounded::<Stamped<u32>>(8);
        // 旧 generation 的残留（seek 前排队）
        tx.send(Stamped::new(0, 1)).unwrap();
        tx.send(Stamped::new(0, 2)).unwrap();
        // seek：generation 前进为 1
        tx.send(Stamped::new(1, 3)).unwrap();
        tx.send(Stamped::new(1, 4)).unwrap();

        let first = recv_current(&rx, 1, Duration::from_secs(1)).unwrap();
        assert_eq!(first.item, 3);
        let second = recv_current(&rx, 1, Duration::from_secs(1)).unwrap();
        assert_eq!(second.item, 4);
        assert_eq!(rx.len(), 0, "陈旧项应已被消费掉，不占通道容量");
    }

    /// 回归：消费线程快照落后于 seek 时，seek 之后入队的包不得被当成陈旧丢弃
    ///
    /// 消费线程在 recv_current 中阻塞期间发生 seek，新位置的包带着更新的
    /// generation；按"不同即陈旧"判定会把这整个窗口的数据丢掉。
    #[test]
    fn recv_current_keeps_items_newer_than_snapshot() {
        let (tx, rx) = bounded::<Stamped<u32>>(4);
        tx.send(Stamped::new(1, 7)).unwrap();
        let got = recv_current(&rx, 0, Duration::from_millis(50)).unwrap();
        assert_eq!(got.item, 7);
    }

    /// 通道里全是陈旧数据时不得无限等待：按超时返回，调用方继续走原有空闲逻辑
    #[test]
    fn recv_current_times_out_when_only_stale_new_arrives_later() {
        let (tx, rx) = bounded::<Stamped<u32>>(4);
        tx.send(Stamped::new(0, 1)).unwrap();
        let started = std::time::Instant::now();
        let err = recv_current(&rx, 1, Duration::from_millis(120)).unwrap_err();
        assert!(matches!(err, RecvTimeoutError::Timeout));
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn recv_current_reports_disconnect() {
        let (tx, rx) = bounded::<Stamped<u32>>(4);
        drop(tx);
        let err = recv_current(&rx, 1, Duration::from_millis(50)).unwrap_err();
        assert!(matches!(err, RecvTimeoutError::Disconnected));
    }
}
