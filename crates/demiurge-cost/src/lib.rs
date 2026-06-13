//! Demiurge cost-function factor algebra.
//!
//! The router's cost function must be *strictly positive* so the learned
//! corrector's multiplicative `±α` envelope is meaningful. We guarantee that
//! structurally: a [`Cost`] is the product of a strictly-positive time core and
//! a set of factors each constrained to a positive range. The public API
//! exposes multiplication only — there is no `Sub`/`Neg`, and every field is
//! private — so a future "just subtract a reward term" cannot be expressed.
//!
//! Spec references (kept in sync by `cargo xtask lint`):
//! - [DEMI-COST-POS]   C(r,q) > 0 by construction (spec 4.3, 4.5).
//! - [DEMI-CORR-CLAMP] corrector multiplier in [1-alpha, 1+alpha] (spec 4.5, 7).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

mod generated_params;
pub use generated_params::*;

/// Corrector half-width α, sourced from the canonical params file.
pub const ALPHA: f64 = generated_params::CORRECTOR_ALPHA;

/// Number of times a factor had to be clamped at runtime. Exported so
/// production can alarm on drift that compile-time and CI checks cannot see.
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

    /// Best-effort constructor for the hot path: clamps to the smallest
    /// positive value instead of failing, and records the event.
    pub fn clamped(seconds: f64) -> Self {
        if seconds.is_finite() && seconds > 0.0 {
            Self(seconds)
        } else {
            bump_clamp();
            Self(f64::MIN_POSITIVE)
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

    pub fn clamped(x: f64) -> Self {
        if x.is_finite() && x >= 1.0 {
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

/// Multiplicative reward in `(0, 1]` — e.g. a cache-locality discount.
/// A reward can only *scale cost down*, never below zero.
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

    pub fn clamped(x: f64) -> Self {
        if x.is_finite() && x > 0.0 && x <= 1.0 {
            Self(x)
        } else {
            bump_clamp();
            if x.is_finite() && x > 1.0 {
                Self(1.0)
            } else {
                Self(f64::MIN_POSITIVE)
            }
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// Learned-corrector multiplier, clamped to `[1-α, 1+α]`. Because `α < 1`, the
/// multiplier is always strictly positive, so it can never flip cost's sign.
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

/// A strictly-positive cost. Constructible only via [`compose`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Cost(f64);

impl Cost {
    pub fn get(self) -> f64 {
        self.0
    }

    pub fn is_positive(self) -> bool {
        self.0.is_finite() && self.0 > 0.0
    }
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The sole constructor of [`Cost`]: a product of strictly-positive terms.
/// Multiplication of a positive core by `≥1`, `(0,1]`, and `[1-α,1+α]` factors
/// is positive, so the result is positive by construction. [DEMI-COST-POS]
pub fn compose(
    core: TimeCore,
    barriers: &[BarrierFactor],
    discounts: &[Discount],
    corrector: Corrector,
) -> Cost {
    let mut c = core.get();
    for b in barriers {
        c *= b.get();
    }
    for d in discounts {
        c *= d.get();
    }
    c *= corrector.get();
    Cost(c)
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
