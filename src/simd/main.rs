//! Port of `src/simd/main.zig`.
//!
//! STATUS: ported
//!
//! SIMD-optimized routines. The Zig build calls C++ (Google Highway,
//! simdutf) when `build_options.simd` is true and otherwise falls back to
//! pure Zig scalar implementations. This port is pure Rust: it implements
//! the scalar semantics, vectorized where it matters (`vt`, `index_of`) with
//! `std::arch` / `memchr`.

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "base64.rs"]
pub mod base64;
#[path = "base64_scalar.rs"]
pub mod base64_scalar;
#[path = "codepoint_width.rs"]
pub mod codepoint_width;
#[path = "index_of.rs"]
pub mod index_of;
#[path = "vt.rs"]
pub mod vt;
// @generated-mods-end

pub use codepoint_width::codepoint_width;

/// The native SIMD register width in bytes for the compile target, or None
/// if the target has no usable SIMD support (`std.simd.suggestVectorLength`).
const fn native_vector_bytes() -> Option<usize> {
    if cfg!(any(target_feature = "avx512bw", target_feature = "avx512f")) {
        Some(64)
    } else if cfg!(target_feature = "avx2") {
        Some(32)
    } else if cfg!(any(
        target_feature = "sse2",
        target_feature = "neon",
        target_feature = "simd128",
        target_feature = "altivec",
    )) {
        Some(16)
    } else {
        None
    }
}

/// The number of vector lanes to use for manually vectorized hot
/// loops operating on elements of type T, or None if the target has
/// no usable SIMD support and a plain scalar loop should be used
/// instead (e.g. wasm32 without simd128).
///
/// This is twice the native vector width so that each iteration works
/// two vector registers, giving better instruction-level parallelism.
pub const fn lanes<T>() -> Option<usize> {
    match native_vector_bytes() {
        Some(bytes) => {
            let size = std::mem::size_of::<T>();
            if size == 0 || size > bytes {
                None
            } else {
                Some(bytes / size * 2)
            }
        }
        None => None,
    }
}

/// Whether the CPU supports AVX2, for hot loops that have a second,
/// AVX2-compiled instance selected at runtime when the compile target
/// itself lacks AVX2 (Zig's native default build gets the wider vectors
/// at compile time; Highway and simdutf select them at runtime). The
/// result is cached by `std`, so this is a load and a bit test.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn avx2_available() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

#[cfg(test)]
thread_local! {
    static FORCE_PORTABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test hook: while true (on this thread), the runtime-selected AVX2
/// instances of the terminal print path are not used, so tests can
/// compare them with the portable instances on the same machine.
#[cfg(test)]
pub fn set_force_portable(v: bool) {
    FORCE_PORTABLE.with(|f| f.set(v));
}

/// See [`set_force_portable`]; always false outside tests.
#[inline(always)]
pub fn force_portable() -> bool {
    #[cfg(test)]
    {
        FORCE_PORTABLE.with(|f| f.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_test() {
        #[cfg(target_arch = "x86_64")]
        {
            let l = lanes::<u8>().unwrap();
            assert!(l >= 32);
            assert_eq!(l / 4, lanes::<u32>().unwrap());
        }
        if let Some(l) = lanes::<u64>() {
            assert!(l >= 2);
        }
    }
}
