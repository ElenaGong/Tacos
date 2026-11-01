//! Syscall handlers
//!

#![allow(dead_code)]
use crate::fs::disk::DISKFS;
use crate::fs::File;
use crate::mem::userbuf::{copy_to_user, read_user_cstr};
use crate::sbi::console::stdout;
use crate::sbi::console_getchar;
use crate::thread::current;
use crate::{OsError, Result};

use crate::mem::userbuf::user_range_writable;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;
use fs::FileSys;

const MAX_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 3072;

/* -------------------------------------------------------------------------- */
/*                               SYSCALL NUMBER                               */
/* -------------------------------------------------------------------------- */

use crate::userproc::{execute, exit, halt, wait, OpenFile};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

fn sys_open(pathname: *const u8, mode: Mode, create: bool, trunc: bool) -> isize {
    let Some(path) = read_user_cstr(pathname as usize) else {
        return -1;
    };
    // 空路径不是合法文件名
    if path.is_empty() {
        return -1;
    }

    // 允许create就直接走sys_create（包含打开已有文件的逻辑）
    let file = if create {
        sys_create(path.as_str())
    }
    // 否则只open
    else {
        DISKFS.open(path.as_str().into())
    };

    let Ok(mut file) = file else {
        return -1;
    };

    // 决定是否允许截断
    if trunc {
        if file.set_len(0).is_err() {
            return -1;
        }
    }

    // 访问权限
    let readable = matches!(mode, Mode::ReadOnly | Mode::ReadWrite);
    let writable = matches!(mode, Mode::WriteOnly | Mode::ReadWrite);

    // 分配fd
    let proc = current();
    let Some(userproc) = proc.userproc.as_ref() else {
        return -1;
    };

    match userproc.alloc_and_insert(OpenFile::new(file, readable, writable)) {
        Some(fd) => fd as isize,
        None => -1,
    }
}

/// 关闭文件，成功返回0，失败返回-1
fn sys_close(fd: usize) -> isize {
    if fd < 3 {
        // TODO: close 0,1,2都相当于空操作；后续可以改成危险行为
        return 0;
    }
    let current = current();
    let Some(userproc) = current.userproc.as_ref() else {
        return -1;
    };
    if userproc.close_file(fd) {
        0
    } else {
        -1
    }
}

/// 文件不存在时创建；已存在则直接打开，保留内容
fn sys_create(path: &str) -> Result<File> {
    match DISKFS.open(path.into()) {
        Ok(file) => Ok(file),
        Err(OsError::NoSuchFile) => DISKFS.create(path.into()),
        Err(e) => Err(e),
    }
}

/// 忙等待，直到从标准输入读取1字节
// TODO: 后续应该支持多字符输入，回车换行才结束输入
fn read_stdin(buffer: *const u8, size: usize) -> isize {
    if size == 0 {
        return 0;
    }
    loop {
        let ch = console_getchar();
        // 暂时没有键盘输入
        if ch == usize::MAX {
            core::hint::spin_loop();
            continue;
        }
        // 只接受合法的单字节输入
        if ch > u8::MAX as usize {
            return -1;
        }
        let byte = [ch as u8];
        // 将读取的字符复制到用户缓冲区
        if copy_to_user(buffer, &byte).is_err() {
            return -1;
        }
        return 1;
    }
}
/// 从fd中读size字节数据到用户缓冲区
/// 返回实际读取的字节数
fn sys_read(fd: usize, buffer: *const u8, size: usize) -> isize {
    if size > isize::MAX as usize {
        return -1;
    }
    if fd == 1 || fd == 2 {
        return -1;
    }
    // 在进行任何 I/O 前，检查整个用户缓冲区
    if !user_range_writable(buffer as usize, size) {
        return -1;
    }

    // 标准输入：同样已完成缓冲区检查
    if fd == 0 {
        return read_stdin(buffer, size);
    }

    let current = current();
    let Some(proc) = current.userproc.as_ref() else {
        return -1;
    };
    let mut buf = [0u8; 512];
    let mut offset = 0usize;

    while offset < size {
        let len = (size - offset).min(buf.len());

        // fd文件 -> 内核缓冲区
        let Some(n) = proc.read_fd(fd, &mut buf[..len]) else {
            if offset > 0 {
                return offset as isize;
            } else {
                return -1;
            };
        };

        if n == 0 {
            break;
        }

        // 用户缓冲区地址
        let Some(addr) = (buffer as usize).checked_add(offset) else {
            return -1;
        };
        // 内核缓冲区 -> 用户缓冲区
        if copy_to_user(addr as *const u8, &buf[..n]).is_err() {
            return -1;
        }

        offset += n;
    }

    offset as isize
}

