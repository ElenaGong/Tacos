//! # Condition Variable
//!
//! [`Condvar`] are able to block a thread so that it consumes no CPU time
//! while waiting for an event to occur. It is typically associated with a
//! boolean predicate (a condition) and a mutex. The predicate is always verified
//! inside of the mutex before determining that a thread must block.
//!
//! ## Usage
//!
//! Suppose there are two threads A and B, and thread A is waiting for some events
//! in thread B to happen.
//!
//! Here is the common practice of thread A:
//! ```rust
//! let pair = Arc::new(Mutex::new(false), Condvar::new());
//!
//! let (lock, cvar) = &*pair;
//! let condition = lock.lock();
//! while !condition {
//!     cvar.wait(&condition);
//! }
//! ```
//!
//! Here is a good practice of thread B:
//! ```rust
//! let (lock, cvar) = &*pair;
//!
//! // Lock must be held during a call to `Condvar.notify_one()`. Therefore, `guard` has to bind
//! // to a local variable so that it won't be dropped too soon.
//!
//! let guard = lock.lock(); // Bind `guard` to a local variable
//! *guard = true;           // Condition change
//! cvar.notify_one();       // Notify (`guard` will overlive this line)
//! ```
//!
//! Here is a bad practice of thread B:
//! ```rust
//! let (lock, cvar) = &*pair;
//!
//! *lock.lock() = true;     // Lock won't be held after this line.
//! cvar.notify_one();       // Buggy: notify another thread without holding the Lock
//! ```
//!

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::cell::RefCell;

use crate::sync::{Lock, MutexGuard, Semaphore};
use crate::thread;

/// 等待队列。队列里放的不是线程，而是**每个等待线程自己私有的信号量**（初值 0）。
/// 这样 `notify_one` 才能精确点名唤醒某一个等待者，而不是"谁抢到算谁"。
/// `RefCell` 提供内部可变性：这些方法只拿得到 `&self`。
pub struct Condvar(RefCell<BTreeMap<u32, VecDeque<Arc<Semaphore>>>>);

// 手写 Sync/Send 的依据：对 `self.0` 的每一次访问都由**调用者手里的那把锁**串行化
// —— `wait` 在释放锁之前就入队，`notify_*` 按约定必须在持锁时调用。
unsafe impl Sync for Condvar {}
unsafe impl Send for Condvar {}

impl Condvar {
    pub fn new() -> Self {
        // `Default` 给一个空的 `VecDeque`
        Condvar(Default::default())
    }

    /// 阻塞当前线程直到被唤醒。等待期间会释放 `guard` 保护的锁。
    pub fn wait<T, L: Lock>(&self, guard: &mut MutexGuard<'_, T, L>) {
        // 1. 给自己造一个私有信号量（初值 0），它是"我正在等"的凭据
        let sema = Arc::new(Semaphore::new(0));
        // 2. 先把凭据挂到队首。
        //    顺序很关键：必须在下面 `release()` 之前入队。
        //    否则通知可能插在"检查条件"和"入队"之间发生，那次唤醒就丢了。
        //    `clone` 克隆的是 `Arc`（只加引用计数），队列和本函数各持一份。
        let current = thread::current();
        let priority = current.priority();
        self.0
            .borrow_mut()
            .entry(priority)
            .or_insert(VecDeque::new())
            .push_front(sema.clone());

        // 3. 释放关联的锁。不放的话通知者拿不到锁去改条件，就永远没人来唤醒我
        guard.release();
        // 4. 阻塞自己。等 `notify_one`/`notify_all` 对这个 sema 调 `up`，`down` 才返回
        sema.down();
        // 5. 醒来后重新加锁，把锁还给调用者——它要在锁的保护下再次检查条件
        guard.acquire();
    }

    /// Wake up one thread from the waiting list
    pub fn notify_one(&self) {
        // 从队尾取一个凭据。配合上面的 `push_front`，取到的是最早入队的那个（FIFO）
        let mut sema: Option<Arc<Semaphore>> = None;
        if let Some(mut entry) = self.0.borrow_mut().last_entry() {
            // 唤醒它。信号量是私有的，所以不会误唤醒别的等待者
            sema = entry.get_mut().pop_back();
            if entry.get().is_empty() {
                entry.remove_entry();
            }
        }
        if let Some(sema) = sema {
            sema.up();
        }
    }

    /// Wake up all waiting threads
    pub fn notify_all(&self) {
        // 先只借 `&self` 遍历并逐个唤醒，避免"边遍历边可变借用"
        self.0.borrow().iter().for_each(|entry| {
            let mut queue = entry.1.clone();
            while !queue.is_empty() {
                let sema = queue.pop_back();
                if let Some(sema) = sema {
                    sema.up();
                }
            }
        });
        // 全部唤醒之后清空队列
        self.0.borrow_mut().clear();
    }
}
