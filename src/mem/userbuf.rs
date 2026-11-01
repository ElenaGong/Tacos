#![allow(dead_code)]

use crate::error::OsError;
use crate::mem::{get_pte, in_kernel_space, PG_SIZE};
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
    for (i, byte) in dst.iter_mut().enumerate() {
        let addr = (user_src as usize).checked_add(i).ok_or(OsError::BadPtr)? as *const u8;
        *byte = read_user_byte(addr)?;
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
    for (i, &byte) in src.iter().enumerate() {
        let addr = (user_dst as usize).checked_add(i).ok_or(OsError::BadPtr)? as *const u8;
        write_user_byte(addr, byte)?;
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
/// 检查当前进程的用户虚拟地址区间 [addr, addr + len)
/// 是否全部映射为用户可写页面。
pub fn user_range_writable(addr: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }

    // 计算最后一个字节，防止整数溢出
    let Some(last) = addr.checked_add(len - 1) else {
        return false;
    };

    // 排除内核地址
    if in_kernel_space(addr) || in_kernel_space(last) {
        return false;
    }

    // Sv39 低半区的规范用户地址范围
    const USER_LIMIT: usize = 1usize << 38;

    if last >= USER_LIMIT {
        return false;
    }

    // 找到覆盖范围中的第一个和最后一个虚拟页号
    let first_page = addr / PG_SIZE;
    let last_page = last / PG_SIZE;

    for page in first_page..=last_page {
        let va = page * PG_SIZE;

        let Some(pte) = get_pte(va) else {
            return false;
        };

        if !pte.is_valid() || !pte.is_leaf() || !pte.is_user() || !pte.is_rwable() {
            return false;
        }
    }

    true
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
