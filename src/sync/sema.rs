use alloc::collections::VecDeque;
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
    // // waiters也和Ready状态的线程一样用多级优先级队列存取
    waiters: RefCell<VecDeque<Arc<Thread>>>,
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
            waiters: RefCell::new(VecDeque::new()),
        }
    }

    /// P operation
    pub fn down(&self) {
        let old = sbi::interrupt::set(false);

        // Is semaphore available?
        while self.value() == 0 {
            // `push_front` ensures to wake up threads in a fifo manner
            self.waiters.borrow_mut().push_front(thread::current());

            // Block the current thread until it's awakened by an `up` operation
            thread::block();
        }
        self.value.set(self.value() - 1);

        sbi::interrupt::set(old);
    }

    /// V operation
    pub fn up(&self) {
        let old = sbi::interrupt::set(false);
        self.value.replace(self.value() + 1);

        // Check if we need to wake up a sleeping waiter
        // 唤醒优先级最高的那个
        let to_be_awakened = {
            let mut q = self.waiters.borrow_mut();
            let mut best = None;
            let mut best_priority = 0;

            for (i, thread) in q.iter().enumerate() {
                let p = thread.priority();

                if best.is_none() || p > best_priority {
                    best = Some(i);
                    best_priority = p;
                }
            }
            best.and_then(|i| q.remove(i))
        };
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
