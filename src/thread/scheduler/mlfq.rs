use crate::thread::{current, Status};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;

use crate::thread::{Schedule, Thread};

/// Priority Queue scheduler.
#[derive(Default)]
pub struct MLFQ(BTreeMap<u32, VecDeque<Arc<Thread>>>);

impl Schedule for MLFQ {
    fn register(&mut self, thread: Arc<Thread>) {
        (self.0.entry(thread.priority()).or_insert(VecDeque::new())).push_front(thread);
    }
    fn schedule(&mut self) -> Option<Arc<Thread>> {
        let current = current();
        let caller_priority = current.priority();
        let caller_status = current.status();
        match caller_status {
            Status::Running => match self.0.last_entry() {
                Some(mut entry) => {
                    let max_priority = (*entry.key()).clone(); // 避免比较 &u32, u32
                    if max_priority < caller_priority {
                        None
                    } else {
                        let next_thread = entry.get_mut().pop_back();
                        if entry.get().is_empty() {
                            entry.remove_entry();
                        }
                        next_thread
                    }
                }
                _ => None,
            },
            _ => match self.0.last_entry() {
                Some(mut entry) => {
                    let next_thread = entry.get_mut().pop_back();
                    if entry.get().is_empty() {
                        entry.remove_entry();
                    }
                    next_thread
                }
                _ => None,
            },
        }
    }

    fn reprioritize(&mut self, thread: Arc<Thread>, old_pri: u32) {
        let old_queue = self.0.get_mut(&old_pri);
        // 从旧队列中删除这项
        if let Some(old_queue) = old_queue {
            old_queue.retain(|t| Arc::ptr_eq(t, &thread) == false);
            if old_queue.is_empty() {
                self.0.remove(&old_pri);
            }
        }

        // 放入新队列
        self.register(thread);
    }
}
