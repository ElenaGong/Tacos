//! User process.
//!

mod load;

use crate::fs::disk::Swap;
use crate::fs::File;
use crate::io::{Read, Seek, SeekFrom, Write};
use crate::mem::frame::{alloc_frame, free_frame, insert_frame, remove_frames_of, unpin_frame};
use crate::mem::mmap::Mapping;
use crate::mem::pagetable::{KernelPgTable, PageTable};
use crate::mem::spt::{PageMeta, PageState, SptEntry, SupplementalPageTable};
use crate::mem::{PTEFlags, PageAlign, PG_SIZE};
use crate::sbi::shutdown;
use crate::sync::Intr;
use crate::sync::Mutex;
use crate::sync::Sleep;
use crate::thread::{self, current, TaskStruct, STACK_TOP};
use crate::trap::{trap_exit_u, Frame};

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::asm;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};
use riscv::register::sstatus;

#[derive(Clone)]
pub struct OpenFile {
    pub file: File,
    pub readable: bool,
    pub writable: bool,
}

impl OpenFile {
    pub fn new(file: File, readable: bool, writable: bool) -> Self {
        Self {
            file,
            readable,
            writable,
        }
    }
}

/// 从指定文件偏移读取到目标缓冲区
fn read_file_part(file: &mut File, offset: usize, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return true;
    }

    file.seek(SeekFrom::Start(offset)).is_ok() && file.read_exact(dst).is_ok()
}

/// 记录被mmap的页的元数据
// #[derive(Clone)]
// pub struct MmapPage {
//     mapid: usize,       // 由mapid能找到Mapping
//     file_offset: usize, // 找到该页从文件中的什么地方开始
//     read_bytes: usize,  // 如果等于PG_SIZE就为一整页；小于就用0补齐
// }

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

impl UserProc {
    pub fn new(file: File, spt: SupplementalPageTable) -> Self {
        Self {
            bin: file,
            fd_table: Mutex::new(BTreeMap::new()),
            spt: Mutex::new(spt.clone()),
            mappings: Mutex::new(BTreeMap::new()),
            next_mapid: AtomicUsize::new(1),
            stack_bottom: AtomicUsize::new(STACK_TOP - PG_SIZE),
            user_sp: AtomicUsize::new(STACK_TOP),
        }
    }

    /// 返回 fd 对应文件的 (inode 号, 字节长度)，供 fstat 使用。
    pub fn get_fstat(&self, fd: usize) -> Option<(usize, usize)> {
        let table = self.fd_table.lock();
        let entry = table.get(&fd)?;
        Some((entry.file.inum(), entry.file.len().ok()?))
    }
    /// # alloc_and_insert
    /// 1. 找到最小的空闲文件描述符，从 3 开始
    /// 2. 分配描述符并插入文件
    /// 3. 返回分配的描述符
    /// * 分配描述符和插入文件放在一个函数里是为了避免两次加锁
    pub fn alloc_and_insert_fd(&self, file: OpenFile) -> Option<usize> {
        let mut candidate: usize = 3;
        let mut fd_table = self.fd_table.lock();

        for &fd in fd_table.keys() {
            if fd < candidate {
                continue;
            }

            if fd > candidate {
                break;
            }

            candidate = candidate.checked_add(1)?;

            // 上限
            if candidate >= 1024 {
                return None;
            }
        }
        fd_table.insert(candidate, file);

        Some(candidate)
    }
    /// # close_file
    /// 关闭给定描述符的文件，返回成功/失败
    pub fn close_file(&self, fd: usize) -> bool {
        if fd < 3 {
            return false;
        }
        let file: Option<OpenFile> = self.fd_table.lock().remove(&fd);

        match file {
            Some(file) => {
                drop(file);
                true
            }
            None => false,
        }
    }
    pub fn read_fd(&self, fd: usize, buf: &mut [u8]) -> Option<usize> {
        let mut table = self.fd_table.lock();
        let entry = table.get_mut(&fd)?;

        if !entry.readable {
            return None;
        }

        entry.file.read(buf).ok()
    }

    pub fn write_fd(&self, fd: usize, buf: &[u8]) -> Option<usize> {
        let mut table = self.fd_table.lock();
        let entry = table.get_mut(&fd)?;

        if !entry.writable {
            return None;
        }

        entry.file.write(buf).ok()
    }

