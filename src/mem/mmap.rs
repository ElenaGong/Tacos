use crate::userproc::OpenFile;
use mem::PTEFlags;

/// 缺页访问类型
pub enum FaultAccess {
    Read,
    Write,
    Execute,
}
#[derive(Clone)]
pub struct Mapping {
    // pub mapid: usize,
    pub openfile: OpenFile,
    pub start: usize, // 虚拟地址空间起始位置
    pub length: usize,
    pub flags: PTEFlags,
}

impl Mapping {
    pub fn allow(&self, access: FaultAccess) -> bool {
        match access {
            FaultAccess::Read => {
                if self.flags & PTEFlags::R == PTEFlags::R {
                    return true;
                }
            }
            FaultAccess::Write => {
                if self.flags & PTEFlags::W == PTEFlags::W {
                    return true;
                }
            }
            FaultAccess::Execute => {
                if self.flags & PTEFlags::X == PTEFlags::X {
                    return true;
                }
            }
        }
        false
    }
}
