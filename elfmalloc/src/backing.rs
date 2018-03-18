use std::marker::PhantomData;
use std::mem;
use std::ptr::{self, NonNull};

use alloc::allocator::Layout;
use bagpipe::bag::WeakBag;
use bagpipe::{BagPipe, BagCleanup};
use bagpipe::queue::FAAQueueLowLevel;
use mmap_alloc::{MapAlloc, MapAllocBuilder};
use object_alloc::UntypedObjectAlloc;

use alloc_type::AllocType;
use util::TryClone;


#[derive(Clone)]
pub struct UnmapCleanup<T>(Layout, PhantomData<T>);

impl<T> UnmapCleanup<T> {
    pub fn new(layout: Layout) -> UnmapCleanup<T> {
        UnmapCleanup(layout, PhantomData)
    }
}

impl<T> BagCleanup for UnmapCleanup<T> {
    type Item = *mut T;
    fn cleanup(&self, it: *mut T) {
        unsafe { ::util::mmap::unmap(it as *mut u8, self.0.size()) };
    }
}

/// An allocator which allocates pages of virtual memory.
///
/// A `PageAlloc` is an `UntypedObjectAlloc` whose allocated objects contain only
/// complete virtual memory pages (in other words, they begin and end on page
/// boundaries) and the memory backing those objects can be safely committed or
/// uncommitted.
///
/// On Windows, no allocated object may contain a partial allocation (from the
/// Windows `VirtualAlloc` function), as Windows does not support unmapping only
/// part of such an allocation.
pub unsafe trait PageAlloc: UntypedObjectAlloc {
    unsafe fn commit(&mut self, ptr: NonNull<u8>);
    unsafe fn uncommit(&mut self, ptr: NonNull<u8>);
}

unsafe impl PageAlloc for MapAlloc {
    unsafe fn commit(&mut self, ptr: NonNull<u8>) {
        MapAlloc::commit(self, ptr.as_ptr(), self.layout());
    }

    unsafe fn uncommit(&mut self, ptr: NonNull<u8>) {
        MapAlloc::uncommit(self, ptr.as_ptr(), self.layout());
    }
}

/// An `UntypedObjectAlloc` which caches committed and uncommitted pages separately.
pub struct PageCache<A: PageAlloc> {
    alloc: A,
    uncommitted: BagPipe<FAAQueueLowLevel<*mut u8>, UnmapCleanup<u8>>,
    committed: BagPipe<FAAQueueLowLevel<*mut u8>, UnmapCleanup<u8>>,
}

impl<A: PageAlloc> PageCache<A> {
    /// Construct a new `PageCache`.
    ///
    /// The constructed `PageCache` is guaranteed to allocate committed objects under
    /// two conditions:
    /// - `alloc` always allocates committed objects
    /// - `PageCache`'s `dealloc` method is only ever called on committed objects
    ///
    /// If either of these two conditions is not met, `PageCache` may sometimes allocate
    /// uncommitted objects.
    ///
    /// `committed_pipe_size` is the number of pipes in the committed bagpipe, and
    /// can be used to tune performance.
    pub fn new(alloc: A, committed_pipe_size: usize) -> PageCache<A> {
        let clean = UnmapCleanup::new(alloc.layout());
        PageCache{
            alloc: alloc,
            uncommitted: BagPipe::new_size_cleanup(2, clean.clone()),
            committed: BagPipe::new_size_cleanup(committed_pipe_size, clean),
        }
    }
}

unsafe impl<A: PageAlloc> UntypedObjectAlloc for PageCache<A> {
    fn layout(&self) -> Layout {
        self.alloc.layout()
    }

    unsafe fn alloc(&mut self) -> Option<NonNull<u8>> {
        if let Ok(ptr) = self.committed.try_pop_mut() {
            trace_event!(grabbed_dirty);
            alloc_debug_assert!(!ptr.is_null());
            Some(NonNull::new_unchecked(ptr))
        } else if let Ok(ptr) = self.uncommitted.try_pop_mut() {
            trace_event!(grabbed_clean);
            alloc_debug_assert!(!ptr.is_null());
            if cfg!(windows) {
                self.alloc.commit(NonNull::new_unchecked(ptr));
            }
            Some(NonNull::new_unchecked(ptr))
        } else {
            self.alloc.alloc()
        }
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>) {
        // TODO: When to uncommit and push onto the uncommitted bagpipe?
        self.committed.push_mut(ptr.as_ptr());
    }
}

impl<A: PageAlloc + TryClone> TryClone for PageCache<A> {
    fn try_clone(&self) -> Option<Self> {
        Some(PageCache {
            alloc: self.alloc.try_clone()?,
            uncommitted: self.uncommitted.clone(),
            committed: self.committed.clone(),
        })
    }
}

/// An `UntypedObjectAlloc` wrapper that writes an `AllocType` header to allocated objects.
///
/// Given an `UntypedObjectAlloc` and an `AllocType`, `MarkedAlloc` wraps the allocator
/// and writes the `AllocType` as a header to each allocated object.
pub struct MarkedAlloc<A: UntypedObjectAlloc> {
    alloc: A,
    ty: AllocType,
}

impl<A: UntypedObjectAlloc> MarkedAlloc<A> {
    /// Construct a new `MarkedAlloc`.
    ///
    /// # Panics
    ///
    /// `new` `alloc_assert`s that:
    /// - `alloc.layout()` has an equal size and alignment. Without this, it would not be
    ///   possible to reliably use a pointer to the interior of an allocated object to
    ///   determine the address of the `AllocType` header.
    /// - `alloc.layout()`'s size is large enough to hold an `AllocType`
    /// - `alloc.layout()`'s alignment satisfies `AllocType`'s alignment
    pub fn new(alloc: A, ty: AllocType) -> MarkedAlloc<A> {
        let (size, align) = (alloc.layout().size(), alloc.layout().align());
        alloc_assert_eq!(size, align);
        alloc_assert!(size >= mem::size_of::<AllocType>());
        alloc_assert!(align >= mem::align_of::<AllocType>());
        MarkedAlloc { alloc, ty }
    }
}

unsafe impl<A: UntypedObjectAlloc> UntypedObjectAlloc for MarkedAlloc<A> {
    fn layout(&self) -> Layout {
        self.alloc.layout()
    }

    unsafe fn alloc(&mut self) -> Option<NonNull<u8>> {
        let ptr = self.alloc.alloc()?;
        ptr::write(ptr.cast().as_ptr(), self.ty);
        Some(ptr)
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>) {
        self.alloc.dealloc(ptr);
    }
}

impl<A: UntypedObjectAlloc + TryClone> TryClone for MarkedAlloc<A> {
    fn try_clone(&self) -> Option<Self> {
        Some(MarkedAlloc {
            alloc: self.alloc.try_clone()?,
            ty: self.ty,
        })
    }
}

pub type MarkedMmapCache = MarkedAlloc<PageCache<MapAlloc>>;

pub fn new_marked_mmap_cache(layout: Layout, committed_pipe_size: usize, ty: AllocType) -> MarkedMmapCache {
    let ma = MapAllocBuilder::default().commit(true).obj_size(layout.size()).obj_align(layout.align()).build();
    MarkedAlloc::new(PageCache::new(ma, committed_pipe_size), ty)
}