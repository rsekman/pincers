use std::borrow::Borrow;
use std::collections::{hash_map::Keys, HashMap};
use std::fmt::{Display, Formatter};
use std::hash::Hash;
use std::str::{self, FromStr};

use bounded_integer::{BoundedI8, BoundedU8};
use mediatype::{MediaTypeBuf, ReadParams};
use nom::{
    branch::alt,
    character::complete::{char, satisfy},
    combinator::{eof, map, value},
    sequence::terminated,
    IResult, Parser,
};
use serde::{Deserialize, Serialize};

use crate::error::Anyhow;

const N_NUMERIC: i8 = 10;
const N_NAMED: u8 = 26;

/// Type alias for a numbered [`RegisterAddress`]. Signed values are used for `NumericT` because we
/// need subtractions in pointer arithmetic
pub type NumericT = BoundedI8<0, { N_NUMERIC - 1 }>;
/// Type alias for a named [`RegisterAddress`].
pub type NamedT = BoundedU8<0, { N_NAMED - 1 }>;

/// Enum representing the name of a register, either numbered or named.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Ord, PartialOrd, Eq, PartialEq)]
pub enum RegisterAddress {
    /// The unnamed register, meaning the last used register
    Unnamed,
    /// A numbered register (`"0, "1, ...`). The field is a range-limited integer from 0..=9.
    Numeric(NumericT),
    /// A named register (`"a, "b, ...`). The field is a range-limited integer from 0..=26.
    Named(NamedT),
}

impl RegisterAddress {
    /// Iterator over all register addresses, first the numbered, then the named.
    pub fn iter() -> impl Iterator<Item = Self> {
        std::iter::once(Self::Unnamed)
            .chain(Self::iter_numeric())
            .chain(Self::iter_named())
    }

    /// Iterator over all numbered registered addresses
    pub fn iter_numeric() -> impl Iterator<Item = Self> {
        (0i8..N_NUMERIC).map(|n| Self::Numeric(NumericT::new(n).unwrap()))
    }

    /// Iterator over all named registered addresses
    pub fn iter_named() -> impl Iterator<Item = Self> {
        (0u8..N_NAMED).map(|n| Self::Named(NamedT::new(n).unwrap()))
    }
}

impl Default for RegisterAddress {
    /// The default register is the unnamed register
    fn default() -> Self {
        Self::Unnamed
    }
}
impl Display for RegisterAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        use RegisterAddress::*;
        if !f.alternate() {
            write!(f, "\"")?;
        }
        match self {
            Numeric(n) => write!(f, "{}", n),
            Named(n) => write!(f, "{}", ascii_shift('a', n.get()).unwrap()),
            Unnamed => write!(f, "\""),
        }
    }
}

pub const ADDRESS_HELP: &str = "A character from [\"0-9a-z]";

fn ascii_distance(a: char, b: char) -> Option<u8> {
    u8::try_from((a as u32) - (b as u32)).ok()
}

fn ascii_shift(a: char, b: u8) -> Option<char> {
    u8::try_from((a as u32) + (b as u32))
        .map(|n| n as char)
        .ok()
}
/// Try to parse an ASCII alphanumeric character into a [`RegisterAddress`]
fn do_parse_address(input: &str) -> IResult<&str, RegisterAddress> {
    alt((
        map(satisfy(|c| c.is_ascii_digit()), |c| {
            RegisterAddress::Numeric(
                ascii_distance(c, '0')
                    .and_then(|d| NumericT::new(d as i8))
                    .unwrap(),
            )
        }),
        map(satisfy(|c| c.is_ascii_lowercase()), |c| {
            RegisterAddress::Named(ascii_distance(c, 'a').and_then(NamedT::new).unwrap())
        }),
        value(RegisterAddress::Unnamed, char('"')),
    ))
    .parse(input)
}

