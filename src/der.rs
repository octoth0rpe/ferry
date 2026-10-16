//! Just enough of a DER/X.509 reader for what ferry needs to know about a
//! certificate: when it expires, whether it's self-signed, and the names in
//! its subjectAltName.
//!
//! This isn't a general X.509 parser and doesn't verify anything; rustls has
//! already checked the certificate and key by the time this runs. Every step
//! is checked, so malformed input gives `None`, never a panic.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::date;

/// What ferry reads from a certificate.
#[derive(Debug, PartialEq)]
pub struct Cert {
    /// The end of the validity period, as a Unix timestamp.
    pub not_after: i64,
    /// Whether the issuer and subject names are identical.
    pub self_signed: bool,
    /// DNS names and IP addresses from the subjectAltName extension.
    pub names: Vec<String>,
}

const SEQUENCE: u8 = 0x30;
const OID: u8 = 0x06;
const BOOLEAN: u8 = 0x01;
const OCTET_STRING: u8 = 0x04;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
/// `[0]` explicit, holding the version.
const VERSION: u8 = 0xa0;
/// `[3]` explicit, holding the extensions.
const EXTENSIONS: u8 = 0xa3;
/// GeneralName `[2]` dNSName and `[7]` iPAddress, both implicit.
const DNS_NAME: u8 = 0x82;
const IP_ADDRESS: u8 = 0x87;
/// 2.5.29.17, subjectAltName.
const SAN_OID: &[u8] = &[0x55, 0x1d, 0x11];

/// Reads the parts of a DER-encoded certificate that ferry uses.
pub fn parse(der: &[u8]) -> Option<Cert> {
    let cert = Reader(der).only(SEQUENCE)?;
    let mut tbs = Reader(Reader(cert).next(SEQUENCE)?);
    if tbs.peek() == Some(VERSION) {
        tbs.any()?;
    }
    tbs.any()?; // serialNumber
    tbs.next(SEQUENCE)?; // signature algorithm
    let issuer = tbs.next(SEQUENCE)?;
    let mut validity = Reader(tbs.next(SEQUENCE)?);
    validity.any()?; // notBefore
    let not_after = time(validity.any()?)?;
    let subject = tbs.next(SEQUENCE)?;
    tbs.next(SEQUENCE)?; // subjectPublicKeyInfo

    // Then optional issuerUniqueID [1], subjectUniqueID [2], extensions [3].
    let mut names = Vec::new();
    while !tbs.0.is_empty() {
        let (tag, value) = tbs.any()?;
        if tag == EXTENSIONS {
            names = san(Reader(value).only(SEQUENCE)?)?;
        }
    }

    Some(Cert {
        not_after,
        self_signed: issuer == subject,
        names,
    })
}

/// The names in the subjectAltName extension, if there is one, from the
/// contents of the extensions SEQUENCE.
fn san(extensions: &[u8]) -> Option<Vec<String>> {
    let mut extensions = Reader(extensions);
    while !extensions.0.is_empty() {
        let mut ext = Reader(extensions.next(SEQUENCE)?);
        if ext.next(OID)? != SAN_OID {
            continue;
        }
        if ext.peek() == Some(BOOLEAN) {
            ext.any()?; // critical
        }
        let mut general_names = Reader(Reader(ext.next(OCTET_STRING)?).only(SEQUENCE)?);
        let mut names = Vec::new();
        while !general_names.0.is_empty() {
            match general_names.any()? {
                (DNS_NAME, v) => names.extend(std::str::from_utf8(v).ok().map(String::from)),
                (IP_ADDRESS, v) => {
                    if let Ok(v4) = <[u8; 4]>::try_from(v) {
                        names.push(Ipv4Addr::from(v4).to_string());
                    } else if let Ok(v6) = <[u8; 16]>::try_from(v) {
                        names.push(Ipv6Addr::from(v6).to_string());
                    }
                }
                _ => {}
            }
        }
        return Some(names);
    }
    Some(Vec::new())
}

