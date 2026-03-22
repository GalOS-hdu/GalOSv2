use alloc::{sync::Arc, vec::Vec};
use hashbrown::HashMap;
use core::slice;

use axerrno::{AxError, AxResult};
use axfs::FileBackend;
use axhal::{
    mem::phys_to_virt,
    paging::{MappingFlags, PageSize, PageTableCursor, PagingError},
};
use axsync::Mutex;
use kspin::SpinNoIrq;
use memory_addr::{PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    AddrSpace, AtomicVmFlags, Backend, BackendOps, PopulateCallback, VmFlags, alloc_frame,
    dealloc_frame, pages_in,
};

struct FrameRefCnt(u32);

impl FrameRefCnt {
    // This function may lock FRAME_TABLE again, so the caller should drop the lock first.
    fn drop_frame(&mut self, paddr: PhysAddr, page_size: PageSize) {
        assert!(self.0 > 0, "dropping unreferenced frame");
        self.0 -= 1;
        if self.0 == 0 {
            // Remove the frame from FRAME_TABLE before deallocating it to avoid a race:
            // if we dealloc the frame first, another thread could allocate the same
            // physical frame before we remove the table entry. This function assumes
            // the caller is not holding the FRAME_TABLE lock, so it is safe to lock
            // FRAME_TABLE here and perform the removal.
            FRAME_TABLE.lock().remove_frame(paddr);
            dealloc_frame(paddr, page_size);
        }
    }
}

struct FrameTableRefCount {
    table: Option<HashMap<usize, Arc<SpinNoIrq<FrameRefCnt>>>>,
}

impl FrameTableRefCount {
    const INITIAL_CNT: u32 = 1;

    const fn new() -> Self {
        Self {
            table: None,
        }
    }

    fn table_mut(&mut self) -> &mut HashMap<usize, Arc<SpinNoIrq<FrameRefCnt>>> {
        self.table.get_or_insert_with(HashMap::new)
    }

    fn get_frame_ref(&self, paddr: PhysAddr) -> Option<Arc<SpinNoIrq<FrameRefCnt>>> {
        self.table.as_ref()?.get(&paddr.as_usize()).cloned()
    }

    fn init_frame(&mut self, paddr: PhysAddr) {
        let table = self.table_mut();
        assert!(
            !table.contains_key(&paddr.as_usize()),
            "initializing already referenced frame"
        );
        table.insert(
            paddr.as_usize(),
            Arc::new(SpinNoIrq::new(FrameRefCnt(Self::INITIAL_CNT))),
        );
    }

    fn remove_frame(&mut self, paddr: PhysAddr) {
        let table = self.table_mut();
        assert!(
            table.contains_key(&paddr.as_usize()),
            "removing unreferenced frame"
        );
        table.remove(&paddr.as_usize());
    }
}

static FRAME_TABLE: SpinNoIrq<FrameTableRefCount> = SpinNoIrq::new(FrameTableRefCount::new());

/// Copy-on-write mapping backend.
///
/// This corresponds to the `MAP_PRIVATE` flag.
#[derive(Clone)]
pub struct CowBackend {
    start: VirtAddr,
    size: PageSize,
    file: Option<(FileBackend, u64, Option<u64>)>,
    vm_flags: AtomicVmFlags,
}

impl CowBackend {
    fn alloc_new_frame(&self, zeroed: bool) -> AxResult<PhysAddr> {
        let frame = alloc_frame(zeroed, self.size)?;
        FRAME_TABLE.lock().init_frame(frame);
        Ok(frame)
    }

    fn alloc_new_at(
        &self,
        vaddr: VirtAddr,
        flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        let frame = self.alloc_new_frame(true)?;

        if let Some((file, file_start, file_end)) = &self.file {
            let buf = unsafe {
                slice::from_raw_parts_mut(phys_to_virt(frame).as_mut_ptr(), self.size as _)
            };
            // vaddr can be smaller than self.start (at most 1 page) due to
            // non-aligned mappings, we need to keep the gap clean.
            let start = self.start.as_usize().saturating_sub(vaddr.as_usize());
            assert!(start < self.size as _);

            let file_start =
                *file_start + vaddr.as_usize().saturating_sub(self.start.as_usize()) as u64;
            let max_read = file_end
                .map_or(u64::MAX, |end| end.saturating_sub(file_start))
                .min((buf.len() - start) as u64) as usize;

            file.read_at(&mut &mut buf[start..start + max_read], file_start)?;
        }
        pt.map(vaddr, frame, self.size, flags)?;
        Ok(())
    }

