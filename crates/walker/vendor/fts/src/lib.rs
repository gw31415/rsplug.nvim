#![doc(html_root_url = "https://docs.rs/fts")]

#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate enum_primitive;
extern crate libc;
extern crate num;

pub mod ffi;
pub mod fts;
pub mod walkdir;

/// Crate-level lock used by tests that mutate `test_data/dir2` permissions.
/// A single lock is needed because `fts::test` and `walkdir::test` are separate
/// modules that share the same on-disk `test_data/dir2` directory.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;
    pub static DIR2_LOCK: Mutex<()> = Mutex::new(());
}
