//! Warmth routing discount. [DEMI-WARM-DISCOUNT]

use crate::Discount;

/// Maximum fractional cost reduction on a full warmth hit.
const MAX_WARMTH_DISCOUNT: f64 = 0.85;

/// Map warmth strength `[0, 1]` to a bounded discount factor in `(0, 1]`.
/// Miss (`strength <= 0`) returns `None` — caller applies no discount.
pub fn warmth_discount(strength: f64) -> Option<Discount> {
    if !strength.is_finite() || strength <= 0.0 {
        return None;
    }
    let factor = 1.0 - strength.clamp(0.0, 1.0) * MAX_WARMTH_DISCOUNT;
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
        assert!(d >= 1.0 - MAX_WARMTH_DISCOUNT);
    }
}
