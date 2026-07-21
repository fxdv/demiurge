//! Warmth routing discount. [DEMI-WARM-DISCOUNT]
//!
//! ρ_max (`WARMTH_MAX_DISCOUNT`) is a first-class trust surface: a full hit
//! can swing placement more than the corrector band α. Keep it in
//! `design/demiurge.params.toml` and treat forged warmth (T3) accordingly.

use crate::{Discount, WARMTH_MAX_DISCOUNT};

/// Map warmth strength `[0, 1]` to a bounded discount factor in `(0, 1]`.
/// Miss (`strength <= 0`) returns `None` — caller applies no discount.
pub fn warmth_discount(strength: f64) -> Option<Discount> {
    if !strength.is_finite() || strength <= 0.0 {
        return None;
    }
    let factor = 1.0 - strength.clamp(0.0, 1.0) * WARMTH_MAX_DISCOUNT;
    Some(Discount::clamped(factor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmth_discount_bounded() {
        assert!(warmth_discount(0.0).is_none());
        let d = warmth_discount(1.0).expect("hit").get();
        assert!(d > 0.0 && d <= 1.0);
        assert!(d >= 1.0 - WARMTH_MAX_DISCOUNT);
        assert!((d - (1.0 - WARMTH_MAX_DISCOUNT)).abs() < 1e-12);
    }
}
