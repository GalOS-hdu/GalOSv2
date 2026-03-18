//! Memory mapping backends.
use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU32, Ordering};

use axalloc::{UsageKind, global_allocator};
use axerrno::{AxError, AxResult};
use axhal::{
    mem::{phys_to_virt, virt_to_phys},
    paging::{MappingFlags, PageSize, PageTable, PageTableCursor},
};
use axsync::Mutex;
use enum_dispatch::enum_dispatch;
use memory_addr::{DynPageIter, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};
use memory_set::MappingBackend;

mod cow;
mod file;
mod linear;
mod shared;

pub use self::shared::SharedPages;
use super::AddrSpace;

bitflags::bitflags! {
    /// Per-VMA metadata flags mirroring Linux `VM_*` flags.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct VmFlags: u32 {
        /// Mapping is shared (MAP_SHARED).
        const SHARED      = 1 << 0;
        /// Stack grows downward (MAP_GROWSDOWN).
        const GROWSDOWN   = 1 << 1;
        /// Do not copy on fork (MADV_DONTFORK / VM_DONTCOPY).
        const DONTCOPY    = 1 << 2;
        /// Pages are locked in memory (MAP_LOCKED / mlock).
        const LOCKED      = 1 << 3;
        /// Do not expand with mremap (MADV_DONTEXPAND).
        const DONTEXPAND  = 1 << 4;
        /// Exclude from core dumps (MADV_DONTDUMP).
        const DONTDUMP    = 1 << 5;
        /// Zero pages on fork (MADV_WIPEONFORK).
        const WIPEONFORK  = 1 << 6;
        /// Huge page mapping (MAP_HUGETLB).
        const HUGETLB     = 1 << 7;
    }
}

/// Atomic storage for VmFlags, allowing mutation through `&self`.
#[derive(Debug)]
pub struct AtomicVmFlags(AtomicU32);

impl AtomicVmFlags {
    pub const fn new(flags: VmFlags) -> Self {
        Self(AtomicU32::new(flags.bits()))
    }

    pub fn load(&self) -> VmFlags {
        VmFlags::from_bits_truncate(self.0.load(Ordering::Relaxed))
    }

    pub fn insert(&self, flags: VmFlags) {
        self.0.fetch_or(flags.bits(), Ordering::Relaxed);
    }

    pub fn remove(&self, flags: VmFlags) {
        self.0.fetch_and(!flags.bits(), Ordering::Relaxed);
    }
}

impl Clone for AtomicVmFlags {
    fn clone(&self) -> Self {
        Self(AtomicU32::new(self.0.load(Ordering::Relaxed)))
    }
}

impl Default for AtomicVmFlags {
    fn default() -> Self {
        Self::new(VmFlags::empty())
    }
}

fn divide_page(size: usize, page_size: PageSize) -> usize {
    assert!(page_size.is_aligned(size), "unaligned");
    size >> (page_size as usize).trailing_zeros()
}

fn alloc_frame(zeroed: bool, size: PageSize) -> AxResult<PhysAddr> {
    let page_size = size as usize;
    let num_pages = page_size / PAGE_SIZE_4K;
    let vaddr =
        VirtAddr::from(global_allocator().alloc_pages(num_pages, page_size, UsageKind::VirtMem)?);
    if zeroed {
        unsafe { core::ptr::write_bytes(vaddr.as_mut_ptr(), 0, page_size) };
    }
    let paddr = virt_to_phys(vaddr);

    Ok(paddr)
}

fn dealloc_frame(frame: PhysAddr, align: PageSize) {
    let vaddr = phys_to_virt(frame);
    let page_size: usize = align.into();
    let num_pages = page_size / PAGE_SIZE_4K;
    global_allocator().dealloc_pages(vaddr.as_usize(), num_pages, UsageKind::VirtMem);
}

fn pages_in(range: VirtAddrRange, align: PageSize) -> AxResult<DynPageIter<VirtAddr>> {
    DynPageIter::new(range.start, range.end, align as usize).ok_or(AxError::InvalidInput)
}

type PopulateCallback = Box<dyn FnOnce(&mut AddrSpace)>;

#[enum_dispatch]
pub trait BackendOps {
    /// Returns the page size of the backend.
    fn page_size(&self) -> PageSize;

    /// Map a memory region.
    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pt: &mut PageTableCursor) -> AxResult;

    /// Unmap a memory region.
    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult;

    /// Called before a memory region is protected.
    fn on_protect(
        &self,
        _range: VirtAddrRange,
        _new_flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        Ok(())
    }

    /// Populate a memory region and return how many pages now satisfy
    /// `access_flags`.
    ///
    /// If another thread has already mapped the page with sufficient permissions,
    /// treat it as populated.
    fn populate(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _access_flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult<(usize, Option<PopulateCallback>)> {
        Ok((0, None))
    }

    /// Duplicates this mapping for use in a different page table.
    ///
    /// This differs from `clone`, which is designed for splitting a mapping
    /// within the same table.
    ///
    /// [`BackendOps::map`] will be latter called to the returned backend.
    fn clone_map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pt: &mut PageTableCursor,
        new_pt: &mut PageTableCursor,
        new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Backend>;

    /// Discard physical pages in the given range without removing the VMA.
    ///
    /// After zap, accessing the range triggers a page fault which re-populates
    /// via [`BackendOps::populate`].  Returns the number of pages zapped.
    fn zap(&self, _range: VirtAddrRange, _pt: &mut PageTableCursor) -> AxResult<usize> {
        Ok(0)
    }

    /// Synchronize dirty pages in the given range to the backing store.
    ///
    /// Only meaningful for file-backed mappings; default is a no-op.
    fn sync(&self, _range: VirtAddrRange, _pt: &mut PageTableCursor) -> AxResult {
        Ok(())
    }

    /// Returns the VMA metadata flags.
    fn vm_flags(&self) -> VmFlags {
        VmFlags::empty()
    }

    /// Sets or clears VMA metadata flags.
    fn set_vm_flags(&self, _flags: VmFlags, _set: bool) {}
}

/// A unified enum type for different memory mapping backends.
#[derive(Clone)]
#[enum_dispatch(BackendOps)]
pub enum Backend {
    Linear(linear::LinearBackend),
    Cow(cow::CowBackend),
    Shared(shared::SharedBackend),
    File(file::FileBackend),
}

impl MappingBackend for Backend {
    type Addr = VirtAddr;
    type Flags = MappingFlags;
    type PageTable = PageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MappingFlags, pt: &mut PageTable) -> bool {
        let range = VirtAddrRange::from_start_size(start, size);
        if let Err(err) = BackendOps::map(self, range, flags, &mut pt.cursor()) {
            warn!("Failed to map area: {:?}", err);
            false
        } else {
            true
        }
    }

    fn unmap(&self, start: VirtAddr, size: usize, pt: &mut PageTable) -> bool {
        let range = VirtAddrRange::from_start_size(start, size);
        if let Err(err) = BackendOps::unmap(self, range, &mut pt.cursor()) {
            warn!("Failed to unmap area: {:?}", err);
            false
        } else {
            true
        }
    }

    fn protect(
        &self,
        start: Self::Addr,
        size: usize,
        new_flags: Self::Flags,
        pt: &mut Self::PageTable,
    ) -> bool {
        let range = VirtAddrRange::from_start_size(start, size);
        let mut cursor = pt.cursor();
        if let Err(err) = BackendOps::on_protect(self, range, new_flags, &mut cursor) {
            warn!("Failed to protect area: {:?}", err);
            return false;
        }
        cursor.protect_region(start, size, new_flags).is_ok()
    }
}
