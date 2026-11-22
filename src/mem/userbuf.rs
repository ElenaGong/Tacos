#![allow(dead_code)]

use crate::error::OsError;
use crate::mem::mmap::FaultAccess;
use crate::mem::{in_kernel_space, PG_SIZE};
use crate::thread::current;
use crate::Result;

use alloc::string::String;
use alloc::vec::Vec;
use core::arch::global_asm;

/// Read a single byte from user space.
///
/// ## Return
/// - `Ok(byte)`
/// - `Err`: A page fault happened.
fn read_user_byte(user_src: *const u8) -> Result<u8> {
    if in_kernel_space(user_src as usize) {
        return Err(OsError::BadPtr);
    }

    let mut byte: u8 = 0;
    let ret_status: u8 = unsafe { __knrl_read_usr_byte(user_src, &mut byte) };

    if ret_status == 0 {
        Ok(byte)
    } else {
        Err(OsError::BadPtr)
    }
}

/// Write a single byte to user space.
///
/// ## Return
/// - `Ok(())`
/// - `Err`: A page fault happened.
fn write_user_byte(user_src: *const u8, value: u8) -> Result<()> {
    if in_kernel_space(user_src as usize) {
        return Err(OsError::BadPtr);
    }

    let ret_status: u8 = unsafe { __knrl_write_usr_byte(user_src, value) };

    if ret_status == 0 {
        Ok(())
    } else {
        Err(OsError::BadPtr)
    }
}

/// 实现用户内存访问
///
/// # copy_from_user
/// ## Args
/// - user_src：用户态虚拟地址。
/// - dst：内核缓冲区，长度由 dst.len() 决定。
/// ## Return
/// - Ok(())：复制成功。
/// - Err(())：地址无效或权限检查失败。
pub fn copy_from_user(user_src: *const u8, dst: &mut [u8]) -> Result<()> {
    let start = user_src as usize;
    start.checked_add(dst.len()).ok_or(OsError::BadPtr)?;
    let mut done = 0usize;

    while done < dst.len() {
        let addr = start.checked_add(done).ok_or(OsError::BadPtr)?;
        let page_va = addr & !(PG_SIZE - 1);
        let page_off = addr & (PG_SIZE - 1);
        let n = (dst.len() - done).min(PG_SIZE - page_off);

        // 先让这一页驻留并检查读权限，再逐字节拷贝
        if !fault_in_page(page_va, false) {
            return Err(OsError::BadPtr);
        }
        let thread = current();
        let table = thread.pagetable.as_ref().ok_or(OsError::BadPtr)?.lock();
        if !table.get_pte(page_va).is_some_and(|pte| pte.is_valid()) {
            continue;
        }
        for i in 0..n {
            let byte_addr = addr.checked_add(i).ok_or(OsError::BadPtr)? as *const u8;
            dst[done + i] = read_user_byte(byte_addr)?;
        }

        done += n;
    }
    Ok(())
}

/// # copy_to_user
/// ## Args
/// - user_dst: 用户态虚拟地址
/// - src：内核缓冲区地址
/// ## Return
/// - Ok(())
/// - Err(())
pub fn copy_to_user(user_dst: *const u8, src: &[u8]) -> Result<()> {
    let start = user_dst as usize;
    start.checked_add(src.len()).ok_or(OsError::BadPtr)?;
    let mut done = 0usize;

    while done < src.len() {
        let addr = start.checked_add(done).ok_or(OsError::BadPtr)?;
        let page_va = addr & !(PG_SIZE - 1);
        let page_off = addr & (PG_SIZE - 1);
        let n = (src.len() - done).min(PG_SIZE - page_off);

        // 先让这一页驻留并检查写权限，再逐字节写入
        if !fault_in_page(page_va, true) {
            return Err(OsError::BadPtr);
        }
        let thread = current();
        let table = thread.pagetable.as_ref().ok_or(OsError::BadPtr)?.lock();
        if !table.get_pte(page_va).is_some_and(|pte| pte.is_valid()) {
            continue;
        }
        for i in 0..n {
            let byte_addr = addr.checked_add(i).ok_or(OsError::BadPtr)? as *const u8;
            write_user_byte(byte_addr, src[done + i])?;
        }

        done += n;
    }
    Ok(())
}

