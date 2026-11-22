//! Swap file.
//!
//! 提供 swap slot 的分配、读写与释放。

use super::DISKFS;
use crate::fs::{File, FileSys};
use crate::io::{Read, Seek, SeekFrom, Write};
use crate::mem::PG_SIZE;
use crate::sync::{Lazy, Mutex, MutexGuard, Primitive};
use alloc::vec::Vec;

pub struct Swap;

pub struct SwapFileGuard(MutexGuard<'static, Option<File>, Primitive>);

impl core::ops::Deref for SwapFileGuard {
    type Target = File;
    fn deref(&self) -> &File {
        self.0.as_ref().unwrap()
    }
}

impl core::ops::DerefMut for SwapFileGuard {
    fn deref_mut(&mut self) -> &mut File {
        self.0.as_mut().unwrap()
    }
}

// Lazy 初始化本身不能执行可能阻塞的文件 I/O。
static SWAPFILE: Lazy<Mutex<Option<File>>> = Lazy::new(|| Mutex::new(None));
static FREE_SLOTS: Lazy<Mutex<Option<Vec<usize>>>> = Lazy::new(|| Mutex::new(None));

impl Swap {
    pub fn len() -> usize {
        Self::lock().len().unwrap()
    }

    pub fn page_num() -> usize {
        // Round down.
        Self::len() / PG_SIZE
    }

    pub fn lock() -> SwapFileGuard {
        let mut file = SWAPFILE.lock();
        if file.is_none() {
            *file = Some(
                DISKFS
                    .open(".glbswap".into())
                    .expect("swap file should exist"),
            );
        }
        SwapFileGuard(file)
    }

    /// 分配一个空闲 slot，没有则返回 None
    pub fn alloc_slot() -> Option<usize> {
        let mut slots = FREE_SLOTS.lock();
        if slots.is_none() {
            *slots = Some((0..Self::page_num()).collect());
        }
        slots.as_mut().unwrap().pop()
    }

    /// 释放一个 slot
    pub fn free_slot(slot: usize) {
        FREE_SLOTS.lock().as_mut().unwrap().push(slot);
    }

    /// 把一页数据写入指定 slot
    pub fn write_page(slot: usize, data: &[u8]) -> bool {
        assert_eq!(data.len(), PG_SIZE);
        let mut file = Self::lock();
        file.seek(SeekFrom::Start(slot * PG_SIZE)).is_ok() && file.write_all(data).is_ok()
    }

    /// 从指定 slot 读出一页数据
    pub fn read_page(slot: usize, data: &mut [u8]) -> bool {
        assert_eq!(data.len(), PG_SIZE);
        let mut file = Self::lock();
        file.seek(SeekFrom::Start(slot * PG_SIZE)).is_ok() && file.read_exact(data).is_ok()
    }
}
