use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::cell::{Cell, RefCell};

use crate::sbi;
use crate::thread::{self, Thread};

/// Atomic counting semaphore
///
/// # Examples
/// ```
/// let sema = Semaphore::new(0);
/// sema.down();
/// sema.up();
/// ```
#[derive(Clone)]
pub struct Semaphore {
    value: Cell<usize>,
    // waiters也和Ready状态的线程一样用多级优先级队列存取
    waiters: RefCell<BTreeMap<u32, VecDeque<Arc<Thread>>>>,
}

// 手写 Sync/Send 的依据：所有读写都在关中断的临界区内完成，
// 所以 Cell/RefCell 不会被真正并发访问。
unsafe impl Sync for Semaphore {}
unsafe impl Send for Semaphore {}

impl Semaphore {
    /// Creates a new semaphore of initial value n.
    /// `const fn` 让它可以出现在 static 初始化里。
    pub const fn new(n: usize) -> Self {
        Semaphore {
            value: Cell::new(n),
            // TODO done
            waiters: RefCell::new(BTreeMap::new()),
        }
    }

    /// P operation
    pub fn down(&self) {
        let old = sbi::interrupt::set(false);

        // Is semaphore available?
        while self.value() == 0 {
            // `push_front` ensures to wake up threads in a fifo manner
            // 先登记自己，再阻塞
            // TODO done
            let current = thread::current();
            let priority = current.priority();
            self.waiters
                .borrow_mut()
                .entry(priority)
                .or_insert(VecDeque::new())
                .push_front(current);
            // self.waiters.borrow_mut().push_front(thread::current());

            // Block the current thread until it's awakened by an `up` operation
            // 阻塞自己；被唤醒后回到 while 顶部重新判断
            thread::block();
        }
        self.value.set(self.value() - 1);

        sbi::interrupt::set(old);
    }

    /// V operation
    pub fn up(&self) {
        let old = sbi::interrupt::set(false);
        let count = self.value.replace(self.value() + 1);

        // Check if we need to wake up a sleeping waiter
        // 唤醒优先级最高的那个
        let mut to_be_awakened: Option<Arc<Thread>> = None;
        if let Some(mut entry) = self.waiters.borrow_mut().last_entry() {
            // 旧值为 0，才说明确实有等待者
            assert_eq!(count, 0);

            to_be_awakened = entry.get_mut().pop_back();
            if entry.get().is_empty() {
                entry.remove_entry();
            }
        }
        // clone 是因为 wake_up 要拿走所有权
        if let Some(to_be_awakened) = to_be_awakened {
            thread::wake_up(to_be_awakened.clone());
        }

        sbi::interrupt::set(old);
    }

    /// Get the current value of a semaphore
    pub fn value(&self) -> usize {
        // 只读计数，不关中断；调用者需自行保证访问上下文
        self.value.get()
    }
}
