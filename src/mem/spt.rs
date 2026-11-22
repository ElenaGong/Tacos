use crate::mem::mmap::FaultAccess;
use crate::mem::{PTEFlags, PageAlign};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// 一个 ELF 物理页内的一个文件读取片段
#[derive(Clone)]
pub struct ElfChunk {
    pub file_offset: usize, // ELF 文件内的偏移
    pub page_offset: usize, // 物理页内的目标偏移
    pub len: usize,         // 读取的字节数
}

#[derive(Clone)]
pub enum PageMeta {
    Stack,
    Mmap {
        mapid: usize,       // 由mapid能找到Mapping
        file_offset: usize, // 找到该页从文件中的什么地方开始
        read_bytes: usize,  // 如果等于PG_SIZE就为一整页；小于就用0补齐
    },
    Elf {
        chunks: Vec<ElfChunk>,
    },
}

#[derive(Clone, Copy)]
pub enum PageState {
    Unloaded,
    Resident,
    Swapped { slot: usize },
}

#[derive(Clone)]
pub struct SptEntry {
    pub meta: PageMeta,
    pub state: PageState,
    pub flags: PTEFlags,
}

impl SptEntry {
    /// 检查该页是否允许给定的访问类型
    pub fn allows(&self, access: FaultAccess) -> bool {
        let required = match access {
            FaultAccess::Read => PTEFlags::R,
            FaultAccess::Write => PTEFlags::W,
            FaultAccess::Execute => PTEFlags::X,
        };
        self.flags.contains(required)
    }
}

#[derive(Clone)]
pub struct SupplementalPageTable {
    entries: BTreeMap<usize, SptEntry>,
}

impl SupplementalPageTable {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// 注册一个虚拟页
    pub fn insert(&mut self, va: usize, entry: SptEntry) -> bool {
        // 地址对齐且不能覆盖已注册过的页表项
        if !va.is_aligned() || self.entries.contains_key(&va) {
            return false;
        }

        self.entries.insert(va, entry);
        true
    }

    /// 查询页面
    pub fn get(&self, va: usize) -> Option<&SptEntry> {
        self.entries.get(&va.floor())
    }

    /// 获取所有 SPT 项的克隆（供退出清理等一次性遍历使用）
    pub fn snapshot(&self) -> alloc::vec::Vec<SptEntry> {
        self.entries.values().cloned().collect()
    }

    /// 修改页面状态
    pub fn get_mut(&mut self, va: usize) -> Option<&mut SptEntry> {
        self.entries.get_mut(&va.floor())
    }

    /// 删除页面
    pub fn remove(&mut self, va: usize) -> Option<SptEntry> {
        self.entries.remove(&va.floor())
    }
}
