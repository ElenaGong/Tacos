use crate::mem::palloc::UserPool;
use crate::mem::userbuf::{
    __knrl_read_usr_byte_pc, __knrl_read_usr_exit, __knrl_write_usr_byte_pc, __knrl_write_usr_exit,
};
use crate::mem::{PTEFlags, PageAlign, PageTable, PhysAddr, PG_SIZE};
use crate::thread::{current, STACK_LIMIT, STACK_TOP};
use mem::mmap::FaultAccess;
// use crate::thread::{self};
use crate::trap::Frame;
use crate::userproc;
use core::sync::atomic::Ordering;

use riscv::register::scause::Exception::{self, *};
use riscv::register::sstatus::{self, SPP};

fn alloc_user_page(va: usize, pg_table: &mut PageTable, flags: PTEFlags) -> Option<PhysAddr> {
    let page = unsafe { UserPool::alloc_pages(1) };
    unsafe {
        core::ptr::write_bytes(page, 0, PG_SIZE);
    }
    let pa = PhysAddr::from(page);
    pg_table.map(pa, va, PG_SIZE, flags);

    Some(pa)
}

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

    // 获取用户态 SP
    let sp = frame.x[2];

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
    let mut table = pt.lock();
    // 从 UserPool 分配页面并建立页表映射
    let flags = PTEFlags::V | PTEFlags::R | PTEFlags::W | PTEFlags::U;

    let mut va = page_floor;

    while va < st_bottom {
        let Some(_pa) = alloc_user_page(va, &mut table, flags) else {
            return false;
        };
        va += PG_SIZE;
    }
    // 更新 stack_bottom
    userproc.stack_bottom.store(page_floor, Ordering::Relaxed);

    // 刷新 TLB
    table.activate();
    drop(table);

    // 分配新页后登记 PageMeta::Stack，状态为 Resident
    let mut va = page_floor;
    while va < st_bottom {
        if !userproc.register_stack_page(va) {
            return false;
        }
        va += PG_SIZE;
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
