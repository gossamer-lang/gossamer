//! `netip::Prefix` arithmetic, after Go's `net/netip`: an address and a
//! prefix length, the address kept as written. IPv4 and IPv6 never mix, so an
//! IPv4 prefix does not contain the IPv4-mapped `::ffff:10.0.0.1`.

use std::net::IpAddr;

/// An address as a 128-bit number with its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bits {
    value: u128,
    width: u8,
}

fn bits_of(addr: IpAddr) -> Bits {
    match addr {
        IpAddr::V4(v4) => Bits {
            value: u128::from(u32::from(v4)),
            width: 32,
        },
        IpAddr::V6(v6) => Bits {
            value: u128::from(v6),
            width: 128,
        },
    }
}

fn addr_of(bits: Bits) -> IpAddr {
    if bits.width == 32 {
        // A 32-bit address holds at most 32 bits of value.
        IpAddr::V4(std::net::Ipv4Addr::from(
            u32::try_from(bits.value).unwrap_or(u32::MAX),
        ))
    } else {
        IpAddr::V6(std::net::Ipv6Addr::from(bits.value))
    }
}

/// The mask of the first `len` bits of a `width`-bit address.
fn mask(width: u8, len: u8) -> u128 {
    let ones = |n: u8| {
        if n >= 128 {
            u128::MAX
        } else {
            (1u128 << n) - 1
        }
    };
    ones(width) & !ones(width - len)
}

fn parse_addr(text: &str) -> Result<Bits, String> {
    text.parse::<IpAddr>()
        .map(bits_of)
        .map_err(|_| format!("netip: `{text}` is not an IP address"))
}

/// The address (in its canonical spelling, its bits as written) and the
/// length of the prefix `text`, such as `10.0.0.1/8`.
///
/// # Errors
/// No `/`, an address that does not parse, or a length that is not plain
/// decimal within 0..=32 for IPv4 or 0..=128 for IPv6.
pub fn parse(text: &str) -> Result<(String, i64), String> {
    let (addr, len) = text
        .split_once('/')
        .ok_or_else(|| format!("netip: `{text}` has no `/length`"))?;
    let bits = parse_addr(addr)?;
    let decimal = !len.is_empty()
        && len.bytes().all(|b| b.is_ascii_digit())
        && (len == "0" || !len.starts_with('0'));
    if !decimal {
        return Err(format!(
            "netip: the length in `{text}` must be plain decimal, with no sign or leading zero"
        ));
    }
    let length = len
        .parse::<u8>()
        .ok()
        .filter(|&n| n <= bits.width)
        .ok_or_else(|| {
            format!(
                "netip: the length in `{text}` must run from 0 to {}",
                bits.width
            )
        })?;
    Ok((addr_of(bits).to_string(), i64::from(length)))
}

fn checked(addr: &str, len: i64) -> Result<(Bits, u8), String> {
    let bits = parse_addr(addr)?;
    let len = u8::try_from(len)
        .ok()
        .filter(|&n| n <= bits.width)
        .ok_or_else(|| format!("netip: no /{len} prefix of {addr}"))?;
    Ok((bits, len))
}

/// One operation on the prefix `addr/len`: 0 its masked address, 1 its first
/// address, 2 its last, 3 the address after `addr`.
///
/// # Errors
/// An address or length the prefix could not hold, an unknown operation, or
/// a successor past the last address of the family.
pub fn op(op: i64, addr: &str, len: i64) -> Result<String, String> {
    let (bits, len) = checked(addr, len)?;
    let network = bits.value & mask(bits.width, len);
    let value = match op {
        0 | 1 => network,
        2 => network | (mask(bits.width, bits.width) & !mask(bits.width, len)),
        3 => {
            let next = bits
                .value
                .checked_add(1)
                .filter(|&v| v <= mask(bits.width, bits.width));
            next.ok_or_else(|| format!("netip: {addr} is the last address"))?
        }
        other => return Err(format!("netip: no prefix operation {other}")),
    };
    Ok(addr_of(Bits {
        value,
        width: bits.width,
    })
    .to_string())
}

/// Whether the prefix `addr/len` holds `candidate`. An address of the other
/// family, or text that is not an address, is never inside.
#[must_use]
pub fn contains(addr: &str, len: i64, candidate: &str) -> bool {
    let (Ok((bits, len)), Ok(other)) = (checked(addr, len), parse_addr(candidate)) else {
        return false;
    };
    bits.width == other.width
        && bits.value & mask(bits.width, len) == other.value & mask(bits.width, len)
}

/// Whether the prefixes `a/a_len` and `b/b_len` share an address.
#[must_use]
pub fn overlaps(a: &str, a_len: i64, b: &str, b_len: i64) -> bool {
    let (Ok((a, a_len)), Ok((b, b_len))) = (checked(a, a_len), checked(b, b_len)) else {
        return false;
    };
    let shorter = mask(a.width, a_len.min(b_len));
    a.width == b.width && a.value & shorter == b.value & shorter
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_keeps_its_address_and_masks_on_request() {
        assert_eq!(parse("10.0.0.1/8").unwrap(), ("10.0.0.1".to_string(), 8));
        assert_eq!(op(0, "10.0.0.1", 8).unwrap(), "10.0.0.0");
        assert_eq!(op(2, "10.0.0.1", 8).unwrap(), "10.255.255.255");
        assert_eq!(op(1, "2001:db8::1", 64).unwrap(), "2001:db8::");
        assert_eq!(
            op(2, "2001:db8::1", 64).unwrap(),
            "2001:db8::ffff:ffff:ffff:ffff"
        );
        assert_eq!(op(2, "0.0.0.0", 0).unwrap(), "255.255.255.255");
        assert_eq!(
            op(2, "::", 0).unwrap(),
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
        );
        assert_eq!(op(1, "1.2.3.4", 32).unwrap(), "1.2.3.4");
        assert_eq!(op(3, "10.0.0.255", 8).unwrap(), "10.0.1.0");
        assert!(op(3, "255.255.255.255", 32).is_err());
    }

    #[test]
    fn lengths_outside_the_family_and_odd_spellings_are_refused() {
        assert!(parse("10.0.0.0/33").is_err());
        assert!(parse("::/129").is_err());
        assert!(parse("10.0.0.0/08").is_err());
        assert!(parse("10.0.0.0/-1").is_err());
        assert!(parse("10.0.0.0").is_err());
        assert!(parse("10.0.0/8").is_err());
        assert_eq!(parse("::/128").unwrap().1, 128);
    }

    #[test]
    fn families_do_not_mix() {
        assert!(contains("10.0.0.0", 8, "10.1.2.3"));
        assert!(!contains("10.0.0.0", 8, "11.0.0.0"));
        assert!(!contains("10.0.0.0", 8, "::ffff:10.0.0.1"));
        assert!(!contains("::", 0, "10.0.0.1"));
        assert!(overlaps("10.0.0.0", 8, "10.1.0.0", 16));
        assert!(!overlaps("10.0.0.0", 16, "10.1.0.0", 16));
        assert!(!overlaps("0.0.0.0", 0, "::", 0));
    }
}