const MAX_LENGTH: usize = 4096;

/// # read_user_cstr
/// 读取以'\0'结尾的用户态C字符串。最多读取4096字节。
/// ## Args
/// - addr (usize): 用户地址空间中的地址
/// ## Return
/// - Some(String)
/// - None
pub fn read_user_cstr(addr: usize) -> Option<String> {
    if addr == 0 {
        return None;
    }
    let mut bytes = Vec::new();

    for i in 0..MAX_LENGTH {
        // 读一个字节到byte
        let mut byte = [0u8; 1];
        let byte_addr = addr.checked_add(i)?;
        copy_from_user(byte_addr as *const u8, &mut byte).ok()?;

        // 如果当前byte为\0，就停止读取并返回
        if byte[0] == 0 {
            return String::from_utf8(bytes).ok();
        }

        // 加到bytes末尾
        bytes.push(byte[0]);
    }
    None
}
/// 让指定虚拟页驻留（懒加载或换入），并检查访问权限。
fn fault_in_page(page_va: usize, write: bool) -> bool {
    let thread = current();
    let Some(userproc) = thread.userproc.as_ref() else {
        return false;
    };
    let Some(pt) = thread.pagetable.as_ref() else {
        return false;
    };

    if userproc.find_page(page_va).is_none() {
        let sp = userproc.user_sp.load(core::sync::atomic::Ordering::Relaxed);
        if page_va < (sp & !(PG_SIZE - 1)) {
            return false;
        }
        if !crate::trap::pagefault::grow_stack(sp, page_va.max(sp)) {
            return false;
        }
    }
    let Some(entry) = userproc.find_page(page_va) else {
        return false;
    };
    let access = if write {
        FaultAccess::Write
    } else {
        FaultAccess::Read
    };
    if !entry.allows(access) {
        return false;
    }

    userproc.lazily_load_page(page_va, pt)
}

/// 让 [start, start + len) 覆盖的所有虚拟页驻留，并检查访问权限。
fn fault_in_range(start: usize, len: usize, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = start.checked_add(len) else {
        return false;
    };

    let mut va = start & !(PG_SIZE - 1);
    while va < end {
        if !fault_in_page(va, write) {
            return false;
        }
        va += PG_SIZE;
    }
    true
}

/// 系统调用访问用户缓冲前，先让范围内的页面驻留并检查权限。
pub fn fault_in_user_range(addr: usize, len: usize, write: bool) -> bool {
    fault_in_range(addr, len, write)
}

/// 检查当前进程的用户虚拟地址区间 [addr, addr + len)
/// 是否全部可写（会顺带把未驻留页加载/换入）。
pub fn user_range_writable(addr: usize, len: usize) -> bool {
    fault_in_user_range(addr, len, true)
}

extern "C" {
    // FIX: byte_ptr should be mutable
    pub fn __knrl_read_usr_byte(user_src: *const u8, byte_ptr: *mut u8) -> u8;
    pub fn __knrl_read_usr_byte_pc();
    pub fn __knrl_read_usr_exit();
    pub fn __knrl_write_usr_byte(user_src: *const u8, value: u8) -> u8;
    pub fn __knrl_write_usr_byte_pc();
    pub fn __knrl_write_usr_exit();
}

global_asm! {r#"
        .section .text
        .globl __knrl_read_usr_byte
        .globl __knrl_read_usr_exit
        .globl __knrl_read_usr_byte_pc

    __knrl_read_usr_byte:
        mv t1, a1
        li a1, 0
    __knrl_read_usr_byte_pc:
        lb t0, (a0)
    __knrl_read_usr_exit:
        # pagefault handler will set a1 if any error occurs
        sb t0, (t1)
        mv a0, a1
        ret

        .globl __knrl_write_usr_byte
        .globl __knrl_write_usr_exit
        .globl __knrl_write_usr_byte_pc

    __knrl_write_usr_byte:
        mv t1, a1
        li a1, 0
    __knrl_write_usr_byte_pc:
        sb t1, (a0)
    __knrl_write_usr_exit:
        # pagefault handler will set a1 if any error occurs
        mv a0, a1
        ret
"#}
