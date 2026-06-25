//! Low-overhead elapsed-time measurement for hot-path instrumentation.
//!
//! On x86_64 we prefer `rdtsc` (≈3 cycles) over `Instant::now()` (≈20-40 cycles
//! via VDSO `clock_gettime`). Both are TSC-backed on Linux when TSC is the active
//! clocksource, but `rdtsc` skips the VDSO calibration/scaling overhead.
//!
//! # Validation before committing to rdtsc
//!
//! 1. `CPUID[0x80000007].EDX[8]` — invariant TSC: rate is constant across
//!    C-states and frequency scaling. Without this, the calibrated rate drifts.
//! 2. Calibrated cycles/µs in `[100, 10000]` — sanity bounds (100 MHz to 10 GHz).
//!    Values outside this range indicate preemption-skewed or implausibly short
//!    calibration windows.
//! 3. Minimum observed window of 50 µs — guards against timer granularity on
//!    hypervisors where `Instant` ticks at coarse resolution.
//!
//! If any check fails, [`now`] and [`delta_us`] fall back to `Instant::now()` so
//! callers see no behaviour change — just slightly higher timing overhead.
//! [`using_tsc`] lets callers log which path is active.

use std::sync::OnceLock;
use std::time::Instant;

const MIN_CYCLES_PER_US: f64 = 100.0;    // 100 MHz — floor for any real CPU
const MAX_CYCLES_PER_US: f64 = 10_000.0; // 10 GHz — ceiling beyond current hardware
const MIN_WINDOW_US: f64 = 50.0;         // reject calibrations shorter than this

enum Backend {
    Tsc(f64),  // cycles per microsecond, validated
    Instant,   // fallback: invariant TSC absent or calibration out of bounds
}

/// Opaque timestamp. Holds either TSC cycles or nanos since a fixed origin.
/// Use only with [`delta_us`] from this module — do not interpret directly.
#[derive(Copy, Clone)]
pub struct Timestamp(u64);

static BACKEND: OnceLock<Backend> = OnceLock::new();
static ORIGIN: OnceLock<Instant> = OnceLock::new();

fn origin() -> Instant {
    *ORIGIN.get_or_init(Instant::now)
}

#[cfg(target_arch = "x86_64")]
fn has_invariant_tsc() -> bool {
    // CPUID leaf 0x80000007 ("Advanced Power Management Information")
    // EDX bit 8 = invariant TSC.
    let r = unsafe { core::arch::x86_64::__cpuid(0x8000_0007) };
    (r.edx & (1 << 8)) != 0
}

#[cfg(not(target_arch = "x86_64"))]
fn has_invariant_tsc() -> bool {
    false
}

fn backend() -> &'static Backend {
    BACKEND.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            if !has_invariant_tsc() {
                return Backend::Instant;
            }

            let t0 = Instant::now();
            let c0 = unsafe { core::arch::x86_64::_rdtsc() };
            while t0.elapsed().as_micros() < 200 {
                core::hint::spin_loop();
            }
            let elapsed_us = t0.elapsed().as_micros() as f64;
            let c1 = unsafe { core::arch::x86_64::_rdtsc() };

            if elapsed_us < MIN_WINDOW_US {
                return Backend::Instant; // timer granularity too coarse
            }
            let cps = c1.saturating_sub(c0) as f64 / elapsed_us;
            if !(MIN_CYCLES_PER_US..=MAX_CYCLES_PER_US).contains(&cps) {
                return Backend::Instant; // calibration out of sane bounds
            }
            Backend::Tsc(cps)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Backend::Instant
        }
    })
}

/// Trigger calibration eagerly (200 µs busy-wait, once per process).
/// Returns `true` if rdtsc is in use, `false` if falling back to `Instant`.
/// Call once at startup to amortise the busy-wait before the hot path runs.
pub fn calibrate() -> bool {
    matches!(backend(), Backend::Tsc(_))
}

/// `true` if rdtsc passed all validation checks; `false` means `Instant` fallback.
#[inline]
pub fn using_tsc() -> bool {
    matches!(backend(), Backend::Tsc(_))
}

/// Capture a timestamp. Pair with [`delta_us`].
#[inline(always)]
pub fn now() -> Timestamp {
    match backend() {
        Backend::Tsc(_) => {
            #[cfg(target_arch = "x86_64")]
            return Timestamp(unsafe { core::arch::x86_64::_rdtsc() });
            #[cfg(not(target_arch = "x86_64"))]
            unreachable!()
        }
        Backend::Instant => Timestamp(Instant::now().duration_since(origin()).as_nanos() as u64),
    }
}

/// Microseconds elapsed since `start` was captured with [`now`].
#[inline(always)]
pub fn delta_us(start: Timestamp) -> u64 {
    match backend() {
        Backend::Tsc(cps) => {
            #[cfg(target_arch = "x86_64")]
            {
                let now = unsafe { core::arch::x86_64::_rdtsc() };
                (now.saturating_sub(start.0) as f64 / cps) as u64
            }
            #[cfg(not(target_arch = "x86_64"))]
            unreachable!()
        }
        Backend::Instant => {
            let now_ns = Instant::now().duration_since(origin()).as_nanos() as u64;
            now_ns.saturating_sub(start.0) / 1_000
        }
    }
}

/// Validate a raw cycles-per-µs value produced by calibration.
/// Returns the value if within sane bounds, `None` if calibration should be rejected.
pub fn validate_cps(cps: f64, window_us: f64) -> Option<f64> {
    if window_us < MIN_WINDOW_US {
        return None;
    }
    if !(MIN_CYCLES_PER_US..=MAX_CYCLES_PER_US).contains(&cps) {
        return None;
    }
    Some(cps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_zero_window() {
        assert!(validate_cps(3000.0, 0.0).is_none());
        assert!(validate_cps(3000.0, 10.0).is_none());
        assert!(validate_cps(3000.0, 49.9).is_none());
    }

    #[test]
    fn validate_rejects_out_of_bounds_cps() {
        assert!(validate_cps(50.0, 200.0).is_none());
        assert!(validate_cps(15_000.0, 200.0).is_none());
        assert!(validate_cps(0.0, 200.0).is_none());
        assert!(validate_cps(-1.0, 200.0).is_none());
    }

    #[test]
    fn validate_accepts_sane_values() {
        assert!(validate_cps(1_000.0, 200.0).is_some());
        assert!(validate_cps(3_000.0, 200.0).is_some());
        assert!(validate_cps(5_000.0, 200.0).is_some());
        assert!(validate_cps(100.0, 50.0).is_some());
        assert!(validate_cps(10_000.0, 50.0).is_some());
    }

    #[test]
    fn delta_us_is_monotone() {
        let t0 = now();
        let d = delta_us(t0);
        let d2 = delta_us(t0);
        assert!(d2 >= d);
    }

    #[test]
    fn delta_us_measures_real_elapsed() {
        let t0 = now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let d = delta_us(t0);
        assert!(d >= 3_000, "expected ≥ 3000 µs, got {} µs", d);
        assert!(d < 500_000, "expected < 500 000 µs, got {} µs", d);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn invariant_tsc_check_does_not_panic() {
        let _ = has_invariant_tsc();
    }
}