    pub fn seek_fd(&self, fd: usize, position: usize) -> bool {
        let mut table = self.fd_table.lock();
        let Some(entry) = table.get_mut(&fd) else {
            return false;
        };
        entry.file.seek(SeekFrom::Start(position)).is_ok()
    }
    pub fn tell_fd(&self, fd: usize) -> Option<usize> {
        let mut table = self.fd_table.lock();
        let entry = table.get_mut(&fd)?;

        entry.file.stream_position().ok()
    }

    /// 进程退出时关闭所有打开的文件
    pub fn close_all_files(&self) {
        let files = {
            let mut table = self.fd_table.lock();
            core::mem::take(&mut *table)
        };

        drop(files);
    }

    pub fn not_mapped(&self, start: usize, end: usize) -> bool {
        // 与已有 mmap 区间检查冲突
        {
            let maps = self.mappings.lock();
            for mapping in maps.values() {
                let pages = mapping.length / PG_SIZE + usize::from(mapping.length % PG_SIZE != 0);
                let old_end = mapping.start + pages * PG_SIZE;

                if start < old_end && mapping.start < end {
                    return false;
                }
            }
        }

        // 与 SPT 中已登记的页面检查冲突（ELF 段、mmap 页、栈页）
        {
            let pages = self.spt.lock();
            let mut va = start;
            while va < end {
                if pages.get(va).is_some() {
                    return false;
                }
                va += PG_SIZE;
            }
        }

        // 与已存在的页表映射检查冲突（例如尚未登记进 SPT 的已增长栈页）
        let thread = current();
        let Some(pt) = thread.pagetable.as_ref() else {
            return false;
        };
        let table = pt.lock();

        let mut va = start;
        while va < end {
            if table.get_pte(va).is_some_and(|e| e.is_valid()) {
                return false;
            }
            va += PG_SIZE;
        }
        drop(table);

        true
    }

    /// 把一页增长出来的栈页登记进 SPT（页面已经驻留内存）
    pub fn register_stack_page(&self, va: usize) -> bool {
        let entry = SptEntry {
            meta: PageMeta::Stack,
            state: PageState::Resident,
            flags: PTEFlags::V | PTEFlags::R | PTEFlags::W | PTEFlags::U,
        };
        let mut pages = self.spt.lock();
        pages.insert(va, entry)
    }

    pub fn find_page(&self, va: usize) -> Option<SptEntry> {
        let pages = self.spt.lock();
        let Some(entry) = pages.get(va) else {
            return None;
        };
        Some(entry.clone())
    }

    /// 更新一页的 SPT 状态
    pub fn set_page_state(&self, va: usize, state: PageState) -> bool {
        let mut pages = self.spt.lock();
        let Some(entry) = pages.get_mut(va) else {
            return false;
        };
        entry.state = state;
        true
    }

    /// 回收所有处于 Swapped 状态的页面的 swap slot（进程退出时调用）
    pub fn release_swap_slots(&self) {
        let snapshot = {
            let pages = self.spt.lock();
            pages.snapshot()
        };
        for entry in snapshot {
            if let PageState::Swapped { slot } = entry.state {
                Swap::free_slot(slot);
            }
        }
    }

    pub fn find_mapping(&self, va: usize) -> Option<Mapping> {
        let Some(entry) = self.find_page(va) else {
            return None;
        };
        let mapid = {
            match entry.meta {
                PageMeta::Mmap { mapid, .. } => mapid,
                _ => {
                    return None;
                }
            }
        };

        let maps = self.mappings.lock();
        let Some(mapping) = maps.get(&mapid) else {
            return None;
        };
        Some(mapping.clone())
    }

