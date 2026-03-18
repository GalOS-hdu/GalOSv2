use axerrno::{AxError, AxResult};
use axtask::current;
use memory_addr::VirtAddr;
use starry_process::Pid;
use starry_vm::vm_load;

use crate::{
    mm::IoVec,
    task::{AsThread, get_process_data},
};

/// Check whether the caller has permission to access the target process's
/// address space.  Currently all processes run as root so the check always
/// passes, but this establishes the correct code path for when UID-based
/// permission checks are added later.
fn check_process_vm_access(pid: Pid) -> AxResult<()> {
    let curr = current();
    let caller_pid = curr.as_thread().proc_data.proc.pid();
    if pid == caller_pid {
        return Ok(());
    }
    // TODO: check CAP_SYS_PTRACE once UID support is implemented.
    Ok(())
}

/// Read data from the address space of another process.
///
/// `process_vm_readv` transfers data from the remote process (identified by
/// `pid`) into the local process.  Each `remote_iov` entry describes a
/// contiguous region in the remote address space, while each `local_iov`
/// entry describes where to place the data locally.
pub fn sys_process_vm_readv(
    pid: Pid,
    local_iov: *const IoVec,
    liovcnt: usize,
    remote_iov: *const IoVec,
    riovcnt: usize,
    _flags: u32,
) -> AxResult<isize> {
    debug!("sys_process_vm_readv <= pid: {pid}, liovcnt: {liovcnt}, riovcnt: {riovcnt}");

    if liovcnt > 1024 || riovcnt > 1024 {
        return Err(AxError::InvalidInput);
    }

    check_process_vm_access(pid)?;

    // Read iovec arrays from user space.
    let local_iovs = read_iovecs(local_iov, liovcnt)?;
    let remote_iovs = read_iovecs(remote_iov, riovcnt)?;

    // Find the target process.
    let target = get_process_data(pid)?;
    let target_aspace = target.aspace.lock();

    let mut total = 0usize;
    let mut local_iter = IovecIter::new(&local_iovs);

    for riov in &remote_iovs {
        let base = riov.iov_base as usize;
        let len = riov.iov_len as usize;
        if len == 0 {
            continue;
        }

        // Read from remote address space into a kernel buffer.
        let mut buf = alloc::vec![0u8; len];
        target_aspace.read(VirtAddr::from(base), &mut buf)?;

        // Copy kernel buffer into local iovec entries.
        let mut offset = 0;
        while offset < len {
            let Some((dst, avail)) = local_iter.next_chunk() else {
                total += offset;
                return Ok(total as isize);
            };
            let n = avail.min(len - offset);
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr().add(offset), dst, n);
            }
            local_iter.advance(n);
            offset += n;
        }
        total += len;
    }

    Ok(total as isize)
}

/// Write data into the address space of another process.
///
/// `process_vm_writev` transfers data from the local process into the remote
/// process (identified by `pid`).
pub fn sys_process_vm_writev(
    pid: Pid,
    local_iov: *const IoVec,
    liovcnt: usize,
    remote_iov: *const IoVec,
    riovcnt: usize,
    _flags: u32,
) -> AxResult<isize> {
    debug!("sys_process_vm_writev <= pid: {pid}, liovcnt: {liovcnt}, riovcnt: {riovcnt}");

    if liovcnt > 1024 || riovcnt > 1024 {
        return Err(AxError::InvalidInput);
    }

    check_process_vm_access(pid)?;

    let local_iovs = read_iovecs(local_iov, liovcnt)?;
    let remote_iovs = read_iovecs(remote_iov, riovcnt)?;

    let target = get_process_data(pid)?;
    let target_aspace = target.aspace.lock();

    let mut total = 0usize;
    let mut local_iter = IovecIter::new(&local_iovs);

    for riov in &remote_iovs {
        let base = riov.iov_base as usize;
        let len = riov.iov_len as usize;
        if len == 0 {
            continue;
        }

        // Gather data from local iovecs into a kernel buffer.
        let mut buf = alloc::vec![0u8; len];
        let mut offset = 0;
        while offset < len {
            let Some((src, avail)) = local_iter.next_chunk() else {
                if offset > 0 {
                    target_aspace.write(VirtAddr::from(base), &buf[..offset])?;
                    total += offset;
                }
                return Ok(total as isize);
            };
            let n = avail.min(len - offset);
            unsafe {
                core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr().add(offset), n);
            }
            local_iter.advance(n);
            offset += n;
        }

        // Write the full buffer into the remote address space.
        target_aspace.write(VirtAddr::from(base), &buf)?;
        total += len;
    }

    Ok(total as isize)
}

/// Read an array of IoVec structs from user space.
fn read_iovecs(iov: *const IoVec, cnt: usize) -> AxResult<alloc::vec::Vec<IoVec>> {
    let iovecs = vm_load(iov, cnt)?;
    for v in &iovecs {
        if v.iov_len < 0 {
            return Err(AxError::InvalidInput);
        }
    }
    Ok(iovecs)
}

/// Iterator over a slice of IoVec entries, tracking the current position.
struct IovecIter<'a> {
    iovs: &'a [IoVec],
    idx: usize,
    off: usize,
}

impl<'a> IovecIter<'a> {
    fn new(iovs: &'a [IoVec]) -> Self {
        Self {
            iovs,
            idx: 0,
            off: 0,
        }
    }

    /// Returns the current chunk pointer and the number of available bytes,
    /// or None if exhausted.
    fn next_chunk(&self) -> Option<(*mut u8, usize)> {
        if self.idx < self.iovs.len() {
            let iov = &self.iovs[self.idx];
            let len = iov.iov_len as usize;
            if self.off < len {
                let ptr = unsafe { iov.iov_base.add(self.off) };
                return Some((ptr, len - self.off));
            }
        }
        None
    }

    /// Advance the iterator by `n` bytes.
    fn advance(&mut self, n: usize) {
        self.off += n;
        while self.idx < self.iovs.len() {
            let len = self.iovs[self.idx].iov_len as usize;
            if self.off < len {
                break;
            }
            self.off -= len;
            self.idx += 1;
        }
    }
}
