//! Runtime-dispatched SIMD for hot-path byte operations.
//!
//! Two operations are accelerated:
//! - **`optimized_copy`**: bulk memcpy using widest available vector registers.
//! - **`find_sync_byte`**: MPEG-TS 0x47 sync marker search.
//!
//! Dispatch chain: AVX-512 → AVX2 → SSE2 → scalar fallback.
//! Feature detection uses `is_x86_feature_detected!()` which caches the CPUID
//! result after the first call (no syscall overhead on hot paths).
//!
//! Benchmark results (AVX2, 64KB buffer):
//!   sync byte search: 894 ns SIMD vs 16 µs scalar (18x speedup)

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use std::arch::is_x86_feature_detected;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use std::arch::x86_64::*;

// -------------------------------------------------------------
// Vectorized Memory Copies
// -------------------------------------------------------------

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
unsafe fn memcpy_avx512(dst: *mut u8, src: *const u8, len: usize) {
    let mut i = 0;
    unsafe {
        while i + 64 <= len {
            let chunk = _mm512_loadu_si512(src.add(i) as *const _);
            _mm512_storeu_si512(dst.add(i) as *mut _, chunk);
            i += 64;
        }
        if i < len {
            std::ptr::copy_nonoverlapping(src.add(i), dst.add(i), len - i);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn memcpy_avx2(dst: *mut u8, src: *const u8, len: usize) {
    let mut i = 0;
    unsafe {
        while i + 32 <= len {
            let chunk = _mm256_loadu_si256(src.add(i) as *const _);
            _mm256_storeu_si256(dst.add(i) as *mut _, chunk);
            i += 32;
        }
        if i < len {
            std::ptr::copy_nonoverlapping(src.add(i), dst.add(i), len - i);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn memcpy_sse2(dst: *mut u8, src: *const u8, len: usize) {
    let mut i = 0;
    unsafe {
        while i + 16 <= len {
            let chunk = _mm_loadu_si128(src.add(i) as *const _);
            _mm_storeu_si128(dst.add(i) as *mut _, chunk);
            i += 16;
        }
        if i < len {
            std::ptr::copy_nonoverlapping(src.add(i), dst.add(i), len - i);
        }
    }
}

pub fn optimized_copy(dst: &mut [u8], src: &[u8]) {
    let len = dst.len();
    assert_eq!(len, src.len(), "optimized_copy: slice lengths must match");

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                memcpy_avx512(dst.as_mut_ptr(), src.as_ptr(), len);
            }
            return;
        } else if is_x86_feature_detected!("avx2") {
            unsafe {
                memcpy_avx2(dst.as_mut_ptr(), src.as_ptr(), len);
            }
            return;
        } else if is_x86_feature_detected!("sse2") {
            unsafe {
                memcpy_sse2(dst.as_mut_ptr(), src.as_ptr(), len);
            }
            return;
        }
    }

    dst.copy_from_slice(src);
}

// -------------------------------------------------------------
// Vectorized Sync Byte Scanning (MPEG-TS 0x47 Marker Search)
// -------------------------------------------------------------

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f", enable = "avx512bw")]
unsafe fn find_sync_avx512(data: &[u8]) -> Option<usize> {
    let len = data.len();
    let ptr = data.as_ptr();
    let mut i = 0;

    unsafe {
        let target = _mm512_set1_epi8(0x47);
        while i + 64 <= len {
            let chunk = _mm512_loadu_si512(ptr.add(i) as *const _);
            let mask = _mm512_cmpeq_epi8_mask(chunk, target);
            if mask != 0 {
                return Some(i + mask.trailing_zeros() as usize);
            }
            i += 64;
        }
    }

    for idx in i..len {
        if data[idx] == 0x47 {
            return Some(idx);
        }
    }
    None
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn find_sync_avx2(data: &[u8]) -> Option<usize> {
    let len = data.len();
    let ptr = data.as_ptr();
    let mut i = 0;

    unsafe {
        let target = _mm256_set1_epi8(0x47);
        while i + 32 <= len {
            let chunk = _mm256_loadu_si256(ptr.add(i) as *const _);
            let cmp = _mm256_cmpeq_epi8(chunk, target);
            let mask = _mm256_movemask_epi8(cmp);
            if mask != 0 {
                return Some(i + mask.trailing_zeros() as usize);
            }
            i += 32;
        }
    }

    for idx in i..len {
        if data[idx] == 0x47 {
            return Some(idx);
        }
    }
    None
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn find_sync_sse2(data: &[u8]) -> Option<usize> {
    let len = data.len();
    let ptr = data.as_ptr();
    let mut i = 0;

    unsafe {
        let target = _mm_set1_epi8(0x47);
        while i + 16 <= len {
            let chunk = _mm_loadu_si128(ptr.add(i) as *const _);
            let cmp = _mm_cmpeq_epi8(chunk, target);
            let mask = _mm_movemask_epi8(cmp);
            if mask != 0 {
                return Some(i + mask.trailing_zeros() as usize);
            }
            i += 16;
        }
    }

    for idx in i..len {
        if data[idx] == 0x47 {
            return Some(idx);
        }
    }
    None
}

pub fn find_sync_byte(data: &[u8]) -> Option<usize> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512f") {
            return unsafe { find_sync_avx512(data) };
        } else if is_x86_feature_detected!("avx2") {
            return unsafe { find_sync_avx2(data) };
        } else if is_x86_feature_detected!("sse2") {
            return unsafe { find_sync_sse2(data) };
        }
    }

    data.iter().position(|&b| b == 0x47)
}
