//! Demiurge cost-function factor algebra.
//!
//! The router's cost must be **strictly positive** so the learned corrector's
//! multiplicative `±α` envelope is meaningful. We get that *genuinely* by
//! construction by representing a [`Cost`] as its **natural logarithm**: a
//! [`Cost`] stores a finite `f64` `ln`, and any finite log is the logarithm of
//! a strictly-positive real. Composition is addition of logs, which — unlike
//! the naive product of factors in linear space — cannot underflow to `0.0`
//! nor flip sign, and only the count of factors (not their magnitudes) could
//! ever push the sum to non-finite, which is impossible for any realistic
//! number of terms.
//!
//! Linear-space access via [`Cost::get`] is provided for display only and may
//! saturate to `0.0`/`∞` at the extremes; **ordering and comparison must use
//! [`Cost::ln`]**, which is exact and monotonic in the true cost.
//!
//! Invalid inputs follow a **fail-expensive** policy (see [`TimeCore::clamped`]
//! et al.): a broken signal can only make a target look *more* expensive, never
//! cheaper, so a NaN latency can never stampede traffic onto a sick backend.
//!
//! Spec references (kept in sync by `cargo xtask lint`):
//! - [DEMI-COST-POS]       C(r,q) > 0 by construction (spec 4.3, 4.5).
//! - [DEMI-CORR-CLAMP]     corrector multiplier in [1-alpha, 1+alpha] (spec 4.5, 7).
//! - [DEMI-FAIL-EXPENSIVE] invalid factors saturate toward "expensive" (spec 4.5).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

mod generated_params;
mod kv;
mod warmth;
pub use generated_params::*;
pub use kv::{
    fleet_marginal_bytes, fleet_marginal_bytes_wrong, kv_breakdown, percentile90, phi_barrier,
    phi_barrier_marginal, KvBreakdown,
};
pub use warmth::warmth_discount;

/// Corrector half-width α, sourced from the canonical params file.
pub const ALPHA: f64 = generated_params::CORRECTOR_ALPHA;

/// Count of fail-expensive saturation events. A coarse, process-global health
/// gauge: a nonzero, climbing value means upstream signals are arriving broken.
/// Per-target attribution belongs in the forwarder's own metrics, not here.
pub static FACTOR_CLAMP_EVENTS: AtomicU64 = AtomicU64::new(0);

fn bump_clamp() {
    FACTOR_CLAMP_EVENTS.fetch_add(1, Ordering::Relaxed);
}

/// Error returned when a factor is constructed outside its permitted range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactorError {
    NotFinite,
    OutOfRange,
}

impl fmt::Display for FactorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FactorError::NotFinite => write!(f, "factor must be finite"),
            FactorError::OutOfRange => write!(f, "factor outside its permitted range"),
        }
    }
}

impl std::error::Error for FactorError {}

/// Strictly-positive analytic time core (seconds): the dimensional anchor.
#[derive(Debug, Clone, Copy)]
pub struct TimeCore(f64);

impl TimeCore {
    pub fn new(seconds: f64) -> Result<Self, FactorError> {
        if !seconds.is_finite() {
            return Err(FactorError::NotFinite);
        }
        if seconds <= 0.0 {
            return Err(FactorError::OutOfRange);
        }
        Ok(Self(seconds))
    }