    /// mmap
    pub fn mmap(&self, fd: usize, addr: usize) -> isize {
        let _vm_guard = crate::mem::frame::VM_LOCK.lock();
        if fd < 3 || addr == 0 || !addr.is_aligned() {
            return -1;
        }
        let table = self.fd_table.lock();
        let Some(entry) = table.get(&fd) else {
            return -1;
        };

        let (file, file_len) = {
            let Ok(len) = entry.file.len() else {
                return -1;
            };
            // 复制文件句柄既能让映射在原 fd 关闭后继续有效，
            // 也能避免 mmap 懒加载时改变原 fd 的读写位置
            (entry.clone(), len)
        };
        if file_len == 0 {
            return -1;
        }

        // 计算end地址
        let n_pages = file_len / PG_SIZE + usize::from(file_len % PG_SIZE != 0);
        let Some(map_size) = n_pages.checked_mul(PG_SIZE) else {
            return -1;
        };
        let Some(end) = addr.checked_add(map_size) else {
            return -1;
        };

        // 必须位于 Sv39 用户低半区
        const USER_VA_END: usize = 1usize << 38;
        if end > USER_VA_END {
            return -1;
        }

        // 与已有 mmap 区间检查冲突
        // 与已存在的页表映射检查冲突
        if !self.not_mapped(addr, end) {
            return -1;
        }

        // 分配mapid
        let mut maps = self.mappings.lock();
        let mapid = self.next_mapid.load(Ordering::Relaxed);
        if mapid == 0 || mapid > isize::MAX as usize {
            return -1;
        }

        // 登记 Mapping（lazy）
        let flag_r = match file.readable {
            true => PTEFlags::R,
            false => PTEFlags::empty(),
        };
        let flag_w = match file.writable {
            true => PTEFlags::W,
            false => PTEFlags::empty(),
        };
        let flags = flag_r | flag_w | PTEFlags::U;
        maps.insert(
            mapid,
            Mapping {
                openfile: file,
                start: addr,
                length: file_len,
                flags: flags,
                // 暂时不设为valid是因为懒加载；load时需要补上
            },
        );

        drop(maps);

        // 为每个被映射的虚拟页保存元数据
        let mut pg_number = 0;
        let mut va = addr;
        let mut file_offset = 0;

        let mut pages = self.spt.lock();

        while pg_number < n_pages {
            let mut read_bytes = PG_SIZE;
            if pg_number == n_pages - 1 {
                read_bytes = file_len - PG_SIZE * (n_pages - 1);
            }
            pages.insert(
                va,
                SptEntry {
                    meta: PageMeta::Mmap {
                        mapid: mapid,
                        file_offset: file_offset,
                        read_bytes: read_bytes,
                    },
                    state: PageState::Unloaded,
                    flags: flags,
                },
            );

            pg_number += 1;
            va += PG_SIZE;
            file_offset += PG_SIZE;
        }

        drop(pages);

        // 更新next_mapid
        self.next_mapid.store(mapid + 1, Ordering::Relaxed);

        mapid as isize
    }

