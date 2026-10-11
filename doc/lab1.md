# Lab 1: Scheduling

---

## Information

Name: 弓晓彤

Email: 2400013168@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.

> With any comments that may help TAs to evaluate your work better, please leave them here.

## Alarm Clock

### Data Structures

> A1: Copy here the **declaration** of every new or modified struct, enum type, and global variable. State the purpose of each within 30 words.

```rust
// src/thread/manager.rs
pub struct Manager {
    pub scheduler: Mutex<Scheduler>,
    pub current: Mutex<Arc<Thread>>,
    all: Mutex<Vec<Arc<Thread>>>,
    sleeping: Mutex<Vec<(i64, Arc<Thread>)>>,
}
```

`Manager` 中新增的 `sleeping` 字段保存 `(唤醒时刻, 线程)`，存放所有正在睡眠的线程的引用，用于定时唤醒。

### Algorithms

> A2: Briefly describe what happens in `sleep()` and the timer interrupt handler.

sleep应该有如下步骤：\
如果ticks<=0，直接return；\
如果ticks>0：
1. 关闭中断，计算出应该sleep到什么时候结束；
2. 把当前进程加入 `Manager` 中的“待唤醒名单” `sleeping`；
3. 阻塞当前进程直到唤醒，恢复中断状态。

将“检查有无应该wake up的线程，并将它们唤醒”这一逻辑包装成一个函数，作为 `Manager` 的method。

时钟中断：先执行 `tick()`；再通过 `check_and_awaken()` 移除并唤醒所有 `deadline <= now` 的线程；最后恢复中断并调用调度器。

> A3: What are your efforts to minimize the amount of time spent in the timer interrupt handler?

`check_and_awaken()` 用 `extract_if()` 批量取出到期线程，释放 `sleeping` 的锁后才逐一 `wake_up()`，缩短持锁时间。当前 `Vec` 实现每个 tick 仍需线性扫描全部睡眠线程，复杂度为 O(n)。

### Synchronization

> A4: How are race conditions avoided when `sleep()` is being called concurrently?

在单核环境中，关中断使“计算截止时间、登记、阻塞”连续执行；`sleeping` 本身也受 `Intr` 互斥锁保护，因此多个线程不会交错修改睡眠队列。

> A5: How are race conditions avoided when a timer interrupt occurs during a call to `sleep()`?

如果不保护临界区，将产生block和wake_up的竞态：如果在阻塞进程之前就到了该唤醒的时间，那么 `wake_up(thread)` 就会传入一个状态为 `Running` 的thread，导致panic。本实现从登记前一直关中断到进入阻塞，可以避免丢失唤醒及线程状态不一致。

## Priority Scheduling

### Data Structures

> B1: Copy here the **declaration** of every new or modified struct, enum type, and global variable. State the purpose of each within 30 words.

```rust
// src/thread/imp.rs
pub struct Thread {
    tid: isize,
    name: &'static str,
    stack: usize,
    status: Mutex<Status>,
    context: Mutex<Context>,
    pub base_priority: AtomicU32,       // 原始优先级
    pub effective_priority: AtomicU32,  // 有效优先级
    pub(crate) waiting_on: Mutex<Option<Weak<SleepInner>>>, // 等待的锁
    pub(crate) held_locks: Mutex<Vec<Weak<SleepInner>>>,    // 持有的锁
    pub userproc: Option<UserProc>,
    pub pagetable: Option<Mutex<PageTable>>,
}
```

`Thread` 新增原始/有效优先级，以及等待的锁、持有的锁，用于计算和递归传播捐赠。

```rust
// src/thread/scheduler/mlfq.rs
pub struct MLFQ(BTreeMap<u32, VecDeque<Arc<Thread>>>);
```

`MLFQ` 按有效优先级组织就绪队列，优先运行最高优先级线程，同级按 FIFO 轮转。

```rust
// src/sync/sema.rs
pub struct Semaphore {
    value: Cell<usize>,
    waiters: RefCell<VecDeque<Arc<Thread>>>,
}
```

`Semaphore` 新增 `waiters` 字段保存阻塞线程。

```rust
// src/sync/condvar.rs
pub struct Condvar(RefCell<BTreeMap<u32, VecDeque<Arc<Semaphore>>>>);
```

`Condvar` 按等待时的优先级保存每个等待者的私有信号量，以便定向通知。

