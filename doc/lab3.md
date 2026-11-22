## Lab 3: Virtual Memory

### Information

Name: 弓晓彤

Email: 2400013168@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.

> With any comments that may help TAs to evaluate your work better, please leave them here

### Stack Growth

##### ALGORITHMS

> A1: Explain your heuristic for deciding whether a page fault for an invalid virtual address should cause the stack to be extended into the page that faulted.

用户态 Load/Store Page Fault 满足以下条件时，分配并映射新的栈页：

1. 缺页地址 `addr >= sp`，且 SP 位于合法栈范围内。
2. 目标页低于 `stack_bottom`，且不低于 `STACK_LIMIT`。
3. 待增长区间不与已有或预留的 ELF、mmap 等页面冲突。

### Memory Mapped Files

##### DATA STRUCTURES

> B1: Copy here the declaration of each new or changed struct or struct member, global or static variable, typedef, or enumeration.  Identify the purpose of each in 25 words or less.

`OpenFile`（`src/userproc.rs`）：保存文件句柄及读写权限，可独立克隆供映射持有。

```rust
#[derive(Clone)]
pub struct OpenFile {
    pub file: File,
    pub readable: bool,
    pub writable: bool,
}
```

`Mapping`（`src/mem/mmap.rs`）：记录一次 mmap 的文件、虚拟地址范围和权限。

```rust
#[derive(Clone)]
pub struct Mapping {
    pub openfile: OpenFile,
    pub start: usize,
    pub length: usize,
    pub flags: PTEFlags,
}
```

`FaultAccess`（`src/mem/mmap.rs`）：区分读、写和取指，用于权限检查。

```rust
pub enum FaultAccess {
    Read,
    Write,
    Execute,
}
```

`UserProc` 中的 `mappings` 按 mapid 管理映射，`next_mapid` 分配映射编号。完整声明见 C1。

##### ALGORITHMS

> B2: Describe how memory mapped files integrate into your virtual memory subsystem.  Explain how the page fault and eviction processes differ between swap pages and other pages.

mmap 将文件区域登记到 `Mapping`，并在 SPT 中记录每页的文件偏移、读取长度和权限，暂不分配物理页。

缺页时检查权限，`Unloaded` 页从文件加载，`Swapped` 页从 swap 换入，再建立 PTE 并更新为 `Resident`。驱逐时，mmap 脏页写回原文件；ELF 脏页和栈页写入 swap；可从文件恢复的干净页直接丢弃。

> B3: Explain how you determine whether a new file mapping overlaps any existing segment.

先将文件长度向上取整到整页，计算映射区间并检查地址合法性和溢出。再由 `not_mapped()` 检查已有 mmap 区间、SPT 条目和有效 PTE，发现重叠就拒绝映射。SPT 检查也覆盖尚未加载的页面。

##### RATIONALE

> B4: Mappings created with "mmap" have similar semantics to those of data demand-paged from executables, except that "mmap" mappings are written back to their original files, not to swap.  This implies that much of their implementation can be shared.  Explain why your implementation either does or does not share much of the code for the two situations.

ELF 和 mmap 共用 SPT 和懒加载函数，统一完成分配、读取和映射，只按元数据选择源文件。驱逐时分别处理：mmap 脏页写回原文件，ELF 私有修改页写入 swap。

### Page Table Management

##### DATA STRUCTURES

> C1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

`UserProc`（`src/userproc.rs`）：保存进程的文件、SPT、映射表及栈信息；`user_sp` 供系统调用期间判断栈增长。

```rust
pub struct UserProc {
    #[allow(dead_code)]
    bin: File,
    fd_table: Mutex<BTreeMap<usize, OpenFile>, Sleep>,
    spt: Mutex<SupplementalPageTable, Sleep>,
    mappings: Mutex<BTreeMap<usize, Mapping>, Sleep>,
    next_mapid: AtomicUsize,
    pub stack_bottom: AtomicUsize,
    pub user_sp: AtomicUsize,
}
```

`ElfChunk`（`src/mem/spt.rs`）：记录 ELF 文件片段的来源偏移、页内位置和长度。

```rust
#[derive(Clone)]
pub struct ElfChunk {
    pub file_offset: usize,
    pub page_offset: usize,
    pub len: usize,
}
```

`PageMeta`（`src/mem/spt.rs`）：区分栈、mmap 和 ELF 页面，并记录数据来源。

