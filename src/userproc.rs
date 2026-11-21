//! User process.
//!

mod load;

use crate::fs::File;
use crate::io::{Read, Seek, SeekFrom, Write};
use crate::mem::mmap::Mapping;
use crate::mem::pagetable::{KernelPgTable, PageTable};
use crate::mem::palloc::UserPool;
use crate::mem::{PTEFlags, PageAlign, PhysAddr, PG_SIZE};
use crate::sbi::shutdown;
use crate::sync::Intr;
use crate::sync::Mutex;
use crate::sync::Sleep;
use crate::thread::{self, current, TaskStruct, STACK_TOP};
use crate::trap::{trap_exit_u, Frame};

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
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

/// 记录被mmap的页的元数据
#[derive(Clone)]
pub struct MmapPage {
    mapid: usize,       // 由mapid能找到Mapping
    file_offset: usize, // 找到该页从文件中的什么地方开始
    read_bytes: usize,  // 如果等于PG_SIZE就为一整页；小于就用0补齐
}

impl MmapPage {}

pub struct UserProc {
    #[allow(dead_code)]
    bin: File,
    fd_table: Mutex<BTreeMap<usize, OpenFile>, Sleep>,
    pages: Mutex<BTreeMap<usize, MmapPage>, Sleep>,
    mappings: Mutex<BTreeMap<usize, Mapping>, Sleep>,
    next_mapid: AtomicUsize,
    pub stack_bottom: AtomicUsize,
}

impl UserProc {
    pub fn new(file: File) -> Self {
        Self {
            bin: file,
            fd_table: Mutex::new(BTreeMap::new()),
            pages: Mutex::new(BTreeMap::new()),
            mappings: Mutex::new(BTreeMap::new()),
            next_mapid: AtomicUsize::new(1),
            stack_bottom: AtomicUsize::new(STACK_TOP - PG_SIZE),
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
        let maps = self.mappings.lock();

        // 与已有 mmap 区间检查冲突
        for mapping in maps.values() {
            let pages = mapping.length / PG_SIZE + usize::from(mapping.length % PG_SIZE != 0);
            let old_end = mapping.start + pages * PG_SIZE;

            if start < old_end && mapping.start < end {
                return false;
            }
        }

        // 与已存在的页表映射检查冲突
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

    fn find_mmap(&self, va: usize) -> Option<MmapPage> {
        let pages = self.pages.lock();
        let Some(mmap_page) = pages.get(&va) else {
            return None;
        };
        Some(mmap_page.clone())
    }

    pub fn find_mapping(&self, va: usize) -> Option<Mapping> {
        let Some(mmap_page) = self.find_mmap(va) else {
            return None;
        };
        let mapid = mmap_page.mapid;
        let maps = self.mappings.lock();
        let Some(mapping) = maps.get(&mapid) else {
            return None;
        };
        Some(mapping.clone())
    }

    /// mmap
    pub fn mmap(&self, fd: usize, addr: usize) -> isize {
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
        maps.insert(
            mapid,
            Mapping {
                openfile: file,
                start: addr,
                length: file_len,
                flags: flag_r | flag_w | PTEFlags::U,
                // 暂时不设为valid是因为懒加载；load时需要补上
            },
        );

        drop(maps);

        // 为每个被映射的虚拟页保存元数据
        let mut pg_number = 0;
        let mut va = addr;
        let mut file_offset = 0;

        let mut pages = self.pages.lock();

        while pg_number < n_pages {
            let mut read_bytes = PG_SIZE;
            if pg_number == n_pages - 1 {
                read_bytes = file_len - PG_SIZE * (n_pages - 1);
            }
            pages.insert(
                va,
                MmapPage {
                    mapid: mapid,
                    file_offset: file_offset,
                    read_bytes: read_bytes,
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

    pub fn load_mmap_page(&self, addr: usize, pt: &Mutex<PageTable, Intr>) -> bool {
        let page_va = addr.floor();
        let pg_table = pt.lock();
        if let Some(entry) = pg_table.get_pte(page_va) {
            if entry.is_valid() {
                return true;
            }
        }
        drop(pg_table);

        // 把磁盘中的文件的指定部分加载到页中，
        // 获取mmap_page和mapping

        let Some(mmap_page) = self.find_mmap(page_va) else {
            return false;
        };
        let Some(mapping) = self.find_mapping(page_va) else {
            return false;
        };

        // 打开指定文件并用seek定位到offset处
        let mut file = mapping.openfile.file;
        let buf = unsafe { UserPool::alloc_pages(1) };
        let page = unsafe { (buf as *mut [u8; PG_SIZE]).as_mut().unwrap() };
        let file_offset = mmap_page.file_offset;
        let read_bytes = mmap_page.read_bytes;

        let Ok(_pos) = file.seek(SeekFrom::Start(file_offset)) else {
            return false;
        };

        let mut filebuf = vec![0u8; read_bytes];

        // 将read_bytes长度的文件片段读到filebuf中
        let Ok(_bytes) = file.read_exact(&mut filebuf) else {
            return false;
        };
        // 再读到指定物理页，并用0补齐
        page[..read_bytes].copy_from_slice(&filebuf);
        page[read_bytes..].fill(0);

        let pa = PhysAddr::from(buf);

        // 对页表加锁并更新valid（注意，因为不能在关闭中断时进行I/O，所以在这里统一进行）
        let mut pg_table = pt.lock();

        // 建立物理页映射并更新页表项为valid
        pg_table.map(pa, page_va, PG_SIZE, mapping.flags | PTEFlags::V);
        pg_table.activate();

        true
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

    let exec_info = match load::load_executable(&mut file, &mut pt) {
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
    let userproc = UserProc::new(file);

    // ---------- (Lab2) Pass arguments to user program ------------
    let stack_top = exec_info.init_sp;
    let stack_bottom = stack_top - PG_SIZE;
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
    let _proc = current
        .userproc
        .as_ref()
        .expect("Current thread has no user process");

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