```rust
// src/sync/sleep.rs
#[derive(Default)]
pub struct SleepInner {
    pub holder: RefCell<Option<Arc<Thread>>>,
    pub waiters: RefCell<VecDeque<Arc<Thread>>>,
}

#[derive(Clone)]
pub struct Sleep {
    shared: Arc<SleepInner>,
}
```

`SleepInner` 保存持锁者与等待队列；`Sleep` 通过 `Arc` 共享锁状态，支持等待、捐赠和直接移交锁。

```rust
// src/thread/scheduler.rs
pub trait Schedule: Default {
    fn register(&mut self, thread: Arc<Thread>);
    fn schedule(&mut self) -> Option<Arc<Thread>>;
    fn reprioritize(&mut self, thread: Arc<Thread>, old_pri: u32);
}
```

`Schedule::reprioritize()` 在 Ready 线程有效优先级变化后调整其就绪队列位置。

> B2: Explain the data structure that tracks priority donation. Clarify your answer with any forms of diagram (e.g., the ASCII art).

每个 `Thread` 保存 `base_priority`、`effective_priority`、`waiting_on` 和 `held_locks`；每把 `Sleep` 保存 `holder` 和 `waiters`。有效优先级取原始优先级及所持各锁的等待者有效优先级中的最大值。`Weak` 引用避免线程与锁之间的引用循环。

```text
H(50) --等待锁 A--> M(20) --等待锁 B--> L(10)
          A.holder=M        B.holder=L
```

H 等待 M 持有的 A，而 M 等待 L 持有的 B，捐赠沿 `waiting_on -> holder` 传播，使 M、L 的有效优先级都提升到 50。

### Algorithms

> B3: How do you ensure that the highest priority thread waiting for a lock, semaphore, or condition variable wakes up first?

`Sleep::release()` 和 `Semaphore::up()` 遍历 `VecDeque`，按**当前有效优先级**选择等待者，避免入队后接受捐赠造成的优先级过期。`Condvar::notify_one()` 从 `BTreeMap` 最高优先级项中按 FIFO 取出私有信号量并通知。不过，现在未实现 `Condvar` 与优先级捐赠相适应的机制。`Condvar` 的键是**入队时**的优先级，等待期间发生优先级变化时，不会重新排序。

> B4: Describe the sequence of events when a thread tries to acquire a lock. How is nested donation handled?

`Sleep::acquire()` 先关中断：若锁空闲，登记 `holder` 和线程的 `held_locks`；否则将线程加入 `waiters`，设置 `waiting_on`，对持锁者调用 `propagate_priority_from()`，然后 `block()`。传播函数重新计算有效优先级；若持锁者也在等待另一把锁，就沿其 `waiting_on` 继续向上传播，并按需调整 Ready 队列。获得移交的锁后才继续执行。

> B5: Describe the sequence of events when a lock, which a higher-priority thread is waiting for, is released.

`Sleep::release()` 验证持锁者，移除其 `held_locks` 中的该锁；从等待队列选出当前有效优先级最高的线程，将 `holder` 直接移交给它，并更新新持有者的 `waiting_on`、`held_locks`。随后重新计算旧持有者和新持有者的优先级，最后唤醒新持有者。

### Synchronization

> B6: Describe a potential race in `thread::set_priority()` and explain how your implementation avoids it. Can you use a lock to avoid this race?

竞态：修改 `base_priority` 后、重算 `effective_priority` 前若被时钟中断抢占，调度器可能暂时看到旧优先级。本实现在 `set_priority()` 外层关中断，覆盖“修改原始优先级—传播—调整就绪队列”，再恢复中断并调度。

## Rationale

> C1: Have you considered other design possibilities? You can talk about anything in your solution that you once thought about doing them another way. And for what reasons that you made your choice?

- 睡眠队列：可按截止时间排序以减少时钟中断扫描成本；目前选择 `Vec`，实现简单，但每 tick 为 O(n)。
- 等待队列：曾使用按优先级分组的 `BTreeMap`；捐赠会动态改变优先级，因此 `Sleep`、`Semaphore` 改为 `VecDeque`，在唤醒时重新比较。
- 锁实现：考虑复用 `Semaphore`；最终让 `Sleep` 独立管理 `holder`、`waiters` 并直接移交所有权，避免两套等待队列不一致。