```rust
#[derive(Clone)]
pub enum PageMeta {
    Stack,
    Mmap {
        mapid: usize,
        file_offset: usize,
        read_bytes: usize,
    },
    Elf {
        chunks: Vec<ElfChunk>,
    },
}
```

`PageState`（`src/mem/spt.rs`）：记录未加载、已驻留或已换出状态，以及 swap slot。

```rust
#[derive(Clone, Copy)]
pub enum PageState {
    Unloaded,
    Resident,
    Swapped { slot: usize },
}
```

`SptEntry`（`src/mem/spt.rs`）：组合页面来源、状态和访问权限。

```rust
#[derive(Clone)]
pub struct SptEntry {
    pub meta: PageMeta,
    pub state: PageState,
    pub flags: PTEFlags,
}
```

`SupplementalPageTable`（`src/mem/spt.rs`）：按虚拟页地址保存和查询页面元数据。

```rust
#[derive(Clone)]
pub struct SupplementalPageTable {
    entries: BTreeMap<usize, SptEntry>,
}
```

`ExecInfo`（`src/userproc/load.rs`）：返回程序入口、初始 SP 和栈物理页，供创建进程及登记 Frame 使用。

```rust
#[derive(Debug, Clone, Copy)]
pub(super) struct ExecInfo {
    pub entry_point: usize,
    pub init_sp: usize,
    pub stack_pa: PhysAddr,
}
```

`FaultError`（`src/trap/pagefault.rs`）：区分非法访问和页面加载失败。

```rust
enum FaultError {
    InvalidAccess,
    LoadFailed,
}
```

##### ALGORITHMS

> C2: In a few paragraphs, describe your code for accessing the data stored in the Supplementary page table about a given page.

每个进程用 `BTreeMap` 保存 SPT，以页对齐的虚拟地址查询。`SptEntry` 记录权限、来源和状态，据此检查访问并加载页面。成功后更新 PTE 和 SPT 状态。

> C3: How does your code coordinate accessed and dirty bits between kernel and user virtual addresses that alias a single frame, or alternatively how do you avoid the issue?

页面置换读取用户 PTE 的 A、D 位。系统调用通过用户虚拟地址拷贝数据，因此也会更新这些位。内核直接映射用于初始化和页面 I/O，不计入用户访问统计。

从 swap 换入时设置 D 位，保留此前的修改信息；栈页驱逐时始终保存到 swap。

##### SYNCHRONIZATION

> C4: When two user processes both need a new frame at the same time, how are races avoided?

全局 `VM_LOCK` 串行化 Frame 分配和驱逐，分配器及 Frame Table 另有内部锁。加载中的新页保持 pinned，完成内容和元数据更新后才允许驱逐。

##### RATIONALE

> C5: Why did you choose the data structure(s) that you did for representing virtual-to-physical mappings?

Sv39 三级页表负责 VA 到 PA 的硬件转换，SPT 负责记录页面来源、权限和驻留状态。SPT 使用 `BTreeMap`，适合稀疏地址空间，只为已登记的虚拟页保存元数据，并支持按地址查找。

### Paging To And From Disk

##### DATA STRUCTURES

> D1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

`FrameEntry`（`src/mem/frame.rs`）：记录物理页、所属线程、用户虚拟地址及 pin 状态。

```rust
pub struct FrameEntry {
    pub pa: PhysAddr,
    pub owner: Weak<Thread>,
    pub va: usize,
    pub pinned: bool,
}
```

`FrameTable`（`src/mem/frame.rs`）：保存 Frame 记录和 Clock 扫描位置。

```rust
pub struct FrameTable {
    frames: Vec<FrameEntry>,
    clock_hand: usize,
}
```

`FRAME_TABLE`（`src/mem/frame.rs`）：全局 Frame Table，由睡眠锁保护。

```rust
static FRAME_TABLE: Lazy<Mutex<FrameTable, Sleep>> = Lazy::new(|| Mutex::new(FrameTable::new()));
```

`VM_LOCK`（`src/mem/frame.rs`）：串行化跨进程 VM 操作，持锁期间允许等待 I/O。

```rust
pub static VM_LOCK: Lazy<Mutex<(), Sleep>> = Lazy::new(|| Mutex::new(()));
```

`Swap`（`src/fs/disk/swap.rs`）：提供 swap slot 的分配、读写和释放接口。

```rust
pub struct Swap;
```

`SwapFileGuard`（`src/fs/disk/swap.rs`）：持锁访问已初始化的 swap 文件，离开作用域时释放锁。

