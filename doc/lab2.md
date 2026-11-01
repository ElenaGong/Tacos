# Lab 2: User Programs

---

## Information

Name: 弓晓彤

Email: 2400013168@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.

> With any comments that may help TAs to evaluate your work better, please leave them here

## Argument Passing

#### DATA STRUCTURES

> A1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

参数传递本身没有专门新增结构体，主要复用内核已有的 `Frame`（保存用户态寄存器）和 `Vec<String>`（保存内核侧参数）。新增或使用的限制常量如下：

```rust
const MAX_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 3072;
```

`MAX_ARGS` 限制参数个数；`MAX_ARG_BYTES` 限制参数字符串占用的总字节数，避免过量参数。`execute()` 内的 `Vec<usize>` 用来临时记录每个字符串在新用户栈中的地址。

#### ALGORITHMS

> A2: Briefly describe how you implemented argument parsing. How do you arrange for the elements of argv[] to be in the right order? How do you avoid overflowing the stack page?

`sys_exec()` 首先从用户空间读取可执行文件路径和以空指针结尾的 `argv` 指针数组，把各个 C 字符串转换为内核中的 `String`，组成 `Vec<String>`，再调用 `execute(file, argv)`。

`execute()` 加载 ELF 并建立新进程页表后，从用户栈高地址向低地址依次写入参数字符串及结尾的 `\0`，同时按照原始顺序记录各字符串的用户虚拟地址。接着给指针数组预留空间，按 `argv[0]`、`argv[1]`……的顺序写入地址，最后放置 `NULL`。因此 `argv[0]`、`argv[1]`……能按照从低地址到高地址的顺序存放在栈上。

写栈时使用 `checked_sub()` 防止整数下溢，并检查栈指针没有越过已分配用户栈页的下界。指针数组和 sp 分别对齐到 8 和 16 字节，最终设置 `frame.x[2] = sp`、`frame.x[10] = argc`、`frame.x[11] = argv`。

#### RATIONALE

> A3: In Tacos, the kernel reads the executable name and arguments from the command. In Unix-like systems, the shell does this work. Identify at least two advantages of the Unix approach.

Unix 将命令解析放在 shell 中，有以下优点：

1. **职责分离：** 内核只需提供通用的进程创建与加载机制，无须理解引号、转义、通配符等 shell 语法。
2. **扩展灵活：** 不同 shell 可以自行实现变量展开、管道、重定向、命令替换等功能，而不需要修改内核。
3. **减少内核复杂度：** 命令解析逻辑在用户态运行，出错时更容易调试和更新，也减少需要由内核直接处理的复杂输入。

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

// 线程结构新增 task 字段，其余为既有字段
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
    pub task: Arc<TaskStruct>,
}

// 向新线程显式传递任务上下文
pub struct Builder {
    priority: u32,
    name: &'static str,
    function: usize,
    userproc: Option<UserProc>,
    pagetable: Option<PageTable>,
    task: Option<Arc<TaskStruct>>,
}

// 每个已打开的文件及其访问权限
pub struct OpenFile {
    file: File,
    readable: bool,
    writable: bool,
}

// 每个用户进程独立持有 fd 表；Sleep 用于互斥保护
pub struct UserProc {
    bin: File,
    fd_table: Mutex<BTreeMap<usize, OpenFile>, Sleep>,
}

