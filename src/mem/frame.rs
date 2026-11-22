//! Global Frame Table.
//!
//! 以物理页（Frame）为中心，记录每个用户物理页的归属，
//! 并提供 Clock（近似 LRU）算法选择牺牲帧。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::arch::asm;

use crate::fs::disk::Swap;
use crate::io::{Seek, SeekFrom, Write};
use crate::mem::palloc::UserPool;
use crate::mem::spt::{PageMeta, PageState};
use crate::mem::{PhysAddr, PG_SIZE};
use crate::sync::{Lazy, Mutex, Sleep};
use crate::thread::Thread;

// 串行化跨进程换入、换出和解除映射；持锁期间允许等待 I/O。
pub static VM_LOCK: Lazy<Mutex<(), Sleep>> = Lazy::new(|| Mutex::new(()));

/// 一个用户物理页（Frame）的记录
pub struct FrameEntry {
    pub pa: PhysAddr,
    /// 所属进程，通过弱引用定位其 PageTable 与 SPT
    pub owner: Weak<Thread>,
    /// 对应的用户虚拟页地址（页对齐）
    pub va: usize,
    /// 是否禁止驱逐
    pub pinned: bool,
}

pub struct FrameTable {
    frames: Vec<FrameEntry>,
    clock_hand: usize,
}

impl FrameTable {
    pub fn new() -> Self {
        Self {
            frames: Vec::new(),
            clock_hand: 0,
        }
    }

    pub fn insert(&mut self, entry: FrameEntry) {
        if let Some(pos) = self.frames.iter().position(|f| f.pa == entry.pa) {
            self.frames[pos] = entry;
        } else {
            self.frames.push(entry);
        }
    }

    pub fn get(&self, pa: PhysAddr) -> Option<&FrameEntry> {
        self.frames.iter().find(|f| f.pa == pa)
    }

    pub fn get_mut(&mut self, pa: PhysAddr) -> Option<&mut FrameEntry> {
        self.frames.iter_mut().find(|f| f.pa == pa)
    }

    pub fn remove(&mut self, pa: PhysAddr) -> Option<FrameEntry> {
        let pos = self.frames.iter().position(|f| f.pa == pa)?;
        let entry = self.frames.remove(pos);
        self.clock_hand = if self.frames.is_empty() {
            0
        } else {
            self.clock_hand % self.frames.len()
        };
        Some(entry)
    }

    pub fn pin(&mut self, pa: PhysAddr) {
        if let Some(f) = self.get_mut(pa) {
            f.pinned = true;
        }
    }

    pub fn unpin(&mut self, pa: PhysAddr) {
        if let Some(f) = self.get_mut(pa) {
            f.pinned = false;
        }
    }

    /// 使用 Clock 算法选择一个牺牲帧，返回其物理地址。
    ///
    /// 扫描过程中会清零被访问帧的 A 位，给它们第二次机会；
    /// 若所有帧都被 pin 或没有可驱逐页，则返回 None。
    pub fn select_victim(&mut self) -> Option<PhysAddr> {
        let len = self.frames.len();
        if len == 0 {
            return None;
        }

        // 扫描两轮：第一轮给 A=1 的页第二次机会（清零 A），
        // 第二轮它们就变成 A=0 可被选中。若仍找不到，说明确实没有可驱逐页。
        for _ in 0..(2 * len) {
            let idx = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % len;

            let frame = &self.frames[idx];
            if frame.pinned {
                continue;
            }

            let Some(owner) = frame.owner.upgrade() else {
                // 进程已退出，跳过陈旧条目
                continue;
            };

            let accessed = owner
                .pagetable
                .as_ref()
                .and_then(|pt| pt.lock().get_pte(frame.va).map(|e| e.is_accessed()));

            match accessed {
                Some(true) => {
                    // 清零 A 位，给第二次机会
                    if let Some(pt) = owner.pagetable.as_ref() {
                        let mut table = pt.lock();
                        if let Some(pte) = table.get_pte_mut(frame.va) {
                            pte.set_unaccessed();
                        }
                    }
                    sfence_vma(frame.va);
                    continue;
                }
                _ => {
                    return Some(frame.pa);
                }
            }
        }

        None
    }
}

static FRAME_TABLE: Lazy<Mutex<FrameTable, Sleep>> = Lazy::new(|| Mutex::new(FrameTable::new()));

/// 登记一个新分配的 Frame
pub fn insert_frame(pa: PhysAddr, owner: Arc<Thread>, va: usize) {
    FRAME_TABLE.lock().insert(FrameEntry {
        pa,
        owner: Arc::downgrade(&owner),
        va,
        pinned: false,
    });
}

/// 移除 Frame 记录
pub fn remove_frame(pa: PhysAddr) {
    FRAME_TABLE.lock().remove(pa);
}

/// 移除属于某个进程的所有 Frame 记录（不回收物理页，仅移除登记）。
pub fn remove_frames_of(owner: &Arc<Thread>) {
    let mut table = FRAME_TABLE.lock();
    let ptr = Arc::as_ptr(owner);
    table.frames.retain(|f| f.owner.as_ptr() != ptr);
    table.clock_hand = if table.frames.is_empty() {
        0
    } else {
        table.clock_hand % table.frames.len()
    };
}

/// 禁止驱逐
pub fn pin_frame(pa: PhysAddr) {
    FRAME_TABLE.lock().pin(pa);
}

