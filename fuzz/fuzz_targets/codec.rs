#![no_main]

use libfuzzer_sys::fuzz_target;

use gossamer_runtime::codec::base32::{self, Alphabet};
use gossamer_runtime::codec::{ascii85, base64, csv, hex, html, pem, varint};

// Every decoder answers rather than panics, and anything it accepts
// re-encodes to text that decodes to the same bytes.
fuzz_target!(|data: &[u8]| {
    if data.len() > 16 * 1024 {
        return;
    }
    if let Ok((value, used)) = varint::decode_unsigned(data) {
        let canonical = varint::encode_unsigned(value);
        assert!(canonical.len() <= used);
        assert_eq!(varint::decode_unsigned(&canonical), Ok((value, canonical.len())));
    }
    let _ = varint::decode_signed(data);
    let Ok(text) = std::str::from_utf8(data) else {
        let _ = base64::decode_bytes(data);
        return;
    };
    if let Ok(bytes) = ascii85::decode(text) {
        assert_eq!(ascii85::decode(&ascii85::encode(&bytes)).as_deref(), Ok(bytes.as_slice()));
    }
    for alphabet in [Alphabet::Standard, Alphabet::Hex] {
        if let Ok(bytes) = base32::decode(text, alphabet) {
            let canonical = base32::encode(&bytes, alphabet);
            assert_eq!(base32::decode(&canonical, alphabet).as_deref(), Ok(bytes.as_slice()));
        }
    }
    if let Ok(bytes) = base64::decode(text) {
        assert_eq!(base64::decode(&base64::encode(&bytes)).as_deref(), Ok(bytes.as_slice()));
    }
    if let Ok(bytes) = hex::decode(text) {
        assert_eq!(hex::decode(&hex::encode(&bytes)).as_deref(), Ok(bytes.as_slice()));
    }
    assert_eq!(html::unescape(&html::escape(text)), text);
    if let Ok(records) = csv::read(text) {
        assert_eq!(csv::read(&csv::write(&records)), Ok(records));
    }
    if let Ok(blocks) = pem::decode_all(text) {
        for block in blocks {
            let encoded = pem::encode(&block.label, &block.bytes);
            assert_eq!(pem::decode(&encoded).map(|(b, _)| b), Ok(block));
        }
    }
});
