//! An order-preserving byte encoding of keys: two keys compare as their
//! encodings compare byte by byte, so the built-in order needs no callback.

/// Builds the encoding of one key, field by field in declaration order.
#[derive(Debug, Default, Clone)]
pub struct KeyEncoder {
    bytes: Vec<u8>,
}

impl KeyEncoder {
    /// An empty encoding.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The finished encoding.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }

    /// A signed integer: big-endian with the sign bit flipped, so negatives
    /// order first.
    pub fn int(&mut self, v: i64) -> &mut Self {
        self.bytes
            .extend_from_slice(&((v as u64) ^ (1 << 63)).to_be_bytes());
        self
    }

    /// An unsigned integer: big-endian.
    pub fn uint(&mut self, v: u64) -> &mut Self {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self
    }

    /// A float in IEEE total order. `-0.0` and `0.0` stay distinct keys, as
    /// they are for `Map`.
    pub fn float(&mut self, v: f64) -> &mut Self {
        let bits = v.to_bits();
        let ordered = if bits >> 63 == 1 {
            !bits
        } else {
            bits | (1 << 63)
        };
        self.uint(ordered)
    }

    /// A `bool`: `false` before `true`.
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.bytes.push(u8::from(v));
        self
    }

    /// A `char` by scalar value.
    pub fn char(&mut self, v: char) -> &mut Self {
        self.bytes.extend_from_slice(&u32::from(v).to_be_bytes());
        self
    }

    /// A string or byte sequence. Each `0x00` is escaped as `0x00 0xFF` and
    /// the run ends in `0x00 0x00`, so a proper prefix orders first and a
    /// field that follows cannot bleed into this one's order.
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        for &b in v {
            self.bytes.push(b);
            if b == 0 {
                self.bytes.push(0xFF);
            }
        }
        self.bytes.extend_from_slice(&[0, 0]);
        self
    }

    /// Opens an element of a variable-length sequence; a sequence ends with
    /// [`KeyEncoder::seq_end`], so a proper prefix orders first.
    pub fn seq_elem(&mut self) -> &mut Self {
        self.bytes.push(1);
        self
    }

    /// Closes a variable-length sequence.
    pub fn seq_end(&mut self) -> &mut Self {
        self.bytes.push(0);
        self
    }

    /// An enum, `Option`, or `Result` variant by rank; the payload follows.
    pub fn variant(&mut self, rank: u32) -> &mut Self {
        self.bytes.extend_from_slice(&rank.to_be_bytes());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::KeyEncoder;

    fn enc(f: impl FnOnce(&mut KeyEncoder)) -> Vec<u8> {
        let mut e = KeyEncoder::new();
        f(&mut e);
        e.finish()
    }

    #[test]
    fn integers_order_by_value() {
        let xs = [i64::MIN, -5, -1, 0, 1, 7, i64::MAX];
        let encoded: Vec<_> = xs.iter().map(|&x| enc(|e| _ = e.int(x))).collect();
        assert!(encoded.windows(2).all(|w| w[0] < w[1]));
        assert!(enc(|e| _ = e.uint(u64::MAX)) > enc(|e| _ = e.uint(1 << 63)));
    }

    #[test]
    fn floats_order_in_total_order() {
        let xs = [
            f64::NEG_INFINITY,
            -1.5,
            -0.0,
            0.0,
            1e-300,
            2.0,
            f64::INFINITY,
            f64::NAN,
        ];
        let encoded: Vec<_> = xs.iter().map(|&x| enc(|e| _ = e.float(x))).collect();
        assert!(encoded.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn strings_order_with_prefixes_first_across_fields() {
        let pair = |a: &str, b: i64| enc(|e| _ = e.bytes(a.as_bytes()).int(b));
        assert!(pair("a", 9) < pair("ab", 0));
        assert!(pair("a\0", 0) > pair("a", 9));
        assert!(pair("b", -1) > pair("a\u{10FFFF}", 5));
    }

    #[test]
    fn sequences_order_lexicographically() {
        let seq = |xs: &[i64]| {
            enc(|e| {
                for &x in xs {
                    e.seq_elem().int(x);
                }
                e.seq_end();
            })
        };
        assert!(seq(&[]) < seq(&[i64::MIN]));
        assert!(seq(&[1, 2]) < seq(&[1, 2, 0]));
        assert!(seq(&[1, 3]) > seq(&[1, 2, 9]));
    }
}