```rust
pub struct SwapFileGuard(MutexGuard<'static, Option<File>, Primitive>);
```

`SWAPFILE`（`src/fs/disk/swap.rs`）：保存延迟打开的 swap 文件，并保护文件位置和 I/O。

```rust
static SWAPFILE: Lazy<Mutex<Option<File>>> = Lazy::new(|| Mutex::new(None));
```

`FREE_SLOTS`（`src/fs/disk/swap.rs`）：保存空闲 swap slot 编号，分配时弹出，释放时放回。

```rust
static FREE_SLOTS: Lazy<Mutex<Option<Vec<usize>>>> = Lazy::new(|| Mutex::new(None));
```

页面的换出状态和 slot 记录在 `PageState` 中，声明见 C1。

##### ALGORITHMS

> D2: When a frame is required but none is free, some frame must be evicted. Describe your code for choosing a frame to evict.

我使用全局 Frame Table 和 Clock 算法。分配失败后，从 `clock_hand` 扫描，跳过 pinned 页和已退出进程的记录。A 位为 1 时清零并给予第二次机会，为 0 时选为牺牲页。

每次最多扫描两轮。驱逐成功后重试分配，找不到牺牲页或达到重试上限则返回失败。

> D3: When a process P obtains a frame that was previously used by a process Q, how do you adjust the page table (and any other data structures) to reflect the frame Q no longer has?

先使 Q 的 PTE 失效并刷新 TLB，完成写回后更新 SPT 状态，回收物理页并删除旧 Frame 记录。写回失败则恢复映射。

P 分配该页后登记新的归属，加载内容并建立 PTE，将 SPT 状态设为 `Resident`。

##### SYNCHRONIZATION

> D5: Explain the basics of your VM synchronization design. In particular, explain how it prevents deadlock. (Refer to the textbook for an explanation of the necessary conditions for deadlock.)

全局 `VM_LOCK` 串行化 VM 操作，内部锁用完即释放。I/O 不持有页表或 Frame Table 锁，也不在持有页表锁时申请 Frame。

系统调用通过内核缓冲区分开用户内存拷贝和文件 I/O，避免持有文件锁时触发页面加载。这些约定避免换入、换出之间形成循环等待。

> D6: A page fault in process P can cause another process Q's frame to be evicted.  How do you ensure that Q cannot access or modify the page during the eviction process?  How do you avoid a race between P evicting Q's frame and Q faulting the page back in?

P 持有 `VM_LOCK`，将牺牲页 pin，使 Q 的 PTE 失效并刷新 TLB，再执行写回。当前单核环境下，Q 再访问该页会缺页，并等待同一把锁。

P 完成驱逐后，Q 重新检查状态并加载；若驱逐失败，P 恢复原 PTE 并解除 pin。

> D7: Suppose a page fault in process P causes a page to be read from the file system or swap.  How do you ensure that a second process Q cannot interfere by e.g. attempting to evict the frame while it is still being read in?

新 Frame 在读取文件或 swap 前已被 pin，Clock 会跳过它。完成读取和 PTE、SPT 更新后才解除 pin；失败则回收。整个过程还持有 `VM_LOCK`，其他进程的换入、换出需要等待。

> D8: Explain how you handle access to paged-out pages that occur during system calls. Do you use page faults to bring in pages (as in user programs), or do you have a mechanism for "locking" frames into physical memory, or do you use some other design?  How do you gracefully handle attempted accesses to invalid virtual addresses?

系统调用逐页检查权限并主动加载或换入，栈增长使用保存的用户 SP。

缓冲区不长期 pin。拷贝时持有页表 `Intr` 锁并复查 PTE，失效则重试；当前单核环境下，关闭中断可防止拷贝期间被驱逐。文件 I/O 使用内核缓冲区。

非法地址、权限错误或加载失败返回错误。底层拷贝若仍缺页，handler 转到恢复位置，返回 `BadPtr`。

##### RATIONALE

> D9: A single lock for the whole VM system would make synchronization easy, but limit parallelism. On the other hand, using many locks complicates synchronization and raises the possibility for deadlock but allows for high parallelism. Explain where your design falls along this continuum and why you chose to design it this way.

我采用全局 `VM_LOCK` 加内部结构锁，整体偏向粗粒度同步。它简化了跨进程驱逐和状态更新，便于实现、调试。

代价是页面 I/O 期间，其他进程的 VM 操作也要等待；已驻留页面仍可正常访问。