/// # sys_write
/// 从用户缓冲区user_src写size字节到指定的fd
/// ## Return
/// - -1: 错误
/// - 非负整数: 实际写入的字节数
fn sys_write(fd: usize, user_src: *const u8, size: usize) -> isize {
    if fd == 0 || size > isize::MAX as usize {
        return -1;
    }
    let mut buf = [0u8; 512];
    let mut offset = 0usize;

    match fd {
        1 | 2 => {
            // let _lock = stdout().lock();
            while offset < size {
                let len = (size - offset).min(buf.len());

                let Some(addr) = (user_src as usize).checked_add(offset) else {
                    return -1;
                };

                // 从用户缓冲区读取到内核缓冲区
                if copy_from_user(addr as *const u8, &mut buf[..len]).is_err() {
                    return -1;
                }

                // 写入控制台
                let _lock = stdout().lock();
                for &byte in &buf[..len] {
                    console_putchar(byte as usize);
                }

                offset += len;
            }
        }
        // TODO: 其他文件描述符
        _ => {
            let current = current();
            let Some(proc) = current.userproc.as_ref() else {
                return -1;
            };

            while offset < size {
                let len = (size - offset).min(buf.len());
                let Some(addr) = (user_src as usize).checked_add(offset) else {
                    return -1;
                };
                if copy_from_user(addr as *const u8, &mut buf[..len]).is_err() {
                    return -1;
                }

                let Some(n) = proc.write_fd(fd, &buf[..len]) else {
                    if offset > 0 {
                        return offset as isize;
                    } else {
                        return -1;
                    }
                };

                if n == 0 {
                    break;
                }

                offset += n;

                // 短写：返回目前实际写入的字节数
                if n < len {
                    break;
                }
            }
        }
    }
    offset as isize
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

fn sys_fstat(fd: usize, buf: *const u8) -> isize {
    let current = current();
    let Some(proc) = current.userproc.as_ref() else {
        return -1;
    };
    // TODO: 用 proc.get(fd) 取出 (ino, size)，拼成 16 字节写回用户缓冲区
    let Some((ino, size)) = proc.get_fstat(fd) else {
        return -1;
    };

    let mut stat = [0u8; 16];
    stat[0..8].copy_from_slice(&(ino as u64).to_le_bytes());
    stat[8..16].copy_from_slice(&(size as u64).to_le_bytes());

    if copy_to_user(buf, &stat).is_err() {
        return -1;
    }
    0
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

fn sys_seek(fd: usize, position: usize) -> isize {
    let current = current();
    let proc = current.userproc.as_ref().expect("No user process");
    proc.seek_fd(fd, position);
    0
}
fn sys_tell(fd: usize) -> isize {
    let current = current();
    let proc = current.userproc.as_ref().expect("No user process");

    match proc.tell_fd(fd) {
        Some(pos) if pos <= isize::MAX as usize => pos as isize,
        _ => -1,
    }
}

fn sys_remove(pathname: *const u8) -> isize {
    let Some(path) = read_user_cstr(pathname as usize) else {
        return -1;
    };
    match DISKFS.remove(path.as_str().into()) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}
pub fn syscall_handler(id: usize, args: [usize; 3]) -> isize {
    // TODO: LAB2 impl
    match id {
        SYS_HALT => {
            halt();
        }
        SYS_EXIT => exit(args[0] as i32 as isize),
        SYS_WAIT => wait(args[0] as i32 as isize).unwrap_or(-1),
        SYS_EXEC => sys_exec(args[0] as *const u8, args[1] as *const usize),
        SYS_OPEN => {
            let path = args[0] as *const u8;
            let flags = args[1];
            const O_CREATE: usize = 0x200;
            const O_TRUNC: usize = 0x400;
            // 拒绝不支持的flag
            if flags & !(0x3 | O_CREATE | O_TRUNC) != 0 {
                return -1;
            }
            // 读写权限
            let access = flags & 0x3;
            let mode = match access {
                0 => Mode::ReadOnly,
                1 => Mode::WriteOnly,
                2 => Mode::ReadWrite,
                _ => {
                    return -1;
                }
            };
            // 创建/截断

            let create = flags & O_CREATE != 0;
            let trunc = flags & O_TRUNC != 0;

            sys_open(path, mode, create, trunc)
        }
        SYS_CLOSE => sys_close(args[0] as usize),
        SYS_READ => sys_read(args[0], args[1] as *const u8, args[2]),
        SYS_WRITE => {
            return sys_write(args[0], args[1] as *const u8, args[2]);
        }
        SYS_FSTAT => sys_fstat(args[0], args[1] as *const u8),
        SYS_SEEK => sys_seek(args[0], args[1]),
        SYS_TELL => sys_tell(args[0]),
        SYS_REMOVE => sys_remove(args[0] as *const u8),
        _ => -1,
    }
}