// open 系统调用使用的访问模式
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}
```

`TaskStruct` 保存父子关系和退出同步信息；`Thread.task`、`Builder.task` 保证新线程获得任务上下文；`UserProc.fd_table` 管理进程内 fd 到文件的映射；`OpenFile` 保存权限；`Mode` 表示打开模式。

> B2: Describe how file descriptors are associated with open files. Are file descriptors unique within the entire OS or just within a single process?

每个 `UserProc` 持有一个受互斥锁保护的 `BTreeMap<usize, OpenFile>`。键是文件描述符 fd，值包含 `File` 对象及可读、可写权限。`open()` 成功时从 3 开始寻找最小空闲 fd，在同一次加锁操作中插入；`close()` 从表中删除相应条目，释放其文件引用。标准输入、输出和错误输出使用保留的 fd `0`、`1`、`2`。

fd 只在单个进程内部唯一，不同进程可以同时使用一个 fd ，但这个 fd 可以指向完全不同的文件。

#### ALGORITHMS

> B3: Describe your code for reading and writing user data from the kernel.

使用 `copy_from_user(user_src: *const u8, dst: &mut [u8])` 和 `copy_to_user(user_dst: *const u8, src: &[u8])` 集中处理用户空间与内核空间之间的数据复制。二者按地址逐字节访问，使用 `checked_add()` 避免地址计算溢出；底层 `read_user_byte()`、`write_user_byte()` 先拒绝内核空间地址，再通过带页故障恢复路径的汇编访问用户地址，出错时返回 `Result::Err`。

`sys_write()` 先把用户数据复制到固定大小（512字节）的内核缓冲区，再写入文件或控制台；`sys_read()` 则先从文件读取到内核缓冲区，再将实际读取的字节数 `n` 复制给用户。两者按照缓冲区长度分块，最终返回实际读写的字节数。

为了避免 `read()` 在目标地址明显无效时先推进文件偏移，在第一次文件读取前按页检查整个目标区间的用户可写性（`user_range_writable`）。

> B4: Suppose a system call causes a full page (4,096 bytes) of data to be copied from user space into the kernel. 
> What is the least and the greatest possible number of inspections of the page table 
> (e.g. calls to `Pagetable.get_pte(addr)` or other helper functions) that might result?
> What about for a system call that only copies 2 bytes of data?
> Is there room for improvement in these numbers, and how much?

在当前实现中，`copy_from_user()` 通过循环逐字节调用 `read_user_byte()`。每次首先使用 `in_kernel_space()` 排除内核地址，再通过汇编 `lb` 指令直接读取用户虚拟地址。正常读取时不会显式调用 `get_pte()`。

如果访问触发 Page Fault，页故障处理程序会调用 `get_pte()` 检查页表，并将非法访问转换为错误。`copy_from_user()` 随后通过 `?` 立即返回，不再继续复制。

因此，按照显式的软件页表查询次数统计：

| 复制长度 | 最少检查次数 | 最多检查次数 |
|---|---:|---:|
| 4096 字节 | 0 | 1 |
| 2 字节 | 0 | 1 |

- **最少 0 次**：所有地址有效，复制成功，不需要进入页故障处理程序。
- **最多 1 次**：某个地址发生页故障，检查一次 PTE 后返回错误，复制立即终止。

这里不包括 CPU MMU 的硬件地址翻译及 TLB 未命中产生的页表遍历。逐字节调用 `read_user_byte()` 也不等于逐字节执行软件页表检查。

优化空间：当前实现虽然没有大量重复的显式页表查询，但复制效率仍然较低。例如复制 4096 字节需要执行 4096 次单字节读取操作。后续可以改为按页检查、按块复制：

1. 计算用户缓冲区覆盖的虚拟页面。
2. 对每页调用一次 `get_pte()`，验证映射及访问权限。
3. 在通过验证的页面内批量复制字节。
4. 跨越页面边界时重新检查下一页。

改进后：

| 复制长度 | 最少 PTE 检查 | 最多 PTE 检查 |
|---|---:|---:|
| 4096 字节 | 1 | 2 |
| 2 字节 | 1 | 2 |

4096 字节在页对齐时只涉及一个页面，未对齐时涉及两个页面；2 字节也可能恰好跨越页面边界。

这种优化不会减少显式页表检查次数，但可以大幅减少逐字节访问的函数调用开销，提高大块内存复制的效率。

> B5: Briefly describe your implementation of the "wait" system call and how it interacts with process termination.

每个线程通过 `Arc<TaskStruct>` 持有任务状态，父任务的 `children` 存放子任务状态的强引用，子任务用 `Weak<TaskStruct>` 指向父任务。`execute()` 创建子任务，交给新线程，并在父任务 `children` 表中登记其 tid。

`wait(tid)` 首先检查 tid 是否属于当前任务的直接子进程，找到后从 `children` 中移除对应记录，保证同一子进程不能成功等待两次。随后对该子任务的 `Semaphore` 调用 `down()`。子进程 `exit(status)` 先把退出码写入 `exit_status`，释放互斥锁，再调用 `done.up()`；父进程被唤醒后读取退出码并返回。若 tid 不属于直接子进程或已被等待，返回 `None`，由系统调用层转换为 `-1`。

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

我将用户指针检查封装为少数辅助函数，避免在每个系统调用中重复写复杂的页故障处理逻辑。例如 `read_user_cstr()` 负责读取以 `\0` 结尾的用户字符串；`copy_from_user()` 和 `copy_to_user()` 负责字节复制。地址计算使用 `checked_add()`、`checked_sub()`；失败时统一进入错误路径。

Rust 的 `Option`、`Result`、`?`、`let ... else`、`match` 让错误可以逐层传递，同时 `MutexGuard` 离开作用域会自动释放锁，`Vec` 和 `Arc` 等资源也依赖 `Drop` 自动释放。比如先在内层代码块中修改 `exit_status`，退出该代码块释放锁，再执行 `done.up()`，避免父线程醒来后争夺仍由子线程持有的锁。对于不会正常返回的 `thread::exit()`，会显式 `drop()` 不再需要的局部强引用。

例子：`copy_from_user(addr, &mut buf).is_err()` 时，系统调用能够立即进入错误分支，局部缓冲区自动回收；文件表的锁则通过 RAII 在离开作用域时释放。

> B7: Briefly describe what will happen if loading the new executable fails. (e.g. the file does not exist, is in the wrong format, or some other error.)

如果路径不存在，`sys_exec()` 打开文件时失败并返回 `-1`，不会创建子线程。若文件存在但 ELF 格式错误、加载内存失败或其他加载错误，`execute()` 的 `load_executable()` 返回 `Err`；此时销毁尚未交由线程管理器管理的新页表，释放临时资源，并返回 `-1`。子进程的 `TaskStruct` 和线程创建放在加载成功之后，因此这类失败不会留下可等待的无效子进程。

#### SYNCHRONIZATION

> B8: Consider parent process P with child process C.  How do you
> ensure proper synchronization and avoid race conditions when P
> calls wait(C) before C exits?  After C exits?  How do you ensure
> that all resources are freed in each case?  How about when P
> terminates without waiting, before C exits?  After C exits?  Are
> there any special cases?

父进程 P 和子进程 C 共同持有 C 的 `Arc<TaskStruct>`，其中有 `exit_status: Mutex<Option<isize>>` 和初始计数为 0 的 `Semaphore`。

如果 P 先调用 `wait(C)`，它在 `done.down()` 阻塞；C 退出时先保存退出码、释放锁，再执行 `done.up()`，唤醒 P。如果 C 先退出，`done.up()` 将信号量计数加一，P 日后调用 `down()` 可以立即成功，不会丢失通知。P 在等待前就从 `children` 移除 C，因此重复 `wait()` 或其他非子进程 tid 会被拒绝。

若 P 不等待就退出，清空自己的 `children`，释放对各子任务的强引用，此时 C 仍通过自己的 `Arc` 保留任务状态，并可继续运行。若 C 已经退出，P 清空 `children` 后，多余状态在最后一个 `Arc` 释放时回收。若 P 在 C 退出之前结束，C 的退出也不依赖父进程仍然存活。实际 `Thread` 内核栈由线程管理器在安全的调度时机回收。

特殊情况包括：等待非直接子进程、重复等待、子进程异常终止（约定状态 `-1`），以及 `spawn()` 可能在父进程登记子任务之前就调度 C。最后一种情况下，只要父线程仍持有 `Arc<TaskStruct>`，且 C 退出只更新共享状态、不要求自己已在 `parent.children` 中，退出状态就不会丢失。

#### RATIONALE

> B9: Why did you choose to implement access to user memory from the
> kernel in the way that you did?

我根据用户内存访问的不同场景，采用了三种机制。

首先，普通数据复制采用逐字节探测。`copy_from_user()` 和 `copy_to_user()` 通过汇编指令 `lb` / `sb` 直接访问用户地址。如果发生缺页异常，`pagefault.rs` 会将 `sepc` 修改为预设的恢复地址，并返回错误码。这种方式无需预先遍历页表，能够统一处理跨页、未映射和权限不足等情况。

其次，`sys_read()` 在读取文件前使用 `user_range_writable()` 按页检查目标缓冲区的有效性和写权限，避免文件读取成功后因用户地址无效而复制失败，导致文件偏移意外前进。

最后，`execute()` 需要向尚未激活页表的新进程写入参数，因此使用 `copy_to_new_user()`，先通过 `get_pte()` 找到对应物理页，再利用内核的物理内存直接映射完成写入。

总体而言，这种设计针对不同场景选择了相对简单且可靠的访问方式，错误处理也较为集中。主要缺点是逐字节复制存在额外的函数调用开销，未来可以通过按页验证和批量复制进一步优化性能。

> B10: What advantages or disadvantages can you see to your design
> for file descriptors?

使用每进程独立的 `BTreeMap<usize, OpenFile>`，优势是实现简单、按 fd 查找直观，可以复用最小可用 fd；文件访问模式和文件对象放在同一条目中，方便 `read()`、`write()` 检查权限。`Mutex` 保护 fd 表以及通过 `&mut File` 修改的读写位置。

不足是每次分配最小 fd 需要遍历表；若整个表被一个锁保护，多个文件的操作可能互相等待。未来可以采用 `next_fd` 加空闲列表，或者对各打开文件设置独立锁，以提高扩展性。

> B11: What is your tid_t to pid_t mapping. What advantages or disadvantages can you see to your design?

当前设计采用 `pid_t = tid_t` 的一一映射。每个用户进程由一个拥有 `UserProc` 的内核线程表示，`execute()` 返回创建线程的 tid，并直接把它用作 `wait()` 的进程标识。

优点是不需要额外维护 pid 分配器或从 pid 到线程的全局映射，父进程可以通过子任务状态中的 tid 直接定位目标。缺点是进程标识与线程标识耦合，如果将来支持一个进程拥有多个用户线程，单纯以 tid 充当 pid 就不够合适，需要独立的进程 ID 和线程组管理。