    /// Infallible hot-path constructor. **Fail-expensive:** a non-finite or
    /// non-positive input (i.e. a broken latency signal) saturates to the
    /// largest finite value, making the target maximally *unattractive*, and
    /// records the event. It never maps garbage to a small (cheap) value.
    pub fn clamped(seconds: f64) -> Self {
        if seconds.is_finite() && seconds > 0.0 {
            Self(seconds)
        } else {
            bump_clamp();
            Self(f64::MAX)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// Multiplicative penalty in `[1, ∞)` — e.g. a memory-pressure barrier.
#[derive(Debug, Clone, Copy)]
pub struct BarrierFactor(f64);

impl BarrierFactor {
    pub fn new(x: f64) -> Result<Self, FactorError> {
        if !x.is_finite() {
            return Err(FactorError::NotFinite);
        }
        if x < 1.0 {
            return Err(FactorError::OutOfRange);
        }
        Ok(Self(x))
    }

    /// Fail-expensive: an invalid penalty saturates to the largest finite value
    /// (maximum penalty), never to the neutral `1.0`.
    pub fn clamped(x: f64) -> Self {
        if x.is_finite() && x >= 1.0 {
            Self(x)
        } else {
            bump_clamp();
            Self(f64::MAX)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// Multiplicative reward in `(0, 1]` — e.g. a cache-locality discount. A reward
/// can only ever *scale cost down*.
#[derive(Debug, Clone, Copy)]
pub struct Discount(f64);

impl Discount {
    pub fn new(x: f64) -> Result<Self, FactorError> {
        if !x.is_finite() {
            return Err(FactorError::NotFinite);
        }
        if x <= 0.0 || x > 1.0 {
            return Err(FactorError::OutOfRange);
        }
        Ok(Self(x))
    }

    /// Fail-expensive: an invalid reward saturates to the neutral `1.0` (no
    /// discount). It never grants an unearned (cheapening) reward.
    pub fn clamped(x: f64) -> Self {
        if x.is_finite() && x > 0.0 && x <= 1.0 {
            Self(x)
        } else {
            bump_clamp();
            Self(1.0)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// Learned-corrector multiplier, clamped to `[1-α, 1+α]`. Because `α < 1` the
/// multiplier is strictly positive, so it can never flip cost's sign.
/// [DEMI-CORR-CLAMP]
#[derive(Debug, Clone, Copy)]
pub struct Corrector(f64);

impl Corrector {
    pub fn new(delta: f64) -> Self {
        let lo = 1.0 - ALPHA;
        let hi = 1.0 + ALPHA;
        let d = if !delta.is_finite() {
            bump_clamp();
            1.0
        } else if delta < lo {
            bump_clamp();
            lo
        } else if delta > hi {
            bump_clamp();
            hi
        } else {
            delta
        };
        Self(d)
    }

    pub fn identity() -> Self {
        Self(1.0)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// A strictly-positive cost, represented by its natural logarithm. The stored
/// `ln` is always finite (class invariant), so the represented value is always
/// a strictly-positive real. Constructible only via [`compose`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Cost {
    ln: f64,
}

impl Cost {
    /// Natural log of the cost — exact, finite, and the *only* sound basis for
    /// comparing two costs.
    pub fn ln(self) -> f64 {
        self.ln
    }

    /// Linear-space value, for display/telemetry only. May saturate to `0.0`
    /// or `∞` at the extremes; do not compare two costs with this.
    pub fn get(self) -> f64 {
        self.ln.exp()
    }

    /// True by construction: the stored log is always finite.
    pub fn is_positive(self) -> bool {
        self.ln.is_finite()
    }

    /// Construct from a finite log-cost (hot-path fast paths in the router).
    #[inline]
    pub fn from_ln(ln: f64) -> Self {
        debug_assert!(ln.is_finite(), "cost log must be finite");
        Self { ln }
    }
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exp({})", self.ln)
    }
}

/// The sole constructor of [`Cost`]. Composition is **addition in log-space**:
/// `ln C = ln(core) + Σ ln(barrier) − Σ |ln(discount)| + ln(corrector)`. Every
/// term is finite by the factor constructors, so the sum is finite and the
/// represented cost is strictly positive — with no underflow-to-zero or
/// sign-flip possible, unlike a linear product. [DEMI-COST-POS]
#[inline]
pub fn compose(
    core: TimeCore,
    barriers: &[BarrierFactor],
    discounts: &[Discount],
    corrector: Corrector,
) -> Cost {
    let mut ln = core.get().ln();
    for b in barriers {
        ln += b.get().ln();
    }
    for d in discounts {
        ln += d.get().ln();
    }
    let c = corrector.get();
    if c != 1.0 {
        ln += c.ln();
    }
    debug_assert!(ln.is_finite(), "cost log overflowed: too many factors");
    Cost { ln }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn rejects_nonpositive_core() {
        assert!(TimeCore::new(0.0).is_err());
        assert!(TimeCore::new(-1.0).is_err());
        assert!(TimeCore::new(f64::NAN).is_err());
    }

    #[test]
    fn identity_corrector_is_one() {
        assert_eq!(Corrector::identity().get(), 1.0);
    }
}