    pub fn lazily_load_page(&self, addr: usize, pt: &Mutex<PageTable, Intr>) -> bool {
        let _vm_guard = crate::mem::frame::VM_LOCK.lock();
        let page_va = addr.floor();

        let pg_table = pt.lock();
        if let Some(entry) = pg_table.get_pte(page_va) {
            if entry.is_valid() {
                return true; //应该true还是false？
            }
        }
        drop(pg_table);

        // 取出该页的元数据（克隆一份，避免在文件 I/O 期间持有 spt 锁）
        let spt_entry = {
            let pages = self.spt.lock();
            let Some(entry) = pages.get(page_va) else {
                return false;
            };
            entry.clone()
        };

        // 判断页面来源：Unloaded 从文件读取，Swapped 从 swap 换入
        let swapped = match spt_entry.state {
            PageState::Unloaded => None,
            PageState::Swapped { slot } => Some(slot),
            PageState::Resident => return false,
        };

        // 分配物理页（统一登记进 Frame Table）
        let Some(pa) = alloc_frame(current(), page_va) else {
            return false;
        };
        let page = unsafe { (pa.into_va() as *mut [u8; PG_SIZE]).as_mut().unwrap() };

        let load_ok = match swapped {
            Some(slot) => {
                // 从 swap 读回整页
                Swap::read_page(slot, page)
            }
            None => {
                // 先清零，再从后备存储读取
                page.fill(0);
                match &spt_entry.meta {
                    PageMeta::Mmap {
                        mapid,
                        file_offset,
                        read_bytes,
                    } => {
                        let file = {
                            let mappings = self.mappings.lock();
                            mappings.get(mapid).map(|m| m.openfile.file.clone())
                        };

                        if *read_bytes > PG_SIZE {
                            false
                        } else if let Some(mut file) = file {
                            read_file_part(&mut file, *file_offset, &mut page[..*read_bytes])
                        } else {
                            false
                        }
                    }
                    PageMeta::Elf { chunks } => {
                        // 从 userproc.bin 中分别读取各个片段，写入各自的页内位置
                        let mut file = self.bin.clone();
                        let mut elf_load_ok = true;
                        for chunk in chunks {
                            let end = chunk.page_offset + chunk.len;
                            if end > PG_SIZE {
                                elf_load_ok = false;
                                break;
                            }
                            if !read_file_part(
                                &mut file,
                                chunk.file_offset,
                                &mut page[chunk.page_offset..end],
                            ) {
                                elf_load_ok = false;
                                break;
                            }
                        }
                        elf_load_ok
                    }
                    PageMeta::Stack => false,
                }
            }
        };

        if !load_ok {
            free_frame(pa);
            return false;
        }

        // 换入成功后释放不再需要的 swap slot
        if let Some(slot) = swapped {
            Swap::free_slot(slot);
        }

        // 对页表加锁并更新为 valid（不能在关闭中断时进行 I/O，因此在这里统一加锁）
        // 若页面来自 swap，说明内容已不同于原后备存储，置 D 位保留脏信息。
        let mut pg_table = pt.lock();
        let map_flags = if swapped.is_some() {
            spt_entry.flags | PTEFlags::V | PTEFlags::D
        } else {
            spt_entry.flags | PTEFlags::V
        };
        pg_table.map(pa, page_va, PG_SIZE, map_flags);
        pg_table.activate();
        drop(pg_table);

        // 页面已经驻留内存，更新 SPT 状态
        self.set_page_state(page_va, PageState::Resident);

        // 加载完成，解除 pin，允许后续驱逐
        unpin_frame(pa);

        true
    }
    pub fn munmap(&self, mapid: usize, pt: &Mutex<PageTable, Intr>) -> bool {
        let _vm_guard = crate::mem::frame::VM_LOCK.lock();
        let mapping = {
            let mappings = self.mappings.lock();

            let Some(mapping) = mappings.get(&mapid) else {
                return false;
            };

            mapping.clone()
        }; // 锁在这里释放

        let mut file = mapping.openfile.file;

        let mut va = mapping.start;
        let n_pages = (mapping.length / PG_SIZE) + usize::from(mapping.length % PG_SIZE != 0);

        let mut writeback_ok = true;

        // 对mapping的每一页，检查是否有PTE，
        for _i in 0..n_pages {
            let Some(page) = self.find_page(va) else {
                return false;
            };
            let (file_offset, write_bytes) = match page.meta {
                PageMeta::Mmap {
                    file_offset,
                    read_bytes,
                    ..
                } => (file_offset, read_bytes),
                _ => return false,
            };

            let resident = {
                let mut pg_table = pt.lock();

                let state = match pg_table.get_pte_mut(va) {
                    Some(pte) if pte.is_valid() => {
                        // mmap 的有效 PTE 应当是用户叶子项
                        assert!(pte.is_leaf() && pte.is_user());

                        let pa = pte.pa();
                        let kernel_va = pa.into_va();
                        let dirty = pte.is_dirty();

                        // 解除 VA -> PA 映射
                        pte.set_invalid();

                        Some((pa, kernel_va, dirty))
                    }
                    _ => None, // 尚未 Lazy Loaded
                };

                if state.is_some() {
                    pg_table.activate(); // 刷新本 hart 的 TLB
                }

                state
            }; // 这里释放 PageTable 锁

            // 只处理已驻留的物理页
            if let Some((pa, kernel_va, dirty)) = resident {
                if dirty {
                    // 内核直接映射：物理页仍然有效
                    let buf =
                        unsafe { core::slice::from_raw_parts(kernel_va as *const u8, write_bytes) };

                    // 文件 I/O 不持有 PageTable 锁
                    let result = file
                        .seek(SeekFrom::Start(file_offset))
                        .and_then(|_| file.write_all(buf));

                    if result.is_err() {
                        writeback_ok = false;
                    }
                }

                // 即使写回失败，也继续回收资源
                free_frame(pa);
            }

            // 从 SPT 中清除页面元数据
            let mut pages = self.spt.lock();
            pages.remove(va);

            va += PG_SIZE;
        }

        // 页面已经回收，无论写回是否成功都移除 Mapping；
        // writeback_ok 仅作为本次写回是否成功的返回值。
        let mut mappings = self.mappings.lock();
        mappings.remove(&mapid);

        writeback_ok
    }
}