impl FromStr for RegisterAddress {
    type Err = Anyhow;
    fn from_str(input: &str) -> Result<RegisterAddress, Anyhow> {
        terminated(do_parse_address, eof)
            .parse(input)
            .map(|(_, addr)| addr)
            .map_err(|_| {
                Anyhow::msg("Register address must be a single character from [\"0-9a-z].")
            })
    }
}

/// Type alias to identify a MIME type. Could possibly change to something more sophisticated in
/// the future.
pub type MimeType = MediaTypeBuf;
type DataBuffer = Vec<u8>;

/// A `Register` is a map from MIME types to buffers containing the data of the respective MIME types
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Register {
    map: HashMap<MimeType, DataBuffer>,
}

/// Enum representing a summary of the contents of a register.
#[derive(Serialize, Deserialize, Debug)]
pub enum RegisterSummary {
    /// The register contains text/plain data, ellipsize but also store the length
    Text(String, usize),
    /// The register contains something else, summarize with its MIME type and length
    Blob(MimeType, usize),
    Empty,
}

impl Display for RegisterSummary {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            Self::Text(s, _) => write!(f, "{}", s.trim_end_matches(char::is_whitespace)),
            Self::Blob(m, n) => write!(
                f,
                "blob ({m}, {})",
                humanize_bytes::humanize_bytes_binary!(*n)
            ),
            Self::Empty => write!(f, "<EMPTY>"),
        }
    }
}

impl Register {
    pub fn new() -> Self {
        Register {
            map: HashMap::new(),
        }
    }

    /// Return a summary of the contents of this register.
    pub fn summarize(&self) -> RegisterSummary {
        let text_types = vec!["text/plain; charset=utf-8", "text/plain", "text/*"];
        if let Some((_, data)) = text_types
            .iter()
            .map(|s| MediaTypeBuf::from_str(s).unwrap())
            .filter_map(|m| self.get(&m))
            .next()
        {
            // TODO for now, assume text data is valid UTF-8
            // TODO implement ellipsis
            let s = String::from_utf8(data.clone()).unwrap();
            let n = s.len();
            RegisterSummary::Text(s, n)
        } else if let Some((m, data)) = self.map.iter().next() {
            RegisterSummary::Blob(m.clone(), data.len())
        } else {
            RegisterSummary::Empty
        }
    }

    /// Clear all the data from this register
    pub fn clear(&mut self) {
        self.map.clear()
    }

    /// Insert new data of a specified MIME type into the register
    pub fn insert(&mut self, mime: MimeType, data: DataBuffer) -> Option<DataBuffer> {
        self.map.insert(mime, data)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Get the data buffer corresponding to given MIME type
    pub fn get<'b, 'a>(&'a self, mime: &'b MimeType) -> Option<(&'a MimeType, &'a DataBuffer)> {
        let has_params = mime.params().count() > 0;
        if mime.subty() != "*" && has_params {
            // exact match
            self.map.get_key_value(mime)
        } else if mime.subty() != "*" {
            // match by essence
            self.map.iter().find(|(m, _)| m.essence() == mime.essence())
        } else if mime.ty() != "*" {
            // match by type, eg image/* matches image/png
            self.map.iter().find(|(m, _)| m.ty() == mime.ty())
        } else {
            // */* -- pick the first type
            self.map.iter().next()
        }
    }

    pub fn keys(&self) -> Keys<'_, MimeType, DataBuffer> {
        self.map.keys()
    }

    pub fn has_mime<Q>(&self, mime: &Q) -> bool
    where
        MimeType: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        return self.map.contains_key(mime);
    }
}

impl<'a> IntoIterator for &'a Register {
    type Item = (&'a MimeType, &'a DataBuffer);
    type IntoIter = std::collections::hash_map::Iter<'a, MimeType, DataBuffer>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.iter()
    }
}

impl IntoIterator for Register {
    type Item = (MimeType, DataBuffer);
    type IntoIter = std::collections::hash_map::IntoIter<MimeType, DataBuffer>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.into_iter()
    }
}
