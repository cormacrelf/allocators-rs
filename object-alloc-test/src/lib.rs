#![feature(alloc)]
#![feature(allocator_api)]
#![feature(test)]
#![feature(const_fn)]
#![feature(plugin)]

#![plugin(interpolate_idents)]

#[cfg(unix)]
#[macro_use]
extern crate lazy_static;
#[allow(plugin_as_library)]
extern crate interpolate_idents;
pub use self::interpolate_idents::*;

pub mod corruption;
pub mod leaky_alloc;
pub mod types;

/// Call a function once for each alignment.
///
/// `foreach_align` calls `f` once for each valid alignment of `T` not greater than `max`. If `max`
/// is greater than any valid alignment of `T`, `f` is called for each valid alignment of `T`, and
/// `max` is ignored.
///
/// `foreach_align` is useful for testing allocators whose behavior may be sensitive to requested
/// alignment.
pub fn foreach_align<T, F: Fn(usize)>(f: F, max: usize) {
    use std::mem::{size_of, align_of};

    let min_align = align_of::<T>();
    let size = size_of::<T>();
    let mut align = min_align;
    while align <= size && align <= max {
        if size % align == 0 {
            f(align);
        }
        align *= 2;
    }
}