/// 将内核数据写入指定页表对应的用户虚拟地址。
fn copy_to_new_user(pt: &PageTable, user_dst: usize, src: &[u8]) -> Option<()> {
    // 检查地址范围是否溢出
    user_dst.checked_add(src.len())?;

    let mut offset = 0;

    while offset < src.len() {
        let va = user_dst.checked_add(offset)?;

        // 查询新进程页表
        let pte = pt.get_pte(va)?;

        // 必须是有效、用户可写的数据页
        if !pte.is_valid() || !pte.is_leaf() || !pte.is_user() || !pte.is_rwable() {
            return None;
        }

        // 计算页内偏移和本次复制长度
        let page_offset = va & (PG_SIZE - 1);
        let n = (PG_SIZE - page_offset).min(src.len() - offset);

        // 物理页转成内核可以访问的虚拟地址
        let kernel_addr = pte.pa().into_va().checked_add(page_offset)?;

        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr().add(offset), kernel_addr as *mut u8, n);
        }

        offset += n;
    }

    Some(())
}

/// Execute an object file with arguments.
///
/// ## Return
/// - `-1`: On error.
/// - `tid`: Tid of the newly spawned thread.
#[allow(unused_variables)]
pub fn execute(mut file: File, argv: Vec<String>) -> isize {
    #[cfg(feature = "debug")]
    kprintln!(
        "[PROCESS] Kernel thread {} prepare to execute a process with args {:?}",
        thread::current().name(),
        argv
    );

    // It only copies L2 pagetable. This approach allows the new thread
    // to access kernel code and data during syscall without the need to
    // switch pagetables.
    let mut pt = KernelPgTable::clone();
    let mut spt = SupplementalPageTable::new();

    let exec_info = match load::load_executable(&mut file, &mut pt, &mut spt) {
        Ok(x) => x,
        Err(_) => unsafe {
            pt.destroy();
            return -1;
        },
    };

    // Initialize frame, pass argument to user.
    let mut frame = unsafe { MaybeUninit::<Frame>::zeroed().assume_init() };
    frame.sepc = exec_info.entry_point;
    frame.x[2] = exec_info.init_sp;

    // Here the new process will be created.
    let stack_top = exec_info.init_sp;
    let stack_bottom = stack_top - PG_SIZE;

    let spt_entry = SptEntry {
        meta: PageMeta::Stack,
        state: PageState::Resident,
        flags: PTEFlags::V | PTEFlags::R | PTEFlags::W | PTEFlags::U,
    };
    if !spt.insert(stack_bottom, spt_entry) {
        return -1;
    };

    let userproc = UserProc::new(file, spt);

    // ---------- (Lab2) Pass arguments to user program ------------
    let mut cursor = stack_top;

    // 把单词推到栈上，记录每个词的首地址指针
    let mut addrs: Vec<usize> = Vec::new();

    let stack_result = (|| -> Option<()> {
        for s in argv.iter() {
            // C字符串中不能出现内部的 NUL
            if s.as_bytes().contains(&0) {
                return None;
            }
            let len = s.len().checked_add(1)?;
            cursor = cursor.checked_sub(len)?;

            // 防止越界
            if cursor < stack_bottom {
                return None;
            }

            copy_to_new_user(&pt, cursor, s.as_bytes())?;
            copy_to_new_user(&pt, cursor + s.len(), &[0u8])?;

            addrs.push(cursor);
        }
        // 按 8 字节对齐
        cursor &= !7usize;

        // 为 argv 指针数组预留空间
        // let pointer_count = argv.len().checked_add(1)?;
        // let pointer_bytes = pointer_count.checked_mul(8)?;
        cursor -= 8 * (argv.len() + 1);

        // argv[0] 的用户虚拟地址
        let argv_ptr = cursor;

        // 按 argv 原来的顺序写入指针
        for (i, &addr) in addrs.iter().enumerate() {
            let offset = i.checked_mul(core::mem::size_of::<usize>())?;
            let slot = argv_ptr.checked_add(offset)?;
            copy_to_new_user(&pt, slot, &addr.to_ne_bytes())?;
        }
        // 写入 argv[argc] = NULL
        let null_slot =
            argv_ptr.checked_add(argv.len().checked_mul(core::mem::size_of::<usize>())?)?;

        copy_to_new_user(&pt, null_slot, &0usize.to_ne_bytes())?;

        // 最终栈指针对齐到16字节
        cursor &= !15usize;
        if cursor < stack_bottom {
            return None;
        }

        // 设置用户态寄存器
        frame.x[2] = cursor; // sp
        frame.x[10] = argv.len(); // a0 = argc
        frame.x[11] = argv_ptr; // a1 = argv
        Some(())
    })();

    if stack_result.is_none() {
        unsafe {
            pt.destroy();
        }
        return -1;
    }

    // 建立父子任务关系
    let parent = Arc::clone(&current().task);
    let child = Arc::new(TaskStruct::child(&parent));
    let child_for_thread = Arc::clone(&child);

    let new_thread = thread::Builder::new(move || start(frame))
        .pagetable(pt)
        .userproc(userproc)
        .task(child_for_thread)
        .spawn();

    let tid = new_thread.id();

    // 登记初始栈页的 Frame（init_user_stack 已急切分配并映射）
    insert_frame(exec_info.stack_pa, new_thread.clone(), stack_bottom);

    child.tid.store(tid, Ordering::Release);

    // 登记子任务到parent
    parent.children.lock().push(child);

    tid
}

