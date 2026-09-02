// SPDX-License-Identifier: GPL-3.0-only

//! WMDE: the shared memory a captured frame is written into.

use std::os::fd::OwnedFd;

/// Somewhere for the compositor to put a frame. Shared memory rather than a dmabuf: a
/// screenshot is read once by the process that asked for it, so there is nothing for a
/// GPU buffer to save.
pub fn create_memfd(width: u32, height: u32) -> OwnedFd {
    // TODO: BSD support using shm_open
    let name = c"wmde-screenshot";
    let fd = rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC).unwrap(); // XXX
    rustix::fs::ftruncate(&fd, (width * height * 4) as _).unwrap();
    fd
}
