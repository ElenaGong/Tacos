use crate::mem::frame::{alloc_frame, free_frame, unpin_frame};
use crate::mem::userbuf::{
    __knrl_read_usr_byte_pc, __knrl_read_usr_exit, __knrl_write_usr_byte_pc, __knrl_write_usr_exit,
};
use crate::mem::{PTEFlags, PageAlign, PageTable, PhysAddr, PG_SIZE};
use crate::thread::{current, STACK_LIMIT, STACK_TOP};
use mem::mmap::FaultAccess;
// use crate::thread::{self};
use crate::trap::Frame;
use crate::userproc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use riscv::register::scause::Exception::{self, *};
use riscv::register::sstatus::{self, SPP};

/// 用户态缺页处理结果
enum FaultError {
    InvalidAccess,
    LoadFailed,
}

/// 尝试增长栈
fn try_grow_stack(frame: &Frame, fault: Exception, addr: usize) -> bool {
    // 检查访问类型
    if !matches!(fault, LoadPageFault | StorePageFault) {
        return false;
    }

    grow_stack(frame.x[2], addr)
}

pub(crate) fn grow_stack(sp: usize, addr: usize) -> bool {
    let _vm_guard = crate::mem::frame::VM_LOCK.lock();
    // 获取当前进程的 stack_bottom
    let current_thread = current();

    let Some(userproc) = current_thread.userproc.as_ref() else {
        return false;
    };

    let st_bottom = userproc.stack_bottom.load(Ordering::Relaxed);
    let page_floor = addr.floor();

    // 检查栈增长边界
    let stack_access_legal: bool = addr >= sp;
    let sp_legal: bool = sp >= STACK_LIMIT && sp <= STACK_TOP;
    let growth_dir_legal: bool = page_floor < st_bottom;
    let within_limit: bool = addr < STACK_TOP && page_floor >= STACK_LIMIT;

    if !(stack_access_legal && sp_legal && growth_dir_legal && within_limit) {
        return false;
    }

    // 检查所有待分配页面是否存在映射冲突（包括 SPT 中已登记的页、PTE 中已分配的页和已经被 mmap 映射的页）
    if !userproc.not_mapped(page_floor, st_bottom) {
        return false;
    }

    // 获取当前进程页表
    let Some(pt) = current_thread.pagetable.as_ref() else {
        return false;
    };
    let flags = PTEFlags::V | PTEFlags::R | PTEFlags::W | PTEFlags::U;

    // 先分配物理页并登记 Frame（不持有页表锁，避免 Intr -> Sleep 的锁序问题）
    let mut frames: Vec<(PhysAddr, usize)> = Vec::new();
    let mut va = page_floor;
    while va < st_bottom {
        let Some(pa) = alloc_frame(current_thread.clone(), va) else {
            for (pa, _) in &frames {
                free_frame(*pa);
            }
            return false;
        };
        // 栈页需要清零
        unsafe {
            core::ptr::write_bytes(pa.into_va() as *mut u8, 0, PG_SIZE);
        }
        frames.push((pa, va));
        va += PG_SIZE;
    }

    // 建立 VA -> PA 映射并刷新 TLB
    let mut table = pt.lock();
    for (pa, va) in &frames {
        table.map(*pa, *va, PG_SIZE, flags);
    }
    // 更新 stack_bottom
    userproc.stack_bottom.store(page_floor, Ordering::Relaxed);
    table.activate();
    drop(table);

    // 元数据完成后才允许驱逐。
    for (_, va) in &frames {
        assert!(userproc.register_stack_page(*va));
    }
    for (pa, _) in &frames {
        unpin_frame(*pa);
    }

    true
}

/// 处理用户态 Page Fault
fn handle_user_page_fault(frame: &Frame, fault: Exception, addr: usize) -> Result<(), FaultError> {
    // 判断访问类型
    let access = match fault {
        LoadPageFault => FaultAccess::Read,
        StorePageFault => FaultAccess::Write,
        InstructionPageFault => FaultAccess::Execute,
        _ => return Err(FaultError::InvalidAccess),
    };

    let page_va = addr.floor();
    let thread = current();
    let proc = thread.userproc.as_ref().ok_or(FaultError::InvalidAccess)?;

    // 查询当前页表
    let pt = thread.pagetable.as_ref().ok_or(FaultError::InvalidAccess)?;

    {
        let table = pt.lock();

        if let Some(pte) = table.get_pte(page_va) {
            if pte.is_valid() && pte.is_leaf() {
                // 已映射却产生 Page Fault
                // 简化设计：按权限异常处理
                // 实际还需考虑 A/D 位等情况
                return Err(FaultError::InvalidAccess);
            }
        }
    }

    // 优先检查 mmap 预留区域
    if let Some(entry) = proc.find_page(page_va) {
        if !entry.allows(access) {
            return Err(FaultError::InvalidAccess);
        }

        if proc.lazily_load_page(page_va, pt) {
            return Ok(());
        }

        return Err(FaultError::LoadFailed);
    }

    // SPT 中不存在，才尝试增长栈
    if try_grow_stack(frame, fault, addr) {
        return Ok(());
    }

    // 所有条件均不满足
    Err(FaultError::InvalidAccess)
}

pub fn handler(frame: &mut Frame, fault: Exception, addr: usize) {
    let privilege = frame.sstatus.spp();

    let _present = {
        let table = unsafe { PageTable::effective_pagetable() };
        match table.get_pte(addr) {
            Some(entry) => entry.is_valid(),
            None => false,
        }
    };

    unsafe { sstatus::set_sie() };

    // kprintln!(
    //     "Page fault at {:#x}: {} error {} page in {} context.",
    //     addr,
    //     if present { "rights" } else { "not present" },
    //     match fault {
    //         StorePageFault => "writing",
    //         LoadPageFault => "reading",
    //         InstructionPageFault => "fetching instruction",
    //         _ => panic!("Unknown Page Fault"),
    //     },
    //     match privilege {
    //         SPP::Supervisor => "kernel",
    //         SPP::User => "user",
    //     }
    // );

    match privilege {
        SPP::Supervisor => {
            if frame.sepc == __knrl_read_usr_byte_pc as _ {
                // Failed to read user byte from kernel space when trap in pagefault
                frame.x[11] = 1; // set a1 to non-zero
                frame.sepc = __knrl_read_usr_exit as _;
            } else if frame.sepc == __knrl_write_usr_byte_pc as _ {
                // Failed to write user byte from kernel space when trap in pagefault
                frame.x[11] = 1; // set a1 to non-zero
                frame.sepc = __knrl_write_usr_exit as _;
            } else {
                panic!("Kernel page fault");
            }
        }
        SPP::User => {
            match handle_user_page_fault(frame, fault, addr) {
                Ok(()) => {
                    // Page Fault 成功处理
                    // 保持 frame.sepc 不变
                    // 返回后重新执行原指令
                    return;
                }

                Err(FaultError::InvalidAccess) => {
                    // 非法地址或权限错误
                    userproc::exit(-1);
                }

                Err(FaultError::LoadFailed) => {
                    // 页面分配或文件加载失败
                    userproc::exit(-1);
                }
            }
        }
    }
}
