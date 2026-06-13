//! linux-boot-extractor: read kernels, initramfs and boot configs out of VM
//! disk images without mounting anything.
//!
//! The stack is layered around one trait, [`blockdev::ReadAt`]:
//!
//! ```text
//!   disk image (qcow2 / raw)          disk::open()       -> impl ReadAt
//!     └─ partition table (GPT/MBR)    part::scan()       -> Vec<Partition>
//!          └─ partition slice         blockdev::Slice    -> impl ReadAt
//!               └─ filesystem         fsys::open()       -> Box<dyn FileSystem>
//!                    └─ boot files    boot::find_*()     -> BootEntry, artifacts
//! ```
//!
//! Everything is read-only. The crate is meant to be embedded in a VMM
//! (load kernel/initrd straight into memory) — the `lbx` binary is a thin
//! CLI wrapper over this library.

pub mod blockdev;
pub mod boot;
pub mod disk;
pub mod error;
pub mod fsys;
pub mod part;
pub mod vdafix;

pub use error::{Error, Result};
