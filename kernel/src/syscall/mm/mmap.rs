use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axfs::FileBackend;
use axhal::paging::{MappingFlags, PageSize};
use axtask::current;
use linux_raw_sys::general::*;
use memory_addr::{MemoryAddr, VirtAddr, VirtAddrRange, align_up_4k};
use starry_vm::{vm_load, vm_write_slice};

use crate::{
    file::{File, FileLike},
    mm::{Backend, SharedPages},
    pseudofs::{Device, DeviceMmap},
    task::AsThread,
};

bitflags::bitflags! {
    /// `PROT_*` flags for use with [`sys_mmap`].
    ///
    /// For `PROT_NONE`, use `ProtFlags::empty()`.
    #[derive(Debug, Clone, Copy)]
    struct MmapProt: u32 {
        /// Page can be read.
        const READ = PROT_READ;
        /// Page can be written.
        const WRITE = PROT_WRITE;
        /// Page can be executed.
        const EXEC = PROT_EXEC;
        /// Extend change to start of growsdown vma (mprotect only).
        const GROWDOWN = PROT_GROWSDOWN;
        /// Extend change to start of growsup vma (mprotect only).
        const GROWSUP = PROT_GROWSUP;
    }
}

impl From<MmapProt> for MappingFlags {
    fn from(value: MmapProt) -> Self {
        let mut flags = MappingFlags::USER;
        if value.contains(MmapProt::READ) {
            flags |= MappingFlags::READ;
        }
        if value.contains(MmapProt::WRITE) {
            flags |= MappingFlags::WRITE;
        }
        if value.contains(MmapProt::EXEC) {
            flags |= MappingFlags::EXECUTE;
        }
        flags
    }
}

bitflags::bitflags! {
    /// flags for sys_mmap
    ///
    /// See <https://github.com/bminor/glibc/blob/master/bits/mman.h>
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct MmapFlags: u32 {
        /// Share changes
        const SHARED = MAP_SHARED;
        /// Share changes, but fail if mapping flags contain unknown
        const SHARED_VALIDATE = MAP_SHARED_VALIDATE;
        /// Changes private; copy pages on write.
        const PRIVATE = MAP_PRIVATE;
        /// Map address must be exactly as requested, no matter whether it is available.
        const FIXED = MAP_FIXED;
        /// Same as `FIXED`, but if the requested address overlaps an existing
        /// mapping, the call fails instead of replacing the existing mapping.
        const FIXED_NOREPLACE = MAP_FIXED_NOREPLACE;
        /// Don't use a file.
        const ANONYMOUS = MAP_ANONYMOUS;
        /// Populate the mapping.
        const POPULATE = MAP_POPULATE;
        /// Don't check for reservations.
        const NORESERVE = MAP_NORESERVE;
        /// Allocation is for a stack.
        const STACK = MAP_STACK;
        /// Huge page
        const HUGE = MAP_HUGETLB;
        /// Huge page 1g size
        const HUGE_1GB = MAP_HUGETLB | MAP_HUGE_1GB;
        /// Deprecated flag
        const DENYWRITE = MAP_DENYWRITE;

        /// Mask for type of mapping
        const TYPE = MAP_TYPE;
    }
}

