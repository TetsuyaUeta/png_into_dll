//! Resource identifiers: either a 16-bit integer ID or a string name.

use std::cmp::Ordering;
use std::fmt;

use anyhow::{Result, bail};

#[derive(Clone, Debug)]
pub enum ResId {
    Id(u16),
    Name(String),
}

impl ResId {
    /// Parses a user-supplied identifier. Pure digits (optionally prefixed
    /// with `#`, like Windows' MAKEINTRESOURCE syntax) become an integer ID;
    /// anything else is a string name.
    pub fn parse(s: &str) -> Result<ResId> {
        let digits = s.strip_prefix('#').unwrap_or(s);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            match digits.parse::<u32>() {
                Ok(n) if (1..=0xFFFF).contains(&n) => Ok(ResId::Id(n as u16)),
                _ => bail!("resource ID `{s}` must be between 1 and 65535"),
            }
        } else if s.is_empty() {
            bail!("resource name must not be empty")
        } else {
            Ok(ResId::Name(s.to_string()))
        }
    }

    /// Sort key matching the order Windows expects in a resource directory:
    /// named entries first (compared case-insensitively as UTF-16), then
    /// integer IDs in ascending order.
    fn key(&self) -> (u8, Vec<u16>, u16) {
        match self {
            ResId::Name(s) => (0, s.to_uppercase().encode_utf16().collect(), 0),
            ResId::Id(id) => (1, Vec::new(), *id),
        }
    }
}

impl PartialEq for ResId {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ResId {}

impl PartialOrd for ResId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ResId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl fmt::Display for ResId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResId::Id(id) => write!(f, "{id}"),
            ResId::Name(s) => f.write_str(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ids_and_names() {
        assert_eq!(ResId::parse("12").unwrap(), ResId::Id(12));
        assert_eq!(ResId::parse("#7").unwrap(), ResId::Id(7));
        assert_eq!(ResId::parse("MYICON").unwrap(), ResId::Name("MYICON".into()));
        assert!(ResId::parse("0").is_err());
        assert!(ResId::parse("70000").is_err());
        assert!(ResId::parse("").is_err());
    }

    #[test]
    fn ordering_names_first_case_insensitive() {
        let mut v = vec![
            ResId::Id(5),
            ResId::Name("b".into()),
            ResId::Id(1),
            ResId::Name("A".into()),
        ];
        v.sort();
        assert_eq!(
            v,
            vec![
                ResId::Name("A".into()),
                ResId::Name("b".into()),
                ResId::Id(1),
                ResId::Id(5)
            ]
        );
        assert_eq!(ResId::Name("abc".into()), ResId::Name("ABC".into()));
    }
}
