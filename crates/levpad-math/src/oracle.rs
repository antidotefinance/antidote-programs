//! Slot-weighted TWAP from a ring of cumulative-price observations (Uniswap v2 style).
//!
//! Every write sets the price that holds from that slot on. The cumulative sum adds the
//! previous price times the slots it held. At most one observation per slot: a second write in
//! the same slot only replaces the price, so a pump undone inside one slot counts for nothing,
//! and a price counts only for the slots it actually stood at a slot boundary.

use crate::{mul_div, MathError};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Obs {
    pub slot: u64,
    /// Sum of price * slots up to `slot`, wrapping.
    pub cum: u128,
    /// Price set at `slot` (the last write in that slot).
    pub price: u128,
}

/// Ring buffer over a caller-owned slice so the program can back it with a zero-copy account.
pub struct Ring<'a> {
    pub obs: &'a mut [Obs],
    /// Index of the newest observation.
    pub head: &'a mut u32,
    /// Number of valid observations.
    pub len: &'a mut u32,
}

impl<'a> Ring<'a> {
    pub fn newest(&self) -> Option<Obs> {
        if *self.len == 0 {
            None
        } else {
            Some(self.obs[*self.head as usize])
        }
    }

    /// Record `price` as the spot at `slot`. Slots must not go backwards.
    pub fn record(&mut self, slot: u64, price: u128) -> Result<(), MathError> {
        let cap = self.obs.len() as u32;
        match self.newest() {
            None => {
                *self.head = 0;
                *self.len = 1;
                self.obs[0] = Obs { slot, cum: 0, price };
            }
            Some(last) if slot <= last.slot => {
                // same slot (or a stale caller): the newest price wins, no time has passed
                self.obs[*self.head as usize].price = price;
            }
            Some(last) => {
                let dt = (slot - last.slot) as u128;
                let cum = last.cum.wrapping_add(last.price.wrapping_mul(dt));
                let next = (*self.head + 1) % cap;
                self.obs[next as usize] = Obs { slot, cum, price };
                *self.head = next;
                if *self.len < cap {
                    *self.len += 1;
                }
            }
        }
        Ok(())
    }

    /// Cumulative price at `slot`, which must be inside the recorded history.
    fn cum_at(&self, slot: u64) -> Result<u128, MathError> {
        let cap = self.obs.len() as u32;
        let len = *self.len;
        if len == 0 {
            return Err(MathError::ShortHistory);
        }
        // walk back from the newest to the first observation at or before `slot`
        for k in 0..len {
            let i = ((*self.head + cap - k) % cap) as usize;
            let o = self.obs[i];
            if o.slot <= slot {
                let dt = (slot - o.slot) as u128;
                return Ok(o.cum.wrapping_add(o.price.wrapping_mul(dt)));
            }
        }
        Err(MathError::ShortHistory)
    }

    /// Time-weighted average price over the `window` slots ending at `now`.
    /// `window == 0` returns the current price.
    pub fn twap(&self, now: u64, window: u64) -> Result<u128, MathError> {
        let last = self.newest().ok_or(MathError::ShortHistory)?;
        if window == 0 {
            return Ok(last.price);
        }
        let start = now.checked_sub(window).ok_or(MathError::ShortHistory)?;
        let a = self.cum_at(start)?;
        let b = self.cum_at(now)?;
        mul_div(b.wrapping_sub(a), 1, window as u128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Q64;

    macro_rules! ring {
        ($r:ident, $n:expr) => {
            let (mut b, mut h, mut l) = ([Obs::default(); $n], 0u32, 0u32);
            #[allow(unused_mut)]
            let mut $r = Ring { obs: &mut b, head: &mut h, len: &mut l };
        };
    }

    #[test]
    fn flat_price_twap() {
        ring!(r, 8);
        r.record(100, 5 * Q64).unwrap();
        assert_eq!(r.twap(150, 50).unwrap(), 5 * Q64);
        assert_eq!(r.twap(150, 51).unwrap_err(), MathError::ShortHistory);
    }

    #[test]
    fn step_is_time_weighted() {
        ring!(r, 8);
        r.record(0, 1 * Q64).unwrap();
        r.record(75, 3 * Q64).unwrap();
        // window [50,100): 25 slots at 1, 25 at 3
        assert_eq!(r.twap(100, 50).unwrap(), 2 * Q64);
    }

    #[test]
    fn same_slot_pump_and_dump_counts_nothing() {
        ring!(r, 8);
        r.record(0, Q64).unwrap();
        r.record(10, 100 * Q64).unwrap();
        r.record(10, Q64).unwrap();
        assert_eq!(r.twap(20, 20).unwrap(), Q64);
    }

    #[test]
    fn one_slot_pump_counts_one_slot() {
        ring!(r, 8);
        r.record(0, Q64).unwrap();
        r.record(10, 101 * Q64).unwrap();
        r.record(11, Q64).unwrap();
        // 99 slots at 1, 1 slot at 101 -> 2
        assert_eq!(r.twap(100, 100).unwrap(), 2 * Q64);
    }

    #[test]
    fn ring_wraps_and_forgets() {
        ring!(r, 4);
        for s in 0..10u64 {
            r.record(s * 10, (s as u128 + 1) * Q64).unwrap();
        }
        // kept slots 60,70,80,90; window back to 60 works, to 59 does not
        assert_eq!(r.twap(100, 40).unwrap(), (7 + 8 + 9 + 10) * Q64 / 4);
        assert_eq!(r.twap(100, 41).unwrap_err(), MathError::ShortHistory);
    }
}