pub fn sys_mmap(
    addr: usize,
    length: usize,
    prot: u32,
    flags: u32,
    fd: i32,
    offset: isize,
) -> AxResult<isize> {
    if length == 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let mut aspace = curr.as_thread().proc_data.aspace.lock();
    let permission_flags = MmapProt::from_bits_truncate(prot);
    let map_flags = match MmapFlags::from_bits(flags) {
        Some(flags) => flags,
        None => {
            warn!("unknown mmap flags: {flags}");
            if (flags & MmapFlags::TYPE.bits()) == MmapFlags::SHARED_VALIDATE.bits() {
                // SHARED_VALIDATE requires all flags to be known.
                return Err(AxError::OperationNotSupported);
            }
            MmapFlags::from_bits_truncate(flags)
        }
    };
    let map_type = map_flags & MmapFlags::TYPE;
    if !matches!(
        map_type,
        MmapFlags::PRIVATE | MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE
    ) {
        return Err(AxError::InvalidInput);
    }
    // MAP_SHARED and MAP_PRIVATE are mutually exclusive.
    if map_flags.contains(MmapFlags::SHARED) && map_flags.contains(MmapFlags::PRIVATE) {
        return Err(AxError::InvalidInput);
    }
    // MAP_FIXED_NOREPLACE implies MAP_FIXED behavior; check for conflict.
    if map_flags.contains(MmapFlags::FIXED_NOREPLACE) && map_flags.contains(MmapFlags::FIXED) {
        // Both are set, FIXED_NOREPLACE takes precedence (this is valid on Linux).
    }
    if map_flags.contains(MmapFlags::ANONYMOUS) != (fd <= 0) {
        return Err(AxError::InvalidInput);
    }
    if fd <= 0 && offset != 0 {
        return Err(AxError::InvalidInput);
    }
    // MAP_FIXED requires a non-null address.
    if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE) && addr == 0 {
        return Err(AxError::InvalidInput);
    }
    let offset: usize = offset.try_into().map_err(|_| AxError::InvalidInput)?;
    if !PageSize::Size4K.is_aligned(offset) {
        return Err(AxError::InvalidInput);
    }

    debug!(
        "sys_mmap <= addr: {addr:#x?}, length: {length:#x?}, prot: {permission_flags:?}, flags: \
         {map_flags:?}, fd: {fd:?}, offset: {offset:?}"
    );

    let page_size = if map_flags.contains(MmapFlags::HUGE_1GB) {
        PageSize::Size1G
    } else if map_flags.contains(MmapFlags::HUGE) {
        PageSize::Size2M
    } else {
        PageSize::Size4K
    };

    let start = addr.align_down(page_size);
    let end = (addr + length).align_up(page_size);
    let mut length = end - start;

    let start = if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE) {
        let dst_addr = VirtAddr::from(start);
        if !map_flags.contains(MmapFlags::FIXED_NOREPLACE) {
            aspace.unmap(dst_addr, length)?;
        }
        dst_addr
    } else {
        let align = page_size as usize;
        aspace
            .find_free_area(
                VirtAddr::from(start),
                length,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                align,
            )
            .or(aspace.find_free_area(
                aspace.base(),
                length,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                align,
            ))
            .ok_or(AxError::NoMemory)?
    };

    let file = if fd > 0 {
        Some(File::from_fd(fd)?)
    } else {
        None
    };

    let backend = match map_type {
        MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE => {
            if let Some(file) = file {
                let file = file.inner();
                let backend = file.backend()?.clone();
                match file.backend()?.clone() {
                    FileBackend::Cached(cache) => {
                        // TODO(mivik): file mmap page size
                        Backend::new_file(
                            start,
                            cache,
                            file.flags(),
                            offset,
                            &curr.as_thread().proc_data.aspace,
                        )
                    }
                    FileBackend::Direct(loc) => {
                        let device = loc
                            .entry()
                            .downcast::<Device>()
                            .map_err(|_| AxError::NoSuchDevice)?;

                        match device.mmap() {
                            DeviceMmap::None => {
                                return Err(AxError::NoSuchDevice);
                            }
                            DeviceMmap::ReadOnly => {
                                Backend::new_cow(start, page_size, backend, offset as u64, None)
                            }
                            DeviceMmap::Physical(mut range) => {
                                range.start += offset;
                                if range.is_empty() {
                                    return Err(AxError::InvalidInput);
                                }
                                length = length.min(range.size().align_down(page_size));
                                Backend::new_linear(
                                    start.as_usize() as isize - range.start.as_usize() as isize,
                                )
                            }
                            DeviceMmap::Cache(cache) => Backend::new_file(
                                start,
                                cache,
                                file.flags(),
                                offset,
                                &curr.as_thread().proc_data.aspace,
                            ),
                        }
                    }
                }
            } else {
                Backend::new_shared(start, Arc::new(SharedPages::new(length, PageSize::Size4K)?))
            }
        }
        MmapFlags::PRIVATE => {
            if let Some(file) = file {
                // Private mapping from a file
                let backend = file.inner().backend()?.clone();
                Backend::new_cow(start, page_size, backend, offset as u64, None)
            } else {
                Backend::new_alloc(start, page_size)
            }
        }
        _ => return Err(AxError::InvalidInput),
    };

    let populate = map_flags.contains(MmapFlags::POPULATE);
    aspace.map(start, length, permission_flags.into(), populate, backend)?;

    Ok(start.as_usize() as _)
}

pub fn sys_munmap(addr: usize, length: usize) -> AxResult<isize> {
    debug!("sys_munmap <= addr: {addr:#x}, length: {length:x}");
    let curr = current();
    let mut aspace = curr.as_thread().proc_data.aspace.lock();
    let length = align_up_4k(length);
    let start_addr = VirtAddr::from(addr);
    aspace.unmap(start_addr, length)?;
    Ok(0)
}

