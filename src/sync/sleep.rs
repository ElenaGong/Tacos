use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::cell::RefCell;

use crate::sbi::interrupt;
use crate::sync::Lock;
use crate::thread::{self, current, propagate_priority_from, wake_up, Thread};

#[derive(Default)]
pub struct SleepInner {
    pub holder: RefCell<Option<Arc<Thread>>>,
    pub waiters: RefCell<VecDeque<Arc<Thread>>>,
}
unsafe impl Sync for SleepInner {}

/// Sleep lock. Uses [`Semaphore`] under the hood.
#[derive(Clone)]
pub struct Sleep {
    shared: Arc<SleepInner>,
    // /// 初值为 1 的信号量，充当二元锁：空闲 = 1，被占用 = 0。
    // inner: Semaphore,
    // /// 当前持有者，用来断言"只有持有者才能释放"。
    // /// 这正是 `Sleep` 与裸信号量的区别。
    // holder: RefCell<Option<Arc<Thread>>>,
    // waiters: RefCell<BTreeMap<u32, VecDeque<Arc<Thread>>>>,
}

impl Default for Sleep {
    fn default() -> Self {
        Self {
            // // 1 表示锁当前空闲。
            // inner: Semaphore::new(1),
            // holder: Default::default(),
            // // TODO: 把holder, waiters组装成 SleepState
            // waiters: RefCell::new(BTreeMap::new()),
            shared: Arc::new(SleepInner::default()),
        }
    }
}

// struct SleepInner {
//     sem: Semaphore,
//     state:
// }

impl Sleep {
    // fn try_acquire(&self) -> bool {
    //     // TODO: （原子地）实现真正的aqcuire
    //     if self.inner.value() == 0 {
    //         return false;
    //     }
    //     return true;
    // }
}
impl Lock for Sleep {
    fn acquire(&self) {
        let old = interrupt::set(false);
        let current = current();
        // TODO
        let owner = self.shared.holder.borrow().clone();
        if let Some(owner) = owner {
            assert!(
                !Arc::ptr_eq(&current, &owner),
                "Recursive sleep lock acquisition"
            );
            // 将当前线程登记为等待者
            self.shared.waiters.borrow_mut().push_front(current.clone()); // clone?

            // 登记该锁为当前线程正等待的锁
            *current.waiting_on.lock() = Some(Arc::downgrade(&self.shared));
            // 向 holder 捐赠优先级，递归传播
            propagate_priority_from(owner);
            // 阻塞，等待锁被移交
            thread::block();
            debug_assert!(self
                .shared
                .holder
                .borrow()
                .as_ref()
                .map_or(false, |holder| Arc::ptr_eq(holder, &current)));
        } else {
            // 直接让当前线程成为锁的持有者
            *self.shared.holder.borrow_mut() = Some(current.clone());
            // 登记该锁为当前线程持有的锁之一
            (*current.held_locks.lock()).push(Arc::downgrade(&self.shared))
        }
        interrupt::set(old);

        // 拿信号量；已被占用时会阻塞自己，直到 release 里的 up()。
        // self.inner.down();
        // 记下持有者。
        // self.holder.borrow_mut().replace(thread::current());
    }

    fn release(&self) {
        let old = interrupt::set(false);
        let current = current();
        // TODO
        // 确认当前线程是 holder
        // 断言释放者就是持有者。
        assert!(Arc::ptr_eq(
            self.shared.holder.borrow().as_ref().unwrap(),
            &current
        ));
        // 从线程持有的锁中删除此锁
        current.held_locks.lock().retain(|weak| {
            weak.upgrade()
                .map_or(false, |lock| !Arc::ptr_eq(&lock, &self.shared))
        });
        // 从waiters中找到有效优先级最高的等待者，记为next
        let next: Option<Arc<Thread>> = {
            let mut q = self.shared.waiters.borrow_mut();
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
        // 将锁移交给 next，或标记为空闲
        *self.shared.holder.borrow_mut() = next.clone();
        if let Some(ref t) = next {
            // next 持有此锁而不再等待此锁
            *t.waiting_on.lock() = None;
            t.held_locks.lock().push(Arc::downgrade(&self.shared));
        }

        // 撤销这把锁带来的捐赠：
        propagate_priority_from(current);
        // 6. 更新新持有者并唤醒
        if let Some(t) = next {
            propagate_priority_from(t.clone());
            wake_up(t);
        }
        // 唤醒新的持有者
        // 先清掉持有者记录，再放开信号量。
        // self.shared.holder.borrow_mut().take().unwrap();
        // self.inner.up();
        interrupt::set(old);
    }
}

// 手写 Sync 的依据：holder 只在持有 inner 信号量期间被访问，
// 而 inner 的访问本身由关中断保护。
unsafe impl Sync for Sleep {}
