//! Global allocator: mimalloc, with arena eager commit off.

use std::ffi::c_char;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `mi_option_arena_eager_commit` (index 4 in mimalloc v2 and v3; libmimalloc-sys has no constant).
const ARENA_EAGER_COMMIT: libmimalloc_sys::mi_option_t = 4;

unsafe extern "C" {
    fn getenv(name: *const c_char) -> *const c_char;
}

/// Do not commit whole arenas up front: idle RSS drops by 4-8 MB for about +3% frame time.
///
/// mimalloc builds its first arena on the first allocation, which the Rust runtime makes before
/// `main`, so a call from `main` is too late; this runs from the loader's constructor list. It
/// must not allocate (no `std::env`). An explicit `MIMALLOC_ARENA_EAGER_COMMIT` wins.
extern "C" fn tune() {
    // SAFETY: getenv on a NUL-terminated literal; mi_option_set is a plain setter without pointers.
    unsafe {
        if getenv(c"MIMALLOC_ARENA_EAGER_COMMIT".as_ptr()).is_null() {
            libmimalloc_sys::mi_option_set(ARENA_EAGER_COMMIT, 0);
        }
    }
}

#[used]
#[cfg_attr(all(unix, not(target_vendor = "apple")), unsafe(link_section = ".init_array"))]
#[cfg_attr(target_vendor = "apple", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(windows, unsafe(link_section = ".CRT$XCU"))]
static TUNE_AT_LOAD: extern "C" fn() = tune;