pub fn sys_mprotect(addr: usize, length: usize, prot: u32) -> AxResult<isize> {
    // TODO: implement PROT_GROWSUP & PROT_GROWSDOWN
    let Some(permission_flags) = MmapProt::from_bits(prot) else {
        return Err(AxError::InvalidInput);
    };
    debug!("sys_mprotect <= addr: {addr:#x}, length: {length:x}, prot: {permission_flags:?}");

    if permission_flags.contains(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let mut aspace = curr.as_thread().proc_data.aspace.lock();
    let length = align_up_4k(length);
    let start_addr = VirtAddr::from(addr);
    aspace.protect(start_addr, length, permission_flags.into())?;

    Ok(0)
}

/// Linux mremap flags.
const MREMAP_MAYMOVE: u32 = 1;
const MREMAP_FIXED: u32 = 2;

pub fn sys_mremap(addr: usize, old_size: usize, new_size: usize, flags: u32) -> AxResult<isize> {
    debug!(
        "sys_mremap <= addr: {addr:#x}, old_size: {old_size:x}, new_size: {new_size:x}, flags: \
         {flags:#x}"
    );

    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    if new_size == 0 {
        return Err(AxError::InvalidInput);
    }
    // MREMAP_FIXED requires MREMAP_MAYMOVE
    if (flags & MREMAP_FIXED) != 0 && (flags & MREMAP_MAYMOVE) == 0 {
        return Err(AxError::InvalidInput);
    }

    let addr = VirtAddr::from(addr);
    let old_size = align_up_4k(old_size);
    let new_size = align_up_4k(new_size);

    // Shrinking: just unmap the tail.
    if new_size < old_size {
        let shrink_start = addr + new_size;
        sys_munmap(shrink_start.as_usize(), old_size - new_size)?;
        return Ok(addr.as_usize() as isize);
    }

    // Same size: nothing to do.
    if new_size == old_size {
        return Ok(addr.as_usize() as isize);
    }

    // Expanding: try to grow in-place first.
    let expand_size = new_size - old_size;
    let expand_start = addr + old_size;

    let curr = current();
    let mut aspace = curr.as_thread().proc_data.aspace.lock();

    let area_flags = aspace.find_area(addr).ok_or(AxError::NoMemory)?.flags();

    // Check if the region right after the old mapping is free.
    let can_expand_in_place = aspace
        .find_free_area(
            expand_start,
            expand_size,
            VirtAddrRange::new(expand_start, expand_start + expand_size),
            PageSize::Size4K as usize,
        )
        .is_some();

    if can_expand_in_place {
        // Expand in-place by mapping the extension region with the same flags.
        aspace.map(
            expand_start,
            expand_size,
            area_flags,
            false,
            Backend::new_alloc(expand_start, PageSize::Size4K),
        )?;
        return Ok(addr.as_usize() as isize);
    }

    // Cannot expand in-place.
    if (flags & MREMAP_MAYMOVE) == 0 {
        return Err(AxError::NoMemory);
    }

    // Allocate a new region and move the data.
    let new_addr = aspace
        .find_free_area(
            aspace.base(),
            new_size,
            VirtAddrRange::new(aspace.base(), aspace.end()),
            PageSize::Size4K as usize,
        )
        .ok_or(AxError::NoMemory)?;

    aspace.map(
        new_addr,
        new_size,
        area_flags,
        false,
        Backend::new_alloc(new_addr, PageSize::Size4K),
    )?;
    drop(aspace);

    // Copy data from old region to new region.
    let copy_len = old_size;
    let data = vm_load(addr.as_ptr(), copy_len)?;
    vm_write_slice(new_addr.as_usize() as *mut u8, &data)?;

    sys_munmap(addr.as_usize(), old_size)?;

    Ok(new_addr.as_usize() as isize)
}

pub fn sys_madvise(addr: usize, length: usize, advice: i32) -> AxResult<isize> {
    debug!("sys_madvise <= addr: {addr:#x}, length: {length:x}, advice: {advice:#x}");

    if !VirtAddr::from(addr).is_aligned_4k() {
        return Err(AxError::InvalidInput);
    }
    if length == 0 {
        return Ok(0);
    }

    let advice = advice as u32;
    match advice {
        // Advisory hints — no-op in this kernel, return success.
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL | MADV_COLD | MADV_PAGEOUT => Ok(0),
        // MADV_WILLNEED: prefetch pages (advisory, errors ignored).
        MADV_WILLNEED => {
            let length = align_up_4k(length);
            let curr = current();
            let mut aspace = curr.as_thread().proc_data.aspace.lock();
            let _ = aspace.populate_area(VirtAddr::from(addr), length, MappingFlags::READ);
            Ok(0)
        }
        // MADV_DONTNEED: zap PTEs and release physical pages, VMA preserved.
        // Next access triggers page fault → re-allocates zero page (anon) or re-reads file.
        MADV_DONTNEED | MADV_DONTNEED_LOCKED => {
            let length = align_up_4k(length);
            let curr = current();
            let mut aspace = curr.as_thread().proc_data.aspace.lock();
            aspace.zap_pages(VirtAddr::from(addr), length)?;
            Ok(0)
        }
        // MADV_FREE: lazy page reclaim, equivalent to DONTNEED without page reclaim daemon.
        MADV_FREE => {
            let length = align_up_4k(length);
            let curr = current();
            let mut aspace = curr.as_thread().proc_data.aspace.lock();
            aspace.zap_pages(VirtAddr::from(addr), length)?;
            Ok(0)
        }
        // Fork/dump/KSM/THP hints — no-op.
        MADV_DONTFORK | MADV_DOFORK | MADV_MERGEABLE | MADV_UNMERGEABLE | MADV_HUGEPAGE
        | MADV_NOHUGEPAGE | MADV_DONTDUMP | MADV_DODUMP | MADV_WIPEONFORK | MADV_KEEPONFORK
        | MADV_COLLAPSE => Ok(0),
        // MADV_POPULATE_READ: fault in pages with read access, propagate errors.
        MADV_POPULATE_READ => {
            let length = align_up_4k(length);
            let curr = current();
            let mut aspace = curr.as_thread().proc_data.aspace.lock();
            aspace.populate_area(VirtAddr::from(addr), length, MappingFlags::READ)?;
            Ok(0)
        }
        // MADV_POPULATE_WRITE: fault in pages with write access, propagate errors.
        MADV_POPULATE_WRITE => {
            let length = align_up_4k(length);
            let curr = current();
            let mut aspace = curr.as_thread().proc_data.aspace.lock();
            aspace.populate_area(
                VirtAddr::from(addr),
                length,
                MappingFlags::READ | MappingFlags::WRITE,
            )?;
            Ok(0)
        }
        // Guard pages — no-op.
        MADV_GUARD_INSTALL | MADV_GUARD_REMOVE => Ok(0),
        // MADV_REMOVE: works only on shared/tmpfs, not supported.
        MADV_REMOVE => Err(AxError::InvalidInput),
        // Hardware poison — privileged, not supported.
        MADV_HWPOISON | MADV_SOFT_OFFLINE => Err(AxError::PermissionDenied),
        _ => {
            warn!("sys_madvise: unknown advice {advice}");
            Err(AxError::InvalidInput)
        }
    }
}

pub fn sys_msync(addr: usize, length: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_msync <= addr: {addr:#x}, length: {length:x}, flags: {flags:#x}");

    if !VirtAddr::from(addr).is_aligned_4k() {
        return Err(AxError::InvalidInput);
    }
    // Validate flags: at least one of MS_ASYNC or MS_SYNC must be set, but not both.
    let has_async = (flags & MS_ASYNC) != 0;
    let has_sync = (flags & MS_SYNC) != 0;
    if has_async && has_sync {
        return Err(AxError::InvalidInput);
    }
    // MS_ASYNC: since Linux 2.6.19+ the kernel automatically tracks dirty
    // pages, so MS_ASYNC is effectively a no-op.
    if has_async {
        return Ok(0);
    }
    // MS_SYNC: synchronously flush dirty pages to backing store.
    let length = align_up_4k(length);
    let curr = current();
    let mut aspace = curr.as_thread().proc_data.aspace.lock();
    aspace.sync_range(VirtAddr::from(addr), length)?;
    Ok(0)
}

pub fn sys_mlock(addr: usize, length: usize) -> AxResult<isize> {
    sys_mlock2(addr, length, 0)
}

pub fn sys_mlock2(_addr: usize, _length: usize, _flags: u32) -> AxResult<isize> {
    Ok(0)
}

pub fn sys_munlock(_addr: usize, _length: usize) -> AxResult<isize> {
    Ok(0)
}

pub fn sys_mlockall(_flags: u32) -> AxResult<isize> {
    Ok(0)
}

pub fn sys_munlockall() -> AxResult<isize> {
    Ok(0)
}
