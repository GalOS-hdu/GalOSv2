use alloc::{sync::Arc, vec::Vec};
use core::ops::Deref;

use axerrno::AxResult;
use axhal::paging::{MappingFlags, PageSize, PageTableCursor, PagingError};
use axsync::Mutex;
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    AddrSpace, AtomicVmFlags, Backend, BackendOps, PopulateCallback, VmFlags, alloc_frame,
    dealloc_frame, divide_page, pages_in,
};

pub struct SharedPages {
    pub phys_pages: Vec<PhysAddr>,
    pub size: PageSize,
}
impl SharedPages {
    pub fn new(size: usize, page_size: PageSize) -> AxResult<Self> {
        let num_pages = divide_page(size, page_size);
        let mut result = Self {
            phys_pages: Vec::with_capacity(num_pages),
            size: page_size,
        };
        for _ in 0..num_pages {
            result.phys_pages.push(alloc_frame(true, page_size)?);
        }
        Ok(result)
    }

    pub fn len(&self) -> usize {
        self.phys_pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.phys_pages.is_empty()
    }
}

impl Deref for SharedPages {
    type Target = [PhysAddr];

    fn deref(&self) -> &Self::Target {
        &self.phys_pages
    }
}

impl Drop for SharedPages {
    fn drop(&mut self) {
        for frame in &self.phys_pages {
            dealloc_frame(*frame, self.size);
        }
    }
}

// FIXME: This implementation does not allow map or unmap partial ranges.
#[derive(Clone)]
pub struct SharedBackend {
    start: VirtAddr,
    pages: Arc<SharedPages>,
    vm_flags: AtomicVmFlags,
}
impl SharedBackend {
    pub fn pages(&self) -> &Arc<SharedPages> {
        &self.pages
    }

    fn pages_starting_from(&self, start: VirtAddr) -> &[PhysAddr] {
        debug_assert!(start.is_aligned(self.pages.size));
        let start_index = divide_page(start - self.start, self.pages.size);
        &self.pages[start_index..]
    }
}

impl BackendOps for SharedBackend {
    fn page_size(&self) -> PageSize {
        self.pages.size
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pt: &mut PageTableCursor) -> AxResult {
        debug!("Shared::map: {:?} {:?}", range, flags);
        for (vaddr, paddr) in
            pages_in(range, self.pages.size)?.zip(self.pages_starting_from(range.start))
        {
            pt.map(vaddr, *paddr, self.pages.size, flags)?;
        }
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult {
        debug!("Shared::unmap: {:?}", range);
        for vaddr in pages_in(range, self.pages.size)? {
            pt.unmap(vaddr)?;
        }
        Ok(())
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pt: &mut PageTableCursor,
        _new_pt: &mut PageTableCursor,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Backend> {
        Ok(Backend::Shared(self.clone()))
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult<(usize, Option<PopulateCallback>)> {
        let mut pages = 0;
        for (vaddr, paddr) in
            pages_in(range, self.pages.size)?.zip(self.pages_starting_from(range.start))
        {
            match pt.query(vaddr) {
                Ok((_, page_flags, _)) => {
                    if page_flags.contains(access_flags) {
                        pages += 1;
                    }
                }
                Err(PagingError::NotMapped) => {
                    pt.map(vaddr, *paddr, self.pages.size, flags)?;
                    pages += 1;
                }
                Err(_) => return Err(axerrno::AxError::BadAddress),
            }
        }
        Ok((pages, None))
    }

    fn zap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult<usize> {
        let mut count = 0;
        for addr in pages_in(range, self.pages.size)? {
            // Only clear the PTE; the shared physical pages are still owned
            // by SharedPages and used by other processes.
            if pt.unmap(addr).is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    fn vm_flags(&self) -> VmFlags {
        self.vm_flags.load()
    }

    fn set_vm_flags(&self, flags: VmFlags, set: bool) {
        if set {
            self.vm_flags.insert(flags);
        } else {
            self.vm_flags.remove(flags);
        }
    }
}

impl Backend {
    pub fn new_shared(start: VirtAddr, pages: Arc<SharedPages>) -> Self {
        Self::Shared(SharedBackend {
            start,
            pages,
            vm_flags: AtomicVmFlags::new(VmFlags::SHARED),
        })
    }
}
