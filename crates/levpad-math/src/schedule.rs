//! Piecewise-linear schedules the admin sets on chain: the profit fee by return on collateral,
//! and the token emission rate by pool size. Flat beyond the first and last points.

use crate::MathError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Point {
    pub x: u64,
    pub y: u64,
}

/// Points in use are the first `n`; x must be strictly increasing.
pub fn validate(points: &[Point], n: usize) -> Result<(), MathError> {
    if n == 0 || n > points.len() {
        return Err(MathError::BadSchedule);
    }
    for w in points[..n].windows(2) {
        if w[1].x <= w[0].x {
            return Err(MathError::BadSchedule);
        }
    }
    Ok(())
}

pub fn eval(points: &[Point], n: usize, x: u64) -> Result<u64, MathError> {
    validate(points, n)?;
    let pts = &points[..n];
    if x <= pts[0].x {
        return Ok(pts[0].y);
    }
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if x <= b.x {
            let dx = (b.x - a.x) as i128;
            let t = (x - a.x) as i128;
            let y = a.y as i128 + (b.y as i128 - a.y as i128) * t / dx;
            return Ok(y as u64);
        }
    }
    Ok(pts[n - 1].y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_and_clamps() {
        let p = [Point { x: 100, y: 1000 }, Point { x: 200, y: 0 }, Point::default()];
        assert_eq!(eval(&p, 2, 0).unwrap(), 1000);
        assert_eq!(eval(&p, 2, 150).unwrap(), 500);
        assert_eq!(eval(&p, 2, 999).unwrap(), 0);
        assert_eq!(eval(&p, 3, 150).unwrap_err(), MathError::BadSchedule);
        assert_eq!(eval(&p, 0, 150).unwrap_err(), MathError::BadSchedule);
    }
}
