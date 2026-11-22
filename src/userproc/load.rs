use alloc::vec;
use alloc::vec::Vec;
use elf_rs::{Elf, ElfFile, ProgramHeaderEntry, ProgramHeaderFlags, ProgramType};

use crate::fs::File;
use crate::io::prelude::*;
use crate::mem::frame::alloc_user_page;
use crate::mem::pagetable::{PTEFlags, PageTable};
use crate::mem::spt::PageState::Unloaded;
use crate::mem::spt::{ElfChunk, PageMeta, SptEntry, SupplementalPageTable};
use crate::mem::{PageAlign, PhysAddr, PG_SIZE};
use crate::OsError::UserError;
use crate::{OsError, Result};

#[derive(Debug, Clone, Copy)]
pub(super) struct ExecInfo {
    pub entry_point: usize,
    pub init_sp: usize,
    pub stack_pa: PhysAddr,
}

/// Loads an executable file
///
/// ## Params
/// - `pagetable`: User's pagetable. We install the mapping to executable codes into it.
///
/// ## Return
/// On success, returns `Ok(usize, usize)`:
/// - arg0: the entry point of user program
/// - arg1: the initial sp of user program
pub(super) fn load_executable(
    file: &mut File,
    pt: &mut PageTable,
    spt: &mut SupplementalPageTable,
) -> Result<ExecInfo> {
    let mut exec_info = load_elf(file, spt)?;

    // Initialize user stack.
    exec_info.stack_pa = init_user_stack(pt, exec_info.init_sp)?;

    // Forbid modifying executable file when running
    file.deny_write();

    Ok(exec_info)
}

/// Parses the specified executable file and loads segments
fn load_elf(file: &mut File, spt: &mut SupplementalPageTable) -> Result<ExecInfo> {
    // Ensure cursor is at the beginning
    file.rewind()?;

    let len = file.len()?;
    let mut buf = vec![0u8; len];
    file.read(&mut buf)?;

    let elf = match Elf::from_bytes(&buf) {
        Ok(Elf::Elf64(elf)) => elf,
        Ok(Elf::Elf32(_)) | Err(_) => return Err(OsError::UnknownFormat),
    };

    // load each loadable segment into memory
    elf.program_header_iter()
        .filter(|p| p.ph_type() == ProgramType::LOAD)
        .try_for_each(|p| load_segment(&p, spt))?;

    Ok(ExecInfo {
        entry_point: elf.elf_header().entry_point() as _,
        init_sp: 0x80500000,
        stack_pa: PhysAddr::from_pa(0),
    })
}

/// Loads one segment and installs pagetable mappings
fn load_segment(phdr: &ProgramHeaderEntry, pagetable: &mut SupplementalPageTable) -> Result<()> {
    assert_eq!(phdr.ph_type(), ProgramType::LOAD);

    let seg_va = phdr.vaddr() as usize;
    let fileoff = phdr.offset() as usize;
    let filesz = phdr.filesz() as usize;
    let memsz = phdr.memsz() as usize;

    // 该段的权限（U/R/W/X），不包含 V
    let mut seg_flags = PTEFlags::U | PTEFlags::R;
    if phdr.flags().contains(ProgramHeaderFlags::EXECUTE) {
        seg_flags |= PTEFlags::X;
    }
    if phdr.flags().contains(ProgramHeaderFlags::WRITE) {
        seg_flags |= PTEFlags::W;
    }

    let file_end = seg_va + filesz;
    let mem_end = seg_va + memsz;

    let mut va = seg_va.floor();

    while va < mem_end {
        let next_va = va + PG_SIZE;

        // 当前页面中真正需要从文件读取的区间
        let begin = seg_va.max(va);
        let end = file_end.min(next_va);

        match pagetable.get_mut(va) {
            Some(entry) => {
                // 同一页需要合并权限
                entry.flags |= seg_flags;

                match &mut entry.meta {
                    PageMeta::Elf { chunks } => {
                        if begin < end {
                            chunks.push(ElfChunk {
                                file_offset: fileoff + (begin - seg_va),
                                page_offset: begin - va,
                                len: end - begin,
                            });
                        }
                    }
                    _ => {
                        // 与非 ELF 页面冲突，应报错
                        return Err(OsError::UnknownFormat);
                    }
                }
            }
            None => {
                let mut chunks = Vec::new();
                if begin < end {
                    chunks.push(ElfChunk {
                        file_offset: fileoff + (begin - seg_va),
                        page_offset: begin - va,
                        len: end - begin,
                    });
                }
                let entry = SptEntry {
                    meta: PageMeta::Elf { chunks },
                    state: Unloaded,
                    flags: seg_flags,
                };
                if !pagetable.insert(va, entry) {
                    return Err(UserError);
                }
            }
        }

        va = next_va;
    }

    Ok(())
}

/// Initializes the user stack.
fn init_user_stack(pagetable: &mut PageTable, init_sp: usize) -> Result<PhysAddr> {
    assert!(init_sp % PG_SIZE == 0, "initial sp address misaligns");

    // Allocate a page from UserPool as user stack（可失败，避免耗尽时 panic）。
    let _vm_guard = crate::mem::frame::VM_LOCK.lock();
    let stack_pa = alloc_user_page().ok_or(UserError)?;
    let stack_va = stack_pa.into_va() as *mut u8;
    unsafe { core::ptr::write_bytes(stack_va, 0, PG_SIZE) };

    // Get the start address of stack page
    let stack_page_begin = PageAlign::floor(init_sp - 1);

    // Install mapping
    let flags = PTEFlags::V | PTEFlags::R | PTEFlags::W | PTEFlags::U;
    pagetable.map(stack_pa, stack_page_begin, PG_SIZE, flags);

    #[cfg(feature = "debug")]
    kprintln!(
        "[USERPROC] User Stack Mapping: (k){:p} -> (u) {:#x}",
        stack_va,
        stack_page_begin
    );

    Ok(stack_pa)
}
