//! The receipt's picture, on chain (owner 2026-10-26: "WE WANT a picture version and details of
//! expiration or redeemability directly onchain as well"; SPEC.md "On-chain picture").
//!
//! `issue_receipt` draws a compact SVG of the ticket (design C, the receipts service's "ticket":
//! wine ground, cream ticket with side notches, pink header band, a dashed tear above the stub)
//! with only the receipt's stable facts: PROOF OF LOSS, the coin's ticker, the loss, what is owed,
//! #n and "Redeemable from <UTC>". No countdown, no state, no vial art, no embedded font (the
//! viewer's sans-serif). It is stored in the NFT's TokenMetadata as the additional field `image`, a
//! `data:image/svg+xml` URI, so it outlives any server; the metadata `uri` still points at the
//! receipts service (the rich PNG). Built by hand with no `format!` (compute).

/// Everything before the ticker, already in data-URI form: single quotes, and every `#` (the
/// colours, the number's) written `%23`, so the URI needs no other escaping and the program builds
/// it in one pass (`svg_of_uri` decodes it back for reading).
const HEAD: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 360 360'>\
<rect width='360' height='360' fill='%235C1236'/>\
<path d='M44 24h272q8 0 8 8v198a14 14 0 0 0 0 28v70q0 8-8 8H44q-8 0-8-8v-70a14 14 0 0 0 0-28V32q0-8 8-8z' fill='%23F3E6D6'/>\
<path d='M36 32q0-8 8-8h272q8 0 8 8v40H36z' fill='%23FF87AB'/>\
<path d='M52 244h256' stroke='%235C1236' stroke-width='2' stroke-dasharray='2 6'/>\
<g font-family='sans-serif' fill='%235C1236'>\
<text x='54' y='56' font-size='18' font-weight='700' letter-spacing='2'>PROOF OF LOSS</text>\
<text x='54' y='114' font-size='28' font-weight='700'>";
const LOSS: &str = "</text><text x='54' y='146' font-size='12' fill='%238A4A63'>Loss</text>\
<text x='54' y='168' font-size='20' font-weight='700'>";
const OWED: &str = "</text><text x='54' y='196' font-size='12' fill='%238A4A63'>Owed</text>\
<text x='54' y='220' font-weight='700' font-size='";
/// The owed line's size: 20, or 16 past `OWED_LONG` bytes so it stays on the ticket.
const OWED_LONG: usize = 20;
const NUM: &str = "</text><text x='54' y='290' font-size='34' font-weight='700'>%23";
const FROM: &str = "</text><text x='54' y='318' font-size='12' fill='%238A4A63'>Redeemable from ";
const TAIL: &str = "</text></g></svg>";

pub const URI_PREFIX: &str = "data:image/svg+xml;utf8,";

/// The longest ticker the picture prints (bytes, after `clean_ticker`).
pub const MAX_TICKER: usize = 10;

/// What the picture prints.
pub struct Ticket<'a> {
    /// `$` and the coin's ticker (or a short mint), at most 1 + `MAX_TICKER` bytes, already clean.
    pub coin: &'a str,
    /// "0.705027502 SOL"
    pub loss: &'a str,
    /// "141,878,449.38 $ANTI"
    pub owed: &'a str,
    pub number: u64,
    /// "2027-01-15 00:19 UTC"
    pub redeemable_utc: &'a str,
}

/// The `image` field: `data:image/svg+xml;utf8,` and the SVG with `#` as `%23` (every other byte
/// of it is a URI-safe character in a data URI's body, the text being ASCII letters, digits and
/// ` $.,:-`), built in one pass.
pub fn data_uri(t: &Ticket) -> String {
    let mut o = String::with_capacity(uri_len_bound());
    o.push_str(URI_PREFIX);
    o.push_str(HEAD);
    o.push_str(t.coin);
    o.push_str(LOSS);
    o.push_str(t.loss);
    o.push_str(OWED);
    o.push_str(if t.owed.len() > OWED_LONG { "16'>" } else { "20'>" });
    o.push_str(t.owed);
    o.push_str(NUM);
    push_u64(&mut o, t.number);
    o.push_str(FROM);
    o.push_str(t.redeemable_utc);
    o.push_str(TAIL);
    o
}

