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
    /// 计数：0 表示没有可用资源。
    /// 用 `Cell` 是为了只借 `&self` 就能改它（内部可变性）。
    value: Cell<usize>,
    /// 阻塞在这个信号量上的线程，按 FIFO 排队。
    /// `RefCell` 同样是为了只借 `&self` 就能改它。
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
        // 关中断，并记下旧状态；整段对定时器不可见。
        let old = sbi::interrupt::set(false);

        // Is semaphore available?
        // 计数为 0 表示资源不可用，必须等待。
        while self.value() == 0 {
            // `push_front` ensures to wake up threads in a fifo manner
            // 先登记自己，再阻塞
            self.waiters.borrow_mut().push_front(thread::current());

            // Block the current thread until it's awakened by an `up` operation
            // 阻塞自己；被唤醒后回到 while 顶部重新判断
            thread::block();
        }
        // 拿到资源：计数减一
        self.value.set(self.value() - 1);

        // 恢复中断
        sbi::interrupt::set(old);
    }

    /// V operation
    pub fn up(&self) {
        // 关中断
        let old = sbi::interrupt::set(false);
        // 计数加一，同时返回旧值
        let count = self.value.replace(self.value() + 1);

        // Check if we need to wake up a sleeping waiter
        // 有人在等就唤醒队尾那个（配合 push_front，整体是 FIFO）
        if let Some(thread) = self.waiters.borrow_mut().pop_back() {
            // 旧值为 0，才说明确实有等待者
            assert_eq!(count, 0);

            // clone 是因为 wake_up 要拿走所有权
            thread::wake_up(thread.clone());
        }

        // 恢复中断
        sbi::interrupt::set(old);
    }

    /// Get the current value of a semaphore
    pub fn value(&self) -> usize {
        // 只读计数，不关中断；调用者需自行保证访问上下文
        self.value.get()
    }
}
