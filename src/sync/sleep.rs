use alloc::sync::Arc;
use core::cell::RefCell;

use crate::sync::{Lock, Semaphore};
use crate::thread::{self, Thread};

/// Sleep lock. Uses [`Semaphore`] under the hood.
#[derive(Clone)]
pub struct Sleep {
    /// 初值为 1 的信号量，充当二元锁：空闲 = 1，被占用 = 0。
    inner: Semaphore,
    /// 当前持有者，用来断言"只有持有者才能释放"。
    /// 这正是 `Sleep` 与裸信号量的区别。
    holder: RefCell<Option<Arc<Thread>>>,
}

impl Default for Sleep {
    fn default() -> Self {
        Self {
            // 1 表示锁当前空闲。
            inner: Semaphore::new(1),
            holder: Default::default(),
        }
    }
}

impl Lock for Sleep {
    fn acquire(&self) {
        // 拿信号量；已被占用时会阻塞自己，直到 release 里的 up()。
        self.inner.down();
        // 记下持有者。
        self.holder.borrow_mut().replace(thread::current());
    }

    fn release(&self) {
        // 断言释放者就是持有者。
        assert!(Arc::ptr_eq(
            self.holder.borrow().as_ref().unwrap(),
            &thread::current()
        ));

        // 先清掉持有者记录，再放开信号量。
        self.holder.borrow_mut().take().unwrap();
        self.inner.up();
    }
}

// 手写 Sync 的依据：holder 只在持有 inner 信号量期间被访问，
// 而 inner 的访问本身由关中断保护。
unsafe impl Sync for Sleep {}
