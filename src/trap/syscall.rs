//! Syscall handlers
//!

#![allow(dead_code)]
use crate::fs::disk::DISKFS;
use crate::mem::userbuf::read_user_cstr;
use crate::sbi::console::stdout;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use fs::FileSys;

const MAX_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 3072;

/* -------------------------------------------------------------------------- */
/*                               SYSCALL NUMBER                               */
/* -------------------------------------------------------------------------- */

use crate::userproc::{execute, exit, halt, wait};
use crate::{mem::userbuf::copy_from_user, sbi::console_putchar};

const SYS_HALT: usize = 1;
const SYS_EXIT: usize = 2;
const SYS_EXEC: usize = 3;
const SYS_WAIT: usize = 4;
const SYS_REMOVE: usize = 5;
const SYS_OPEN: usize = 6;
const SYS_READ: usize = 7;
const SYS_WRITE: usize = 8;
const SYS_SEEK: usize = 9;
const SYS_TELL: usize = 10;
const SYS_CLOSE: usize = 11;
const SYS_FSTAT: usize = 12;

/* -------------------------------------------------------------------------- */
/*                             SYSCALL FUNCTIONS                              */
/* -------------------------------------------------------------------------- */

/// # sys_write
/// 从用户缓冲区user_src写size字节到指定的fd
/// ## Return
/// - -1: 错误
/// - 非负整数: 实际写入的字节数
fn sys_write(fd: usize, user_src: *const u8, size: usize) -> isize {
    if fd == 0 {
        return -1;
    }
    match fd {
        1 | 2 => {
            let mut buffer = vec![0u8; size];
            if (copy_from_user(user_src, &mut buffer)).is_err() {
                return -1;
            }

            let _lock = stdout().lock();
            // 逐字节输出
            for &byte in buffer.iter() {
                console_putchar(byte as usize);
            }

            size as isize
        }
        // TODO: 其他文件描述符
        _ => -1,
    }
}

pub fn read_user_argv(addr: usize) -> Option<Vec<String>> {
    // addr是argv数组的地址。
    // argv数组中有一系列usize类型的元素，每个元素是一个字符串指针。
    // 对argv的每个元素，读取一个C风格字符串。
    if addr == 0 {
        return None;
    }

    let mut argv = Vec::new();
    let mut total_bytes = 0usize;

    for i in 0..=MAX_ARGS {
        // 计算第i个字符串指针的地址
        let offset = i.checked_mul(size_of::<usize>())?;
        let addr_i = addr.checked_add(offset)?;

        // 把字符串指针读到buf中
        let mut buf = [0u8; size_of::<usize>()];
        copy_from_user(addr_i as *const u8, &mut buf).ok()?;
        let str_addr = usize::from_ne_bytes(buf);

        // 如果addr_i处读不出字符串，表明argv数组结束
        if str_addr == 0 {
            return Some(argv);
        }

        if i == MAX_ARGS {
            return None;
        }

        // 读取一个字符串
        let str_i = read_user_cstr(str_addr)?;

        total_bytes = total_bytes.checked_add(str_i.len().checked_add(1)?)?;
        if total_bytes > MAX_ARG_BYTES {
            return None;
        }
        argv.push(str_i);
    }

    None
}

fn sys_exec(pathname: *const u8, argv: *const usize) -> isize {
    let Some(path) = read_user_cstr(pathname as usize) else {
        return -1;
    };

    let Some(args) = read_user_argv(argv as usize) else {
        return -1;
    };

    let Ok(file) = DISKFS.open(path.as_str().into()) else {
        return -1;
    };

    execute(file, args)
}

pub fn syscall_handler(id: usize, args: [usize; 3]) -> isize {
    // TODO: LAB2 impl
    match id {
        SYS_WRITE => {
            return sys_write(args[0], args[1] as *const u8, args[2]);
        }
        SYS_HALT => {
            halt();
        }
        SYS_EXIT => exit(args[0] as i32 as isize),
        SYS_WAIT => wait(args[0] as i32 as isize).unwrap_or(-1),
        SYS_EXEC => sys_exec(args[0] as *const u8, args[1] as *const usize),
        _ => -1,
    }
}
