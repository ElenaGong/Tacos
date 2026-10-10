use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use crate::sync::{self, Lock};

/// A mutual exclusion primitive useful for protecting shared data
///
/// # Examples
/// ```
/// let foo = Mutex::new(7);
/// {
///     let mut foo = foo.lock();
///     foo += 3;
/// }
/// assert_eq!(foo.lock(), 10);
/// ```
#[derive(Debug, Default)]
pub struct Mutex<T, L: Lock = sync::Primitive> {
    // 被保护的数据。`UnsafeCell` 是通过 `&self` 拿到 `&mut T` 的唯一合法入口，
    // 剩下的安全检查由我们自己负责（见下面的 `Sync` 和 `MutexGuard`）。
    value: UnsafeCell<T>,
    // 用哪种锁。默认是 `Sleep`（拿不到就阻塞）；也可以换成 `Intr` 或 `Spin`。
    lock: L,
}

// The only access to a Mutex's value is MutexGuard, so safety is guaranteed here.
// Requiring T to be Send-able prohibits implementing Sync for types like Mutex<*mut T>.
// 手写标记 trait 的依据：值的唯一入口是 `MutexGuard`，而 `MutexGuard` 必须先加锁才能拿到。
// 约束 `T: Send` 是为了挡住 `Mutex<*mut T>` 这类不应该跨线程共享的类型。
unsafe impl<T: Send, L: Lock> Sync for Mutex<T, L> {}
unsafe impl<T: Send, L: Lock> Send for Mutex<T, L> {}

impl<T, L: Lock> Mutex<T, L> {
    /// Creates a mutex in an unlocked state ready for use.
    pub fn new(value: T) -> Self {
        Self {
            // 把值装进 `UnsafeCell`
            value: UnsafeCell::new(value),
            // 锁用 `L` 的默认构造（`Sleep` 初值 1，表示空闲）
            lock: L::default(),
        }
    }

    /// Acquires a mutex, blocking the current thread until it is able to do so.
    pub fn lock(&self) -> MutexGuard<'_, T, L> {
        // 真正加锁：可能要阻塞（`Sleep`）、关中断（`Intr`）或自旋（`Spin`）
        self.lock.acquire();
        // 把 `&self` 包进守卫返回。返回类型的 `'_` 绑定到 `&self`，
        // 所以只要守卫还活着，原 `Mutex` 就一直被借着。
        MutexGuard(self)
    }
}

/// An RAII implementation of a “scoped lock” of a mutex.
/// When this structure is dropped (falls out of scope), the lock will be unlocked.
///
/// The data protected by the mutex can be accessed through
/// this guard via its Deref and DerefMut implementations.
// 守卫只存一个指向 `Mutex` 的引用：它不拥有数据，只代表"持有权"。
// 字段没有 `pub`，所以只有 `mutex` 模块自己能访问 `guard.0`。
pub struct MutexGuard<'a, T, L: Lock>(&'a Mutex<T, L>);

// 只有当被保护的数据本身是 `Sync` 时，`&MutexGuard` 才能跨线程共享。
unsafe impl<T: Sync, L: Lock> Sync for MutexGuard<'_, T, L> {}

impl<T, L: Lock> Deref for MutexGuard<'_, T, L> {
    // `Target = T` 是"守卫能当数据用"的根据：`*guard`、`guard.method()` 都走这条路。
    type Target = T;

    fn deref(&self) -> &T {
        // 安全性：调用者此刻正持有锁，没有别人能同时拿到 `&mut T`
        unsafe { &*self.0.value.get() }
    }
}

impl<T, L: Lock> DerefMut for MutexGuard<'_, T, L> {
    fn deref_mut(&mut self) -> &mut T {
        // 同理。这里要求 `&mut self`，从类型上保证是独占访问
        unsafe { &mut *self.0.value.get() }
    }
}

impl<T, L: Lock> Drop for MutexGuard<'_, T, L> {
    fn drop(&mut self) {
        // RAII：守卫离开作用域就自动解锁。
        // 所谓"临时值活到语句结束"，就是指这次 `drop` 在分号处被触发。
        self.0.lock.release();
    }
}

// Useful in Condvar
impl<T, L: Lock> MutexGuard<'_, T, L> {
    // `pub(super)` = 只在 `crate::sync` 及其子模块可见（`condvar` 就在其中）。
    // `Condvar::wait` 靠这两个方法"临时让出锁再拿回来"，而守卫本身始终有效。
    pub(super) fn release(&self) {
        // 注意：不消耗守卫，只是把底下的锁放开
        self.0.lock.release();
    }

    pub(super) fn acquire(&self) {
        // 重新加锁。返回后调用者又处在"持有锁"的状态
        self.0.lock.acquire();
    }
}
