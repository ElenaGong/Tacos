use crate::fs::File;

pub struct Mapping {
    // pub mapid: usize,
    pub file: File,
    pub start: usize,
    pub length: usize,
}