/// 允许驱逐
pub fn unpin_frame(pa: PhysAddr) {
    FRAME_TABLE.lock().unpin(pa);
}

/// 选择牺牲帧（Clock）
pub fn select_victim() -> Option<PhysAddr> {
    FRAME_TABLE.lock().select_victim()
}

/// 统一分配一个用户物理页并登记 Frame。
/// 内存不足时，使用 Clock 选择牺牲帧并驱逐后重试。
pub fn alloc_frame(owner: Arc<Thread>, va: usize) -> Option<PhysAddr> {
    let pa = alloc_user_page()?;
    insert_frame(pa, owner, va);
    pin_frame(pa);
    Some(pa)
}

/// 分配尚未绑定线程的用户页；首个用户栈同样需要支持置换。
pub fn alloc_user_page() -> Option<PhysAddr> {
    if let Some(page) = unsafe { UserPool::try_alloc_pages(1) } {
        return Some(PhysAddr::from(page));
    }

    // 内存不足：反复驱逐牺牲帧后重试
    for _ in 0..256 {
        let Some(victim) = select_victim() else {
            return None;
        };
        if evict_frame(victim) {
            if let Some(page) = unsafe { UserPool::try_alloc_pages(1) } {
                return Some(PhysAddr::from(page));
            }
        }
    }

    None
}

/// 把指定物理帧换出：
/// 1. 根据 SPT 元数据写回 mmap 文件或写入 swap；
/// 2. 解除 VA -> PA 映射并刷新 TLB；
/// 3. 回收物理页并移除 Frame 记录。
pub fn evict_frame(pa: PhysAddr) -> bool {
    // 1. 取出 Frame 信息并标记为正在驱逐，防止并发再次选中
    let (owner, va) = {
        let mut table = FRAME_TABLE.lock();
        let Some(entry) = table.get_mut(pa) else {
            return false;
        };
        let Some(owner) = entry.owner.upgrade() else {
            return false;
        };
        entry.pinned = true;
        (owner, entry.va)
    };

    // 执行驱逐；失败时恢复为可驱逐，成功则移除记录
    let ok = evict_frame_impl(&owner, va, pa);

    if ok {
        remove_frame(pa);
    } else {
        unpin_frame(pa);
    }

    ok
}

fn evict_frame_impl(owner: &Arc<Thread>, va: usize, pa: PhysAddr) -> bool {
    let Some(userproc) = owner.userproc.as_ref() else {
        return false;
    };
    let Some(pt) = owner.pagetable.as_ref() else {
        return false;
    };

    // 读 SPT 元数据与 dirty 位
    let Some(spt_entry) = userproc.find_page(va) else {
        return false;
    };
    // 先撤销用户访问，防止写回等待 I/O 时所属进程继续修改该页。
    let original = {
        let mut table = pt.lock();
        match table.get_pte_mut(va) {
            Some(pte) if pte.is_valid() => {
                let original = *pte;
                pte.set_invalid();
                original
            }
            _ => return false,
        }
    };
    sfence_vma(va);
    let dirty = original.is_dirty();
    let writeback_ok = (|| {
        // 写回（不持有页表锁）
        match &spt_entry.meta {
            PageMeta::Mmap {
                file_offset,
                read_bytes,
                ..
            } => {
                if dirty {
                    let Some(mapping) = userproc.find_mapping(va) else {
                        return false;
                    };
                    let mut file = mapping.openfile.file.clone();
                    let buf = unsafe {
                        core::slice::from_raw_parts(pa.into_va() as *const u8, *read_bytes)
                    };
                    if file.seek(SeekFrom::Start(*file_offset)).is_err()
                        || file.write_all(buf).is_err()
                    {
                        return false;
                    }
                }
                userproc.set_page_state(va, PageState::Unloaded);
            }
            PageMeta::Elf { .. } => {
                if dirty {
                    let Some(slot) = Swap::alloc_slot() else {
                        return false;
                    };
                    let buf =
                        unsafe { core::slice::from_raw_parts(pa.into_va() as *const u8, PG_SIZE) };
                    if !Swap::write_page(slot, buf) {
                        Swap::free_slot(slot);
                        return false;
                    }
                    userproc.set_page_state(va, PageState::Swapped { slot });
                } else {
                    userproc.set_page_state(va, PageState::Unloaded);
                }
            }
            PageMeta::Stack => {
                let Some(slot) = Swap::alloc_slot() else {
                    return false;
                };
                let buf =
                    unsafe { core::slice::from_raw_parts(pa.into_va() as *const u8, PG_SIZE) };
                if !Swap::write_page(slot, buf) {
                    Swap::free_slot(slot);
                    return false;
                }
                userproc.set_page_state(va, PageState::Swapped { slot });
            }
        }

        true
    })();
    if !writeback_ok {
        if let Some(pte) = pt.lock().get_pte_mut(va) {
            *pte = original;
        }
        sfence_vma(va);
        return false;
    }

    // 回收物理页
    unsafe {
        UserPool::dealloc_pages(pa.into_va() as *mut u8, 1);
    }

    true
}

/// 释放一个用户物理页并移除 Frame 记录。
pub fn free_frame(pa: PhysAddr) {
    remove_frame(pa);
    unsafe { UserPool::dealloc_pages(pa.into_va() as *mut u8, 1) };
}

fn sfence_vma(va: usize) {
    unsafe {
        asm!("sfence.vma {}, zero", in(reg) va);
    }
}