/// Exits a process.
///
/// Panic if the current thread doesn't own a user process.
pub fn exit(value: isize) -> ! {
    // 检查是否拥有用户进程
    let current = thread::current();
    let proc = current
        .userproc
        .as_ref()
        .expect("Current thread has no user process");

    if let Some(pt) = current.pagetable.as_ref() {
        // let mut unmap_ok = true;
        let mapids: Vec<usize> = {
            let mappings = proc.mappings.lock();
            let keys = mappings.keys();
            keys.cloned().collect()
        };
        for mapid in mapids {
            proc.munmap(mapid, pt);
        }
    };

    // 回收 ELF/栈页面换出时占用的 swap slot
    let vm_guard = crate::mem::frame::VM_LOCK.lock();
    proc.release_swap_slots();

    // 移除本进程剩余（驻留）页面的 Frame 记录；
    // 物理页随后由 Thread::drop -> PageTable::destroy 统一回收。
    remove_frames_of(&current);
    drop(vm_guard);

    let task = Arc::clone(&current.task);
    drop(current);

    // 父进程退出时，不再保留对子进程的强引用
    task.children.lock().clear();

    // 写入退出状态
    {
        let mut status = task.exit_status.lock();
        assert!(status.is_none(), "Process exited twice");
        *status = Some(value);
    }
    // 让 status 在右花括号处析构，把锁释放掉

    // 唤醒等待者
    task.done.up();

    // 退出
    drop(task);

    thread::exit();
}

/// Waits for a child thread, which must own a user process.
///
/// ## Return
/// - `Some(exit_value)`
/// - `None`: if tid was not created by the current thread.
pub fn wait(tid: isize) -> Option<isize> {
    let current = Arc::clone(&current().task);

    // 找到所需的子进程
    let child = {
        let mut children = current.children.lock();
        let index = children
            .iter()
            .position(|child| child.tid.load(Ordering::Acquire) == tid)?;

        // 把 Arc<TaskStruct> 从 children 中取出来，交给局部变量 child
        children.remove(index)
    };

    // 等待锁
    child.done.down();

    // 子进程退出后获取其退出状态
    let status = *child.exit_status.lock();
    status
}

pub fn halt() -> ! {
    shutdown();
}

/// Initializes a user process in current thread.
///
/// This function won't return.
pub fn start(mut frame: Frame) -> ! {
    unsafe { sstatus::set_spp(sstatus::SPP::User) };
    frame.sstatus = sstatus::read();

    // Set kernel stack pointer to intr frame and then jump to `trap_exit_u()`.
    let kernal_sp = (&frame as *const Frame) as usize;

    unsafe {
        asm!(
            "mv sp, t0",
            "jr t1",
            in("t0") kernal_sp,
            in("t1") trap_exit_u as *const u8
        );
    }

    unreachable!();
}
