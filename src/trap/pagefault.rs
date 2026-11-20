use crate::mem::palloc::UserPool;
use crate::mem::userbuf::{
    __knrl_read_usr_byte_pc, __knrl_read_usr_exit, __knrl_write_usr_byte_pc, __knrl_write_usr_exit,
};
use crate::mem::{PTEFlags, PageAlign, PageTable, PhysAddr, PG_SIZE};
use crate::thread::{current, STACK_LIMIT, STACK_TOP};
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

    // 检查所有待分配页面是否存在映射冲突（包括PTE中已分配的页和已经被mmap映射的页）
    if !userproc.not_mapped(page_floor, page_floor + PG_SIZE) {
        return false;
    }

    // 获取当前进程页表
    let Some(pt) = current_thread.pagetable.as_ref() else {
        return false;
    };
    let mut table = pt.lock();
    // 从UserPool分配页面并建立页表映射
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

    true
}

pub fn handler(frame: &mut Frame, fault: Exception, addr: usize) {
    let privilege = frame.sstatus.spp();

    let present = {
        let table = unsafe { PageTable::effective_pagetable() };
        match table.get_pte(addr) {
            Some(entry) => entry.is_valid(),
            None => false,
        }
    };

    unsafe { sstatus::set_sie() };

    kprintln!(
        "Page fault at {:#x}: {} error {} page in {} context.",
        addr,
        if present { "rights" } else { "not present" },
        match fault {
            StorePageFault => "writing",
            LoadPageFault => "reading",
            InstructionPageFault => "fetching instruction",
            _ => panic!("Unknown Page Fault"),
        },
        match privilege {
            SPP::Supervisor => "kernel",
            SPP::User => "user",
        }
    );

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
            if try_grow_stack(frame, fault, addr) {
                return;
            }
            userproc::exit(-1);
        }
    }
}