    fn handle_cow_fault(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        let mut frame_table = FRAME_TABLE.lock();
        let frame = frame_table
            .get_frame_ref(paddr)
            .ok_or(AxError::BadAddress)?;
        drop(frame_table);
        let mut frame = frame.lock();
        assert!(frame.0 > 0, "invalid frame reference count");
        match frame.0 {
            1 => {
                // Only one reference, just upgrade the permissions.
                pt.protect(vaddr, flags)?;
                return Ok(());
            }
            _ => {
                // Multiple references, need to copy the frame.
                let new_frame = self.alloc_new_frame(false)?;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        phys_to_virt(paddr).as_ptr(),
                        phys_to_virt(new_frame).as_mut_ptr(),
                        self.size as _,
                    );
                }
                pt.remap(vaddr, new_frame, flags)?;
                frame.drop_frame(paddr, self.size);
            }
        }

        Ok(())
    }
}

impl BackendOps for CowBackend {
    fn page_size(&self) -> PageSize {
        self.size
    }

    fn map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        debug!("Cow::map: {range:?} {flags:?}",);
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult {
        debug!("Cow::unmap: {range:?}");
        for addr in pages_in(range, self.size)? {
            if let Ok((frame, _flags, page_size)) = pt.unmap(addr) {
                assert_eq!(page_size, self.size);
                let frame_ref = FRAME_TABLE
                    .lock()
                    .get_frame_ref(frame)
                    .ok_or(AxError::BadAddress)?;
                let mut frame_ref = frame_ref.lock();
                frame_ref.drop_frame(frame, self.size);
            } else {
                // Deallocation is needn't if the page is not allocated.
            }
        }
        Ok(())
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult<(usize, Option<PopulateCallback>)> {
        let mut pages = 0;
        for addr in pages_in(range, self.size)? {
            match pt.query(addr) {
                Ok((paddr, page_flags, page_size)) => {
                    assert_eq!(self.size, page_size);
                    if access_flags.contains(MappingFlags::WRITE)
                        && !page_flags.contains(MappingFlags::WRITE)
                    {
                        self.handle_cow_fault(addr, paddr, flags, pt)?;
                        pages += 1;
                    } else if page_flags.contains(access_flags) {
                        pages += 1;
                    }
                }
                // If the page is not mapped, try map it.
                Err(PagingError::NotMapped) => {
                    self.alloc_new_at(addr, flags, pt)?;
                    pages += 1;
                }
                Err(_) => return Err(AxError::BadAddress),
            }
        }
        Ok((pages, None))
    }

    fn clone_map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pt: &mut PageTableCursor,
        new_pt: &mut PageTableCursor,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Backend> {
        let cow_flags = flags - MappingFlags::WRITE;

        // Phase 1: collect all mapped pages from the old page table.
        let mapped_pages: Vec<(VirtAddr, PhysAddr)> = pages_in(range, self.size)?
            .filter_map(|vaddr| match old_pt.query(vaddr) {
                Ok((paddr, _, page_size)) => {
                    assert_eq!(page_size, self.size);
                    Some(Ok((vaddr, paddr)))
                }
                Err(PagingError::NotMapped) => None,
                Err(_) => Some(Err(AxError::BadAddress)),
            })
            .collect::<AxResult<Vec<_>>>()?;

        // Phase 2: batch-acquire all frame refs under a single FRAME_TABLE lock.
        let frame_refs: Vec<_> = {
            let mut ft = FRAME_TABLE.lock();
            mapped_pages
                .iter()
                .map(|(_, paddr)| ft.get_frame_ref(*paddr).ok_or(AxError::BadAddress))
                .collect::<AxResult<Vec<_>>>()?
        }; // FRAME_TABLE lock dropped

        // Phase 3: increment refcounts and update page tables.
        for ((vaddr, paddr), frame_ref) in mapped_pages.iter().zip(frame_refs.iter()) {
            let mut frame = frame_ref.lock();
            assert!(frame.0 > 0, "referencing unreferenced frame");
            if frame.0 == u32::MAX {
                warn!("frame reference count overflow");
                return Err(AxError::NoMemory);
            }
            frame.0 += 1;
            old_pt.protect(*vaddr, cow_flags)?;
            new_pt.map(*vaddr, *paddr, self.size, cow_flags)?;
        }

        Ok(Backend::Cow(self.clone()))
    }

    fn zap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult<usize> {
        let mut count = 0;
        for addr in pages_in(range, self.size)? {
            if let Ok((frame, _flags, page_size)) = pt.unmap(addr) {
                assert_eq!(page_size, self.size);
                if let Some(frame_ref) = FRAME_TABLE.lock().get_frame_ref(frame) {
                    let mut frame_ref = frame_ref.lock();
                    frame_ref.drop_frame(frame, self.size);
                }
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
    pub fn new_cow(
        start: VirtAddr,
        size: PageSize,
        file: FileBackend,
        file_start: u64,
        file_end: Option<u64>,
    ) -> Self {
        Self::Cow(CowBackend {
            start,
            size,
            file: Some((file, file_start, file_end)),
            vm_flags: AtomicVmFlags::default(),
        })
    }

    pub fn new_alloc(start: VirtAddr, size: PageSize) -> Self {
        Self::Cow(CowBackend {
            start,
            size,
            file: None,
            vm_flags: AtomicVmFlags::default(),
        })
    }
}