/// The SVG itself, as XML (the URI's body with `%23` decoded): what a viewer renders.
pub fn svg_of_uri(uri: &str) -> Option<String> {
    uri.strip_prefix(URI_PREFIX).map(|b| b.replace("%23", "#"))
}

/// The longest the SVG part of the URI can be: the fixed parts, 1 + MAX_TICKER, a loss of up to 25 bytes
/// ("18446744073.709551615 SOL"), an owed amount of up to 41 (a u64 at 0 decimals grouped,
/// ".00 $" and a 10-byte ticker), 20 digits and a UTC date of up to 29 (any i64).
pub const fn svg_len_bound() -> usize {
    HEAD.len() + LOSS.len() + OWED.len() + 4 + NUM.len() + FROM.len() + TAIL.len() + (1 + MAX_TICKER) + 25 + 41 + 20 + 29
}

/// The longest `data_uri` can be.
pub const fn uri_len_bound() -> usize {
    URI_PREFIX.len() + svg_len_bound()
}

fn push_u64(o: &mut String, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    o.push_str(core::str::from_utf8(&buf[i..]).unwrap());
}

/// A ticker the picture can print verbatim: ASCII letters, digits, `_`, `-` and `.` kept, the rest
/// dropped, at most `MAX_TICKER` bytes; None when nothing is left.
pub fn clean_ticker(s: &str) -> Option<String> {
    let t: String = s.bytes().filter(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')).take(MAX_TICKER).map(|b| b as char).collect();
    (!t.is_empty()).then_some(t)
}

/// Whole tokens with thousands separators and two decimals, truncated ("141,878,449.38").
pub fn grouped(atoms: u64, decimals: u8) -> String {
    let p = 10u128.pow(decimals as u32);
    let whole = atoms as u128 / p;
    let cents = (atoms as u128 % p) * 100 / p;
    let mut digits = [0u8; 40];
    let (mut w, mut n) = (whole, 0);
    loop {
        digits[n] = b'0' + (w % 10) as u8;
        n += 1;
        w /= 10;
        if w == 0 {
            break;
        }
    }
    let mut o = String::with_capacity(n + n / 3 + 3);
    for i in (0..n).rev() {
        o.push(digits[i] as char);
        if i > 0 && i % 3 == 0 {
            o.push(',');
        }
    }
    o.push('.');
    o.push((b'0' + (cents / 10) as u8) as char);
    o.push((b'0' + (cents % 10) as u8) as char);
    o
}

/// Lamports as SOL with trailing zeros dropped, at least two decimals (705,027,500 ->
/// "0.7050275 SOL", 1 SOL -> "1.00 SOL").
pub fn sol_trim(lamports: u64) -> String {
    let mut o = String::with_capacity(26);
    push_u64(&mut o, lamports / 1_000_000_000);
    o.push('.');
    let mut frac = [b'0'; 9];
    let mut f = lamports % 1_000_000_000;
    for i in (0..9).rev() {
        frac[i] = b'0' + (f % 10) as u8;
        f /= 10;
    }
    let mut end = 9;
    while end > 2 && frac[end - 1] == b'0' {
        end -= 1;
    }
    o.push_str(core::str::from_utf8(&frac[..end]).unwrap());
    o.push_str(" SOL");
    o
}

/// The symbol in a Token-2022 mint's TokenMetadata extension (type 19), read in place; None when
/// the account is not a mint with one.
pub fn t22_symbol(d: &[u8]) -> Option<&str> {
    let mut i = 166;
    if d.len() <= 165 || d[165] != 1 {
        return None;
    }
    while i + 4 <= d.len() {
        let t = u16::from_le_bytes([d[i], d[i + 1]]);
        let l = u16::from_le_bytes([d[i + 2], d[i + 3]]) as usize;
        if t == 0 {
            return None;
        }
        if t == 19 {
            let v = d.get(i + 4..i + 4 + l)?;
            let mut o = 64usize;
            let mut s = || -> Option<&str> {
                let n = u32::from_le_bytes(v.get(o..o + 4)?.try_into().ok()?) as usize;
                let r = core::str::from_utf8(v.get(o + 4..o + 4 + n)?).ok();
                o += 4 + n;
                r
            };
            let _name = s()?;
            return s();
        }
        i += 4 + l;
    }
    None
}
