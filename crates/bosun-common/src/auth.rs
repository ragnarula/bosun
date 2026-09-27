//! The shared password guarding the control plane: how a client presents it
//! and how the control plane checks it.
//!
//! The control plane, the CLI and the nodes all build and check the credential
//! through this module, so the three cannot drift apart. The client always
//! sends the username [`USER`]; the verifier ignores it, so the password is
//! the whole secret.

use reqwest::header::AUTHORIZATION;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;

/// The username clients send. The verifier ignores it.
pub const USER: &str = "bosun";

/// The `Authorization` header value presenting `password`: `Basic ` followed
/// by the standard base64 of `bosun:<password>`.
pub fn authorization(password: &str) -> String {
    format!("Basic {}", encode(format!("{USER}:{password}").as_bytes()))
}

/// Whether `header` presents `password`. A missing header, another scheme,
/// base64 that does not decode, and a decoded value with no colon are all
/// refusals. The part after the first colon is compared as bytes, so a
/// password holding `:` or non-ASCII text works, and the comparison is
/// constant time, so a refusal says nothing about how much of the password
/// matched.
pub fn authorized(header: Option<&str>, password: &str) -> bool {
    let Some((scheme, encoded)) = header.and_then(|header| header.split_once(' ')) else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Basic") {
        return false;
    }
    let Some(decoded) = decode(encoded) else {
        return false;
    };
    let Some(colon) = decoded.iter().position(|byte| *byte == b':') else {
        return false;
    };
    constant_time_eq(&decoded[colon + 1..], password.as_bytes())
}

/// The default headers a client presents for `password`: the `Authorization`
/// header, or none at all when no password is configured, so an unconfigured
/// client sends nothing.
pub fn client_headers(password: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(password) = password {
        let mut value = HeaderValue::from_str(&authorization(password))
            .expect("base64 is ASCII, so it is always a valid header value");
        // Marked sensitive so a debug format of the map prints `Sensitive`
        // rather than the credential.
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    headers
}

/// Compares two byte strings in constant time. Different lengths are unequal,
/// which reveals only the length of the password.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b) {
        difference |= left ^ right;
    }
    difference == 0
}

/// One character of the standard base64 alphabet (RFC 4648 section 4).
fn base64_digit(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64 with `=` padding.
fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Standard base64 with `=` padding. Returns `None` for input that is not a
/// whole number of four-character groups, holds a character outside the
/// alphabet, or pads anywhere but the last one or two characters.
fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut chunks = bytes.chunks(4).peekable();
    while let Some(chunk) = chunks.next() {
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        // Padding only ever ends the whole input: a group in the middle is a
        // character outside the alphabet, and so is a digit after a pad.
        if padding > 2 || (padding > 0 && chunks.peek().is_some()) {
            return None;
        }
        let digits = chunk.len() - padding;
        let mut value = 0u32;
        for byte in &chunk[..digits] {
            value = (value << 6) | base64_digit(*byte)?;
        }
        value <<= 6 * padding as u32;
        // The group holds `digits` six-bit values in the top of the word; the
        // bytes below them are padding.
        decoded.extend_from_slice(&value.to_be_bytes()[1..digits]);
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_header_verifies() {
        let header = authorization("correct horse");
        assert_eq!(header, "Basic Ym9zdW46Y29ycmVjdCBob3JzZQ==");
        assert!(authorized(Some(&header), "correct horse"));
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        let header = authorization("hunter2");
        let credentials = header.split_once(' ').unwrap().1;
        assert!(authorized(Some(&format!("basic {credentials}")), "hunter2"));
        assert!(authorized(Some(&format!("BASIC {credentials}")), "hunter2"));
    }

    #[test]
    fn everything_but_the_right_credential_is_refused() {
        let header = authorization("hunter2");
        assert!(!authorized(Some(&header), "hunter3"));
        assert!(!authorized(Some(&header), ""));
        assert!(!authorized(None, "hunter2"));
        assert!(!authorized(Some(""), "hunter2"));
        assert!(!authorized(Some("Bearer hunter2"), "hunter2"));
        assert!(!authorized(Some("Basic"), "hunter2"));
        assert!(!authorized(Some("Basic !!!!"), "hunter2"));
        assert!(!authorized(Some("Basic aHVudGVyMg"), "hunter2"));
        // No colon: the credential carries no password part.
        assert!(!authorized(Some("Basic aHVudGVyMg=="), "hunter2"));
    }

    #[test]
    fn a_password_holding_a_colon_and_a_non_ascii_password_round_trip() {
        let colon = authorization("p:ss:word");
        assert_eq!(colon, "Basic Ym9zdW46cDpzczp3b3Jk");
        assert!(authorized(Some(&colon), "p:ss:word"));
        // Everything after the first colon is the password, so a truncation
        // at the second one does not match.
        assert!(!authorized(Some(&colon), "p"));

        let unicode = authorization("pässwörd");
        assert!(authorized(Some(&unicode), "pässwörd"));
        assert!(!authorized(Some(&unicode), "passwörd"));
    }

    #[test]
    fn the_codecs_match_known_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");

        assert_eq!(decode("").unwrap(), b"");
        assert_eq!(decode("Zg==").unwrap(), b"f");
        assert_eq!(decode("Zm8=").unwrap(), b"fo");
        assert_eq!(decode("Zm9v").unwrap(), b"foo");
        assert_eq!(decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(decode("Zm9vYmFy").unwrap(), b"foobar");
        // The header of RFC 7617 section 2.
        assert_eq!(
            decode("QWxhZGRpbjpvcGVuIHNlc2FtZQ==").unwrap(),
            b"Aladdin:open sesame"
        );
    }

    #[test]
    fn the_decoder_rejects_bad_input() {
        assert!(decode("Zg=").is_none());
        assert!(decode("Zg").is_none());
        assert!(decode("Zm9vY").is_none());
        assert!(decode("Zm?v").is_none());
        assert!(decode("Zg==Zg==").is_none());
        assert!(decode("Zm=v").is_none());
        assert!(decode("Z===").is_none());
        assert!(decode("Zm9v\n").is_none());
    }

    #[test]
    fn client_headers_carry_the_credential_only_when_one_is_set() {
        let headers = client_headers(None);
        assert!(headers.is_empty());

        let headers = client_headers(Some("hunter2"));
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            authorization("hunter2")
        );
    }
}
