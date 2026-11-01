# Lab 2: User Programs

---

## Information

Name: 弓晓彤

Email: 2400013168@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.
1. [PintOS文档中的参数传递部分](https://pkuflyingpig.gitbook.io/pintos/project-description/lab2-user-programs/background#argument-passing)

## Argument Passing

#### DATA STRUCTURES

> A1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

参数传递复用已有的 `Frame` 和 `Vec<String>`，只新增以下限制常量：
```rust
const MAX_ARGS: usize = 64;         // 限制参数数量
const MAX_ARG_BYTES: usize = 3072;  // 限制总字节数
```

#### ALGORITHMS

> A2: Briefly describe how you implemented argument parsing. How do you arrange for the elements of argv[] to be in the right order? How do you avoid overflowing the stack page?

`sys_exec()` 将用户态路径和 `argv` 转换为内核中的 `String` 和 `Vec<String>`，传给 `execute()`。`execute()` 从栈顶向下写入以 `\0` 结尾的字符串，记录其地址，再按原始顺序写入 `argv[]` 指针数组及末尾的 `NULL`。通过 `checked_sub()` 和栈页边界检查防止溢出，按 ABI 要求对齐后设置 `sp`、`a0 = argc` 和 `a1 = argv`。

#### RATIONALE

> A3: In Tacos, the kernel reads the executable name and arguments from the command. In Unix-like systems, the shell does this work. Identify at least two advantages of the Unix approach.

1. 职责分离：内核无需处理命令解析，降低复杂度。
2. 扩展灵活：shell 可以独立实现变量展开、管道、重定向等功能，无需修改内核。

## System Calls

#### DATA STRUCTURES

> B1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

```rust
// 父子进程关系、退出状态与 wait 同步
pub struct TaskStruct {
    pub tid: AtomicIsize,
    pub parent: Option<Weak<TaskStruct>>,
    pub children: Mutex<Vec<Arc<TaskStruct>>>,
    pub exit_status: Mutex<Option<isize>>,
    pub done: Semaphore,
}
#[repr(C)]
pub struct Thread {
    tid: isize,
    name: &'static str,
    stack: usize,
    status: Mutex<Status>,
    context: Mutex<Context>,
    pub base_priority: AtomicU32,
    pub effective_priority: AtomicU32,
    pub(crate) waiting_on: Mutex<Option<Weak<SleepInner>>>,
    pub(crate) held_locks: Mutex<Vec<Weak<SleepInner>>>,
    pub userproc: Option<UserProc>,
    pub pagetable: Option<Mutex<PageTable>>,
    pub task: Arc<TaskStruct>, // 线程结构新增 task 字段，其余为既有字段

}
pub struct Builder {
    priority: u32,
    name: &'static str,
    function: usize,
    userproc: Option<UserProc>,
    pagetable: Option<PageTable>,
    task: Option<Arc<TaskStruct>>, // 向新线程显式传递任务上下文task

}
// 每个已打开的文件及其访问权限
pub struct OpenFile {
    file: File,
    readable: bool,
    writable: bool,
}
pub struct UserProc {
    bin: File,
    fd_table: Mutex<BTreeMap<usize, OpenFile>, Sleep>,
}// 每个用户进程独立持有 fd 表；Sleep 用于互斥保护
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}
```
`TaskStruct` 管理父子关系和退出同步；`Thread.task`、`Builder.task` 传递任务上下文；`UserProc` 管理 fd 表；`OpenFile` 和 `Mode` 保存文件访问权限。

> B2: Describe how file descriptors are associated with open files. Are file descriptors unique within the entire OS or just within a single process?

每个 `UserProc` 通过 `BTreeMap<usize, OpenFile>` 管理 fd。`open()` 从 3 开始分配最小空闲 fd，`close()` 删除对应条目；0、1、2 为标准描述符。fd 仅在进程内部唯一，不同进程可以使用相同的 fd 指向不同文件。

#### ALGORITHMS

> B3: Describe your code for reading and writing user data from the kernel.

1. 普通复制通过汇编 `lb` / `sb` 逐字节访问，由页故障处理程序将异常转换为错误。`copy_from_user()` 和 `copy_to_user()` 逐字节访问用户地址，通过 `checked_add()` 防止溢出，并借助汇编及页故障恢复机制处理非法指针。
2. `sys_read()` 在读取文件前使用 `user_range_writable()` 按页预检，避免无效缓冲区导致文件偏移意外前进。同时使用 512 字节内核缓冲区分块复制。
3. `execute()` 写入新进程栈时，新页表尚未激活，因此通过 `get_pte()` 找到物理页，再利用内核直接映射写入。

> B4: Suppose a system call causes a full page (4,096 bytes) of data to be copied from user space into the kernel.
> What is the least and the greatest possible number of inspections of the page table
> (e.g. calls to `Pagetable.get_pte(addr)` or other helper functions) that might result?
> What about for a system call that only copies 2 bytes of data?
> Is there room for improvement in these numbers, and how much?

当前 `copy_from_user()` 逐字节调用 `read_user_byte()`，由汇编 `lb` 直接读取地址。正常访问不查询 PTE；发生页故障时，异常处理程序查询一次 PTE，随后复制终止。因此，按显式软件页表查询次数统计：
| 复制长度 | 最少 | 最多 |
|---|---:|---:|
| 4096 字节 | 0 | 1 |
| 2 字节 | 0 | 1 |

当前实现的主要开销是逐字节函数调用。可以改为按页验证、批量复制：4096 字节或 2 字节均可能跨越 1～2 页，因此需要 1～2 次主动 PTE 检查，但能显著减少单字节访问开销。

> B5: Briefly describe your implementation of the "wait" system call and how it interacts with process termination.

父进程通过 `children` 持有子进程的 `Arc<TaskStruct>`，子进程以 `Weak` 引用父进程。`wait(tid)` 查找并移除直接子进程记录，再调用 `done.down()` 等待。`exit(status)` 保存退出码并执行 `done.up()` 唤醒父进程。重复等待或非法 tid 返回 `-1`。

> B6: Any access to user program memory at a user-specified address
> can fail due to a bad pointer value.  Such accesses must cause the
> process to be terminated.  System calls are fraught with such
> accesses, e.g. a "write" system call requires reading the system
> call number from the user stack, then each of the call's three
> arguments, then an arbitrary amount of user memory, and any of
> these can fail at any point.  This poses a design and
> error-handling problem: how do you best avoid obscuring the primary
> function of code in a morass of error-handling?  Furthermore, when
> an error is detected, how do you ensure that all temporarily
> allocated resources (locks, buffers, etc.) are freed?
> Have you used some features in Rust, to make these things easier than in C?
> In a few paragraphs, describe the strategy or strategies you adopted for
> managing these issues.  Give an example.

我将用户内存访问封装为 `read_user_cstr()`、`copy_from_user()` 和 `copy_to_user()`，集中处理非法指针和页故障，避免系统调用中重复错误处理代码。

Rust 的 `Option`、`Result`、`?` 和 `let ... else` 简化了错误传播；RAII 和 `Drop` 保证锁、缓冲区等资源在离开作用域时自动释放。例如 `copy_from_user()` 失败后，系统调用可直接进入错误路径，无需手动释放局部缓冲区。

对于 `thread::exit()` 这种不返回的函数，则在退出前显式释放局部 `Arc`。

> B7: Briefly describe what will happen if loading the new executable fails. (e.g. the file does not exist, is in the wrong format, or some other error.)

文件不存在时，`sys_exec()` 直接返回 `-1`。ELF 格式错误或加载失败时，`execute()` 销毁新页表并返回 `-1`。由于子线程和任务状态在加载成功后才创建，因此不会留下无效子进程。

#### SYNCHRONIZATION

> B8: Consider parent process P with child process C.  How do you
> ensure proper synchronization and avoid race conditions when P
> calls wait(C) before C exits?  After C exits?  How do you ensure
> that all resources are freed in each case?  How about when P
> terminates without waiting, before C exits?  After C exits?  Are
> there any special cases?

父子进程共享 C 的 `Arc<TaskStruct>`，通过 `Mutex<Option<isize>>` 保存退出码，使用初值为 0 的 `Semaphore` 同步。

若 P 先等待，`down()` 阻塞，直到 C 退出并调用 `up()`；若 C 先退出，信号量保留通知，P 随后可以直接取得状态。`wait()` 先从 `children` 移除记录，防止重复等待。若 P 未等待就退出，会清空 `children`，但 C 自己持有的 `Arc` 保证状态仍有效。最后一个强引用释放后资源自动回收，线程栈由调度器安全回收。

特殊情况包括非法 tid、重复等待、异常退出和子线程先于父进程登记运行。共享的 `Arc` 保证这些执行顺序下退出状态不会丢失。

#### RATIONALE

> B9: Why did you choose to implement access to user memory from the
> kernel in the way that you did?

针对不同场景，我采用三种用户内存访问机制，如B3 所述。这种设计实现简单、错误处理集中，但逐字节复制效率较低，未来可改为按页批量复制。

> B10: What advantages or disadvantages can you see to your design
> for file descriptors?

使用每进程独立的 `BTreeMap<usize, OpenFile>`，优点是结构简单、查找方便、能够复用最小空闲 fd，并集中管理访问权限。缺点是分配 fd 需要遍历表，单一 `Mutex` 也可能限制并发性能。后续可以使用空闲 fd 列表和更细粒度的锁优化。

> B11: What is your tid_t to pid_t mapping. What advantages or disadvantages can you see to your design?

采用 `pid_t = tid_t` 的一一映射，每个用户进程由一个内核线程表示，`execute()` 返回的 tid 同时作为进程标识。优点是无需额外的 pid 分配器和映射表；缺点是进程与线程标识耦合，将来支持多线程进程时需要重新设计。