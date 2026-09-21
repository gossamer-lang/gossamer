//! Canonical HTTP status reason phrases, shared by every server tier.

/// Reason phrase RFC 9110 (and RFC 6585 for 428, 429, 431, 511) registers
/// for `status`, or `None` for a code with no registered phrase.
#[must_use]
pub const fn reason_phrase(status: u16) -> Option<&'static str> {
    Some(match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        511 => "Network Authentication Required",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::reason_phrase;

    #[test]
    fn reason_phrase_names_registered_codes() {
        assert_eq!(reason_phrase(200), Some("OK"));
        assert_eq!(reason_phrase(405), Some("Method Not Allowed"));
        assert_eq!(reason_phrase(422), Some("Unprocessable Content"));
        assert_eq!(reason_phrase(429), Some("Too Many Requests"));
    }

    #[test]
    fn reason_phrase_is_none_for_an_unregistered_code() {
        assert_eq!(reason_phrase(299), None);
        assert_eq!(reason_phrase(999), None);
    }
}
