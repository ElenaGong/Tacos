//! Kernel Threads

mod imp;
pub mod manager;
pub mod scheduler;
pub mod switch;

pub use self::imp::*;
pub use self::manager::Manager;
pub(self) use self::scheduler::{Schedule, Scheduler};

use crate::sbi::interrupt;
use alloc::sync::Arc;

/// Create a new thread
pub fn spawn<F>(name: &'static str, f: F) -> Arc<Thread>
where
    F: FnOnce() + Send + 'static,
{
    Builder::new(f).name(name).spawn()
}

/// Get the current running thread
pub fn current() -> Arc<Thread> {
    // `Manager::get()` 返回全局唯一的线程管理器 `&'static Manager`。
    //   它是一个 `Lazy<Manager>`，第一次调用时才完成初始化，之后直接返回同一个实例。
    //
    // `.current` 是 `Mutex<Arc<Thread>>` 类型的字段，它指向“此刻正在运行的线程”。
    //
    // `.lock()` 对这个互斥锁加锁，返回一个 `MutexGuard`（锁守卫）。
    //   加锁期间，别的线程不能同时读写 `current`。
    //
    // `.clone()` 通过 `Deref` 把守卫当作 `&Arc<Thread>` 使用，然后克隆智能指针。
    //   这里只增加 `Arc` 的引用计数，不复制 `Thread` 本身。
    //   注意：`.clone()` 结束后，这个临时守卫就离开作用域，锁立刻被释放。
    //   所以本函数返回的是“调用瞬间的当前线程”的一份快照。
    Manager::get().current.lock().clone()
}

/// Yield the control to another thread (if there's another one ready to run).
pub fn schedule() {
    Manager::get().schedule()
}

/// Gracefully shut down the current thread, and schedule another one.
/// 返回类型 `!` 表示该函数正常情况下永远不会返回。
pub fn exit() -> ! {
    // 用独立作用域限制 `current` 锁守卫的生命周期，使其在调度前被释放。
    {
        // 获取线程管理器，并锁定其中指向当前运行线程的共享指针。
        let current = Manager::get().current.lock();

        // 仅在启用 `debug` feature 时，打印将要退出的线程。
        #[cfg(feature = "debug")]
        kprintln!("Exit: {:?}", *current);

        // 将当前线程标记为待销毁；切换线程后，`schedule_tail()` 会回收它。
        current.set_status(Status::Dying);
    }

    // 选择其他就绪线程并切换上下文；当前线程不应再获得运行机会。
    schedule();

    // 若控制流到达此处，说明已退出的线程被错误地再次调度，立即报错。
    unreachable!("An exited thread shouldn't be scheduled again");
}

/// Mark the current thread as [`Blocked`](Status::Blocked) and
/// yield the control to another thread
pub fn block() {
    // 取到当前正在运行的线程，得到一份 `Arc<Thread>` 快照（锁在 `current()` 内部已释放）。
    let current = current();

    // 把状态改成 `Blocked`（阻塞）。
    // `set_status()` 会先锁住该线程自己的 `status` 字段，写入新值，然后解锁。
    current.set_status(Status::Blocked);

    #[cfg(feature = "debug")]
    kprintln!("[THREAD] Block {:?}", current);

    // 让出 CPU，选择另一个就绪线程运行。
    // 关键点：本线程状态已是 `Blocked`，所以 `Manager::schedule_tail()` 不会把它放回就绪队列。
    // 于是本线程会一直“睡”在这里，直到别的代码调用 `wake_up()` 把它唤醒。
    // 若将来被唤醒并被再次调度，控制流会从 `schedule()` 的下一行继续执行，
    // 也就是本函数的结尾，于是 `block()` 正常返回。
    schedule();
}

/// Wake up a previously blocked thread, mark it as [`Ready`](Status::Ready),
/// and register it into the scheduler.
pub fn wake_up(thread: Arc<Thread>) {
    // 前置条件检查：只有处于 `Blocked` 状态的线程才能被唤醒。
    // `==` 比较会用到 `Status` 上 `#[derive(PartialEq)]` 生成的实现；
    // 断言失败时的报错信息会用到 `#[derive(Debug)]` 生成的实现。
    // 这里的 `thread` 是按值传入的 `Arc<Thread>`，函数结束时它的引用计数自动减一。
    assert_eq!(thread.status(), Status::Blocked);

    // 把状态从 `Blocked` 改成 `Ready`，表示“可以被调度了”。
    thread.set_status(Status::Ready);

    // 仅在启用 `debug` feature 时打印日志。
    // `{:?}` 会调用 `impl Debug for Thread`，输出形如 `name(id)[status]`。
    // 注意这里用的是 `thread` 而不是 `*thread`，因为 `Arc<Thread>` 本身就实现了 `Debug`。
    #[cfg(feature = "debug")]
    kprintln!("[THREAD] Wake up {:?}", thread);

    // `Manager::get()` 取全局管理器；`.scheduler` 是 `Mutex<Scheduler>`。
    // `.lock()` 锁住调度器，`.register(thread)` 把该线程放回就绪队列。
    // `register` 会移动 `thread`（`Arc` 的所有权转移进队列），所以之后不能再使用这个变量。
    // 表达式结束时锁守卫被丢弃，调度器解锁。此时线程真正回到“可运行”状态。
    Manager::get().scheduler.lock().register(thread);
}

/// (Lab1) Sets the current thread's priority to a given value
pub fn set_priority(_priority: u32) {}

/// (Lab1) Returns the current thread's effective priority.
pub fn get_priority() -> u32 {
    0
}

/// (Lab1) Make the current thread sleep for the given ticks.

/// 如果ticks<=0，直接return；
/// 如果ticks>0：
///     1. 计算什么时候该唤醒
///     2. 加入睡眠队列
///     3. 进入block状态
///     4. 时间到了就变回ready
///     注意： 保护临界区
pub fn sleep(ticks: i64) {
    use crate::sbi::timer::timer_ticks;

    if ticks <= 0 {
        return;
    }
    let old = interrupt::set(false);

    // 计算该唤醒的时刻
    let start = timer_ticks();
    let wake_time = start + ticks;

    // 加入睡眠队列
    Manager::get().register_sleeping((wake_time, current()));

    // block自己等待唤醒
    #[cfg(feature = "debug")]
    kprintln!("I'm asleep😴...\n");
    block();
    // sema.down();

    #[cfg(feature = "debug")]
    kprintln!("I'm awakened!😊\n");
    interrupt::set(old);
    // while timer_elapsed(start) < ticks {
    //     schedule();
    // }
}
