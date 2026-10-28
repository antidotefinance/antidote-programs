//! The observation ring, byte for byte the layout `levpad-engine/src/oracle.rs` reads:
//! 0..8 magic, 8..40 engine market, 40..44 head, 44..48 len, 48..52 cap, 52..56 pad (byte 52
//! holds this ring's PDA bump; the engine ignores it), 56..64 last quote reserve (lamports),
//! 64.. cap * 40-byte observations (slot u64, cum u128, price u128, all little-endian).
//!
//! Recording goes through `levpad_math::oracle::Ring`, so the semantics (one observation per
//! slot, the last write in a slot wins, wrapping cumulative sum) are the engine's exactly.

use levpad_math::oracle::{Obs, Ring};
use levpad_math::MathError;

pub const MAGIC: [u8; 8] = *b"LVOBSRNG";
pub const HEADER: usize = 64;
pub const OBS_LEN: usize = 40;
pub const CAP: usize = 240;
pub const LEN: usize = HEADER + CAP * OBS_LEN;
pub const BUMP_AT: usize = 52;

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}
fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}

pub fn init(d: &mut [u8], market: &[u8; 32], bump: u8) {
    d[..8].copy_from_slice(&MAGIC);
    d[8..40].copy_from_slice(market);
    d[40..48].fill(0);
    d[48..52].copy_from_slice(&(CAP as u32).to_le_bytes());
    d[BUMP_AT] = bump;
}

pub fn is_ring(d: &[u8]) -> bool {
    d.len() >= LEN && d[..8] == MAGIC && u32_at(d, 48) as usize == CAP
}

fn read_obs(d: &[u8], i: usize) -> Obs {
    let o = HEADER + i * OBS_LEN;
    Obs { slot: u64_at(d, o), cum: u128_at(d, o + 8), price: u128_at(d, o + 24) }
}

/// The newest observation, if any.
pub fn newest(d: &[u8]) -> Option<Obs> {
    let len = u32_at(d, 44);
    let head = u32_at(d, 40) as usize;
    (len > 0 && head < CAP).then(|| read_obs(d, head))
}

/// CAP default observations without cloning one by one (`vec![Obs::default(); CAP]` clones 240
/// structs, about 5k CU on SBF). All-zero bytes are `Obs::default()`.
fn zeroed_obs() -> Vec<Obs> {
    let layout = std::alloc::Layout::array::<Obs>(CAP).unwrap();
    // SAFETY: Obs is plain integers, so zeroed memory is a valid Obs; the Vec takes ownership of
    // exactly the allocation made with this layout.
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout) as *mut Obs;
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Vec::from_raw_parts(ptr, CAP, CAP)
    }
}

/// The cumulative price at `slot`: the newest observation's, extended at its price (wrapping).
/// None for an empty ring.
pub fn cum_at_newest(d: &[u8], slot: u64) -> Option<u128> {
    let o = newest(d)?;
    Some(o.cum.wrapping_add(o.price.wrapping_mul(slot.saturating_sub(o.slot) as u128)))
}

/// Restart the ring at a venue switch: one observation, `price` from `slot` on, whose cumulative
/// carries on from the old ring's (the old newest price held until `slot`). So the ring's own
/// TWAP windows hold only the new observations, while a reader that kept an earlier cumulative
/// (the engine's anti-farming samples) still gets the exact time-weighted mean across the switch.
/// The caller has checked `is_ring`.
pub fn restart(d: &mut [u8], market: &[u8; 32], slot: u64, price: u128, quote_reserve: u64) -> Result<(), MathError> {
    let carried = cum_at_newest(d, slot).unwrap_or(0);
    let bump = d[BUMP_AT];
    init(d, market, bump);
    record(d, slot, price, quote_reserve)?;
    d[HEADER + 8..HEADER + 24].copy_from_slice(&carried.to_le_bytes());
    Ok(())
}

/// Record `price` at `slot` and the venue's quote reserve. Only the header and the one
/// observation that changed are written back. The caller has checked `is_ring`.
pub fn record(d: &mut [u8], slot: u64, price: u128, quote_reserve: u64) -> Result<(), MathError> {
    let mut head = u32_at(d, 40);
    let mut len = u32_at(d, 44);
    if head as usize >= CAP || len as usize > CAP {
        return Err(MathError::Overflow);
    }
    // Ring::record reads only the newest observation, so the rest of the buffer stays default.
    let mut obs = zeroed_obs();
    if len > 0 {
        obs[head as usize] = read_obs(d, head as usize);
    }
    {
        let mut ring = Ring { obs: &mut obs, head: &mut head, len: &mut len };
        ring.record(slot, price)?;
    }
    d[40..44].copy_from_slice(&head.to_le_bytes());
    d[44..48].copy_from_slice(&len.to_le_bytes());
    d[56..64].copy_from_slice(&quote_reserve.to_le_bytes());
    let o = HEADER + head as usize * OBS_LEN;
    let ob = obs[head as usize];
    d[o..o + 8].copy_from_slice(&ob.slot.to_le_bytes());
    d[o + 8..o + 24].copy_from_slice(&ob.cum.to_le_bytes());
    d[o + 24..o + 40].copy_from_slice(&ob.price.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use levpad_math::Q64;

    /// The byte-level ring matches a full in-memory `Ring` over the same writes, wrap included.
    #[test]
    fn matches_full_ring() {
        let mut d = vec![0u8; LEN];
        init(&mut d, &[7u8; 32], 254);
        let (mut b, mut h, mut l) = (vec![Obs::default(); CAP], 0u32, 0u32);
        let mut slot = 10u64;
        for i in 0..600u64 {
            slot += i % 3; // 0, 1 or 2 slots apart: same-slot rewrites included
            let price = Q64 / 1000 * (1 + (i * 7919) % 97) as u128;
            record(&mut d, slot, price, i).unwrap();
            Ring { obs: &mut b, head: &mut h, len: &mut l }.record(slot, price).unwrap();
        }
        assert_eq!(u32_at(&d, 40), h);
        assert_eq!(u32_at(&d, 44), l);
        for i in 0..CAP {
            assert_eq!(read_obs(&d, i), b[i], "obs {i}");
        }
        assert_eq!(u64_at(&d, 56), 599);
        assert_eq!(d[BUMP_AT], 254);
        assert!(is_ring(&d));
    }
}