/// A UTCTime or GeneralizedTime as a Unix timestamp. Only the forms DER
/// allows: UTC (`Z`), with seconds and no fraction.
fn time((tag, value): (u8, &[u8])) -> Option<i64> {
    let (year, rest) = match tag {
        UTC_TIME => {
            let yy = digits(value.get(..2)?)?;
            // RFC 5280: YY of 50 and above means 19YY.
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &value[2..])
        }
        GENERALIZED_TIME => (digits(value.get(..4)?)?, &value[4..]),
        _ => return None,
    };
    if rest.len() != 11 || rest[10] != b'Z' {
        return None;
    }
    let field = |i: usize| digits(&rest[i..i + 2]);
    let (month, day) = (field(0)?, field(2)?);
    let (hour, minute, second) = (field(4)?, field(6)?, field(8)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = date::days_from_civil(year, month as u32, day as u32);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// ASCII decimal digits as a number.
fn digits(s: &[u8]) -> Option<i64> {
    s.iter().try_fold(0, |n, &b| {
        b.is_ascii_digit().then(|| n * 10 + i64::from(b - b'0'))
    })
}

/// Reads tag-length-value items from the front of a byte slice.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn peek(&self) -> Option<u8> {
        self.0.first().copied()
    }

    /// The next item's tag and contents.
    fn any(&mut self) -> Option<(u8, &'a [u8])> {
        let (&tag, rest) = self.0.split_first()?;
        let (&first, rest) = rest.split_first()?;
        let (len, rest) = if first < 0x80 {
            (usize::from(first), rest)
        } else {
            // Long form: the low bits count the length bytes that follow. 0
            // would be BER's indefinite length, which DER doesn't allow.
            let n = usize::from(first & 0x7f);
            if n == 0 || n > 4 {
                return None;
            }
            let len = rest
                .get(..n)?
                .iter()
                .fold(0usize, |len, &b| len << 8 | usize::from(b));
            (len, &rest[n..])
        };
        let value = rest.get(..len)?;
        self.0 = &rest[len..];
        Some((tag, value))
    }

    /// The contents of the next item, which must have tag `tag`.
    fn next(&mut self, tag: u8) -> Option<&'a [u8]> {
        self.any().filter(|&(t, _)| t == tag).map(|(_, v)| v)
    }

    /// The contents of the only item, which must have tag `tag`.
    fn only(mut self, tag: u8) -> Option<&'a [u8]> {
        let value = self.next(tag)?;
        self.0.is_empty().then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, date_time_ymd};

    fn cert(names: &[&str], not_after: (i32, u8, u8)) -> Vec<u8> {
        let mut params =
            CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .unwrap();
        params.not_after = date_time_ymd(not_after.0, not_after.1, not_after.2);
        let key = KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().der().to_vec()
    }

    #[test]
    fn reads_self_signed_cert() {
        let der = cert(
            &["localhost", "127.0.0.1", "::1", "a.example"],
            (2031, 2, 28),
        );
        assert_eq!(
            parse(&der).unwrap(),
            Cert {
                not_after: date::days_from_civil(2031, 2, 28) * 86_400,
                self_signed: true,
                names: vec![
                    "localhost".into(),
                    "127.0.0.1".into(),
                    "::1".into(),
                    "a.example".into()
                ],
            }
        );
    }

    #[test]
    fn reads_generalized_time() {
        // rcgen writes dates from 2050 on as GeneralizedTime.
        let der = cert(&["localhost"], (2061, 7, 4));
        assert_eq!(
            parse(&der).unwrap().not_after,
            date::days_from_civil(2061, 7, 4) * 86_400
        );
    }

    #[test]
    fn reads_times() {
        let t = |tag, s: &str| time((tag, s.as_bytes()));
        assert_eq!(t(UTC_TIME, "700101000000Z"), Some(0));
        assert_eq!(t(UTC_TIME, "491231235959Z"), Some(2_524_607_999));
        assert_eq!(t(GENERALIZED_TIME, "19700101000001Z"), Some(1));
        assert_eq!(t(UTC_TIME, "7001010000Z"), None);
        assert_eq!(t(UTC_TIME, "700101000000+0100"), None);
        assert_eq!(t(UTC_TIME, "701301000000Z"), None);
        assert_eq!(t(UTC_TIME, "70010100000aZ"), None);
        assert_eq!(t(OCTET_STRING, "700101000000Z"), None);
    }

    #[test]
    fn ca_issued_cert_is_not_self_signed() {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Test CA");
        let ca = Issuer::new(ca_params, KeyPair::generate().unwrap());

        let params = CertificateParams::new(vec!["example.com".to_string()]).unwrap();
        let key = KeyPair::generate().unwrap();
        let der = params.signed_by(&key, &ca).unwrap().der().to_vec();
        let cert = parse(&der).unwrap();
        assert!(!cert.self_signed);
        assert_eq!(cert.names, vec!["example.com".to_string()]);
    }

    #[test]
    fn no_san_means_no_names() {
        let der = cert(&[], (2030, 1, 1));
        assert_eq!(parse(&der).unwrap().names, Vec::<String>::new());
    }

    #[test]
    fn long_form_lengths() {
        let mut r = Reader(&[0x04, 0x82, 0x01, 0x00]);
        assert_eq!(r.any(), None); // 256 bytes promised, none present
        let mut data = vec![0x04, 0x81, 0x80];
        data.extend([7; 0x80]);
        data.push(0x05);
        data.push(0x00);
        let mut r = Reader(&data);
        assert_eq!(r.any().unwrap().1.len(), 0x80);
        assert_eq!(r.any(), Some((0x05, &[][..])));
        assert_eq!(Reader(&[0x30, 0x80, 0x00, 0x00]).any(), None); // indefinite
    }

    #[test]
    fn truncated_and_garbage_input_never_panics() {
        let der = cert(&["localhost", "127.0.0.1"], (2030, 1, 1));
        for len in 0..der.len() {
            assert_eq!(parse(&der[..len]), None, "truncated to {len}");
        }

        // Corrupt every byte in turn, then random bytes. Anything may come
        // back, but it mustn't panic.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for i in 0..der.len() {
            let mut bad = der.clone();
            bad[i] ^= (rand() as u8) | 1;
            let _ = parse(&bad);
        }
        for _ in 0..2000 {
            let len = (rand() % 600) as usize;
            let mut bad: Vec<u8> = (0..len).map(|_| rand() as u8).collect();
            if let Some(b) = bad.first_mut() {
                *b = SEQUENCE;
            }
            let _ = parse(&bad);
        }
    }
}
