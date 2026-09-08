use super::Tag;
use crate::base::{Bytes, BytesCow, HasReplacementsError, Range};
use encoding_rs::Encoding;
use std::fmt;

// NOTE: All standard tag names contain only ASCII alpha characters
// and digits from 1 to 6 (in numbered header tags, i.e. <h1> - <h6>).
// Considering that tag names are case insensitive we have only
// 26 + 6 = 32 characters. Thus, single character can be encoded in
// 5 bits and we can fit up to 64 / 5 ≈ 12 characters in a 64-bit
// integer. This is enough to encode all standard tag names, so
// we can just compare integers instead of expensive string
// comparison for tag names.
//
// The original idea of this tag hash-like thing belongs to Ingvar
// Stepanyan and was implemented in lazyhtml. So, kudos to him for
// comming up with this cool optimisation. This implementation differs
// from the original one as it adds ability to encode digits from 1
// to 6 which allows us to encode numbered header tags.
//
// In this implementation we reserve numbers from 0 to 5 for digits
// from 1 to 6 and numbers from 6 to 31 for ASCII alphas. Otherwise,
// if we use numbers from 0 to 25 for ASCII alphas we'll have an
// ambiguity for repetitative `a` characters: both `a`,
// `aaa` and even `aaaaa` will give us 0 as a hash. It's still a case
// for digits, but considering that tag name can't start with a digit
// we are safe here, since we'll just get first character shifted left
// by zeroes as repetitave 1 digits get added to the hash.
//
// LocalNameHash is built incrementally as tags are parsed, so it needs
// to be able to invalidate itself if parsing an unrepresentable name.
// `EMPTY_HASH` is used as a sentinel value.
//
/// A tag name of up to twelve `[a-zA-Z1-6]` characters packed five bits per
/// character into a `u64`; compares as an integer. See the notes above.
#[derive(PartialEq, Eq, Copy, Clone, Default, Hash)]
pub struct LocalNameHash(u64);

const EMPTY_HASH: u64 = !0;

impl LocalNameHash {
    #[inline]
    #[must_use]
    /// The hash of the empty name.
    pub const fn new() -> Self {
        Self(0)
    }

    #[inline]
    #[must_use]
    /// Whether the name was not representable (too long, or other characters).
    pub const fn is_empty(&self) -> bool {
        self.0 == EMPTY_HASH
    }

    /// Appends one character.
    #[inline]
    pub const fn update(&mut self, ch: u8) {
        let h = self.0;

        // NOTE: check if we still have space for yet another
        // character and if not then invalidate the hash.
        // Note, that we can't have `1` (which is encoded as 0b00000) as
        // a first character of a tag name, so it's safe to perform
        // check this way.
        // EMPTY_HASH has all bits set, so it will fail this check.
        self.0 = if h >> (64 - 5) == 0 {
            match ch {
                // NOTE: apply 0x1F mask on ASCII alpha to convert it to the
                // number from 1 to 26 (character case is controlled by one of
                // upper bits which we eliminate with the mask). Then add
                // 5, since numbers from 0 to 5 are reserved for digits.
                // Aftwerards put result as 5 lower bits of the hash.
                b'a'..=b'z' | b'A'..=b'Z' => (h << 5) | ((ch as u64 & 0x1F) + 5),

                // NOTE: apply 0x0F mask on ASCII digit to convert it to number
                // from 1 to 6. Then subtract 1 to make it zero-based.
                // Afterwards, put result as lower bits of the hash.
                b'1'..=b'6' => (h << 5) | ((ch as u64 & 0x0F) - 1),

                // NOTE: for any other characters hash function is not
                // applicable, so we completely invalidate the hash.
                _ => EMPTY_HASH,
            }
        } else {
            EMPTY_HASH
        };
    }
}

impl LocalNameHash {
    /// Hashes a complete name; usable in `const` context, e.g. to build
    /// `match` arms over [`as_u64`](Self::as_u64) values.
    #[must_use]
    pub const fn from_ascii(name: &[u8]) -> Self {
        let mut hash = Self::new();
        let mut i = 0;
        while i < name.len() {
            hash.update(name[i]);
            i += 1;
        }
        hash
    }

    /// The packed value (see the encoding notes above); `!0` when the name
    /// was not representable.
    #[inline]
    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Unpacks the hash back into the ASCII-lowercase tag name it encodes.
    /// The encoding is reversible for every representable name (the first
    /// character of a tag name is always a letter, so leading zero groups are
    /// unambiguous padding). Returns `None` for the empty/invalid hash.
    #[inline]
    pub fn decode<'b>(&self, buf: &'b mut [u8; 12]) -> Option<&'b str> {
        if self.is_empty() || self.0 == 0 {
            return None;
        }
        let mut pos = 12;
        let mut h = self.0;
        loop {
            pos -= 1;
            buf[pos] = match (h & 31) as u8 {
                v @ 6.. => v + (b'a' - 6),
                v => v + b'1',
            };
            h >>= 5;
            if h == 0 || pos == 0 {
                break;
            }
        }
        // Only ASCII letters and digits were written.
        std::str::from_utf8(&buf[pos..]).ok()
    }
}

impl fmt::Debug for LocalNameHash {
    #[cold]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; 12];
        match self.decode(&mut buf) {
            Some(name) => name.fmt(f),
            None => f.write_str("N/A"),
        }
    }
}

impl From<&str> for LocalNameHash {
    #[inline]
    fn from(string: &str) -> Self {
        Self::from_ascii(string.as_bytes())
    }
}

impl PartialEq<Tag> for LocalNameHash {
    #[inline]
    fn eq(&self, tag: &Tag) -> bool {
        self.0 == *tag as u64
    }
}

/// `LocalName` is used for the comparison of tag names.
/// In the majority of cases it will be represented as a hash, however for long
/// non-standard tag names it fallsback to the Name representation.
#[derive(Clone, Debug, Eq)]
pub enum LocalName<'i> {
    /// A name representable as a [`LocalNameHash`].
    Hash(LocalNameHash),
    /// Any other name, as written (compared ASCII case-insensitively).
    Bytes(BytesCow<'i>),
}

// `PartialEq` compares `Bytes` case-insensitively, so `Hash` must case-fold too.
impl std::hash::Hash for LocalName<'_> {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
        match self {
            LocalName::Hash(h) => h.hash(state),
            LocalName::Bytes(b) => b.iter().for_each(|c| c.to_ascii_lowercase().hash(state)),
        }
    }
}

impl<'i> LocalName<'i> {
    #[inline]
    #[must_use]
    pub(crate) fn new(input: Bytes<'i>, range: Range, hash: LocalNameHash) -> Self {
        if hash.is_empty() {
            LocalName::Bytes(input.slice(range).into())
        } else {
            LocalName::Hash(hash)
        }
    }

    #[inline]
    #[must_use]
    /// Detaches the name from the input buffer.
    pub fn into_owned(self) -> LocalName<'static> {
        match self {
            LocalName::Bytes(b) => LocalName::Bytes(b.into_owned()),
            LocalName::Hash(h) => LocalName::Hash(h),
        }
    }

    #[inline]
    /// Builds a name for comparison; fails if `string` is not representable
    /// in `encoding`.
    pub fn from_str_without_replacements<'s>(
        string: &'s str,
        encoding: &'static Encoding,
    ) -> Result<LocalName<'s>, HasReplacementsError> {
        let hash = LocalNameHash::from(string);

        if hash.is_empty() {
            BytesCow::from_str_without_replacements(string, encoding).map(LocalName::Bytes)
        } else {
            Ok(LocalName::Hash(hash))
        }
    }
}

impl PartialEq<Tag> for LocalName<'_> {
    #[inline]
    fn eq(&self, tag: &Tag) -> bool {
        match self {
            LocalName::Hash(h) => h == tag,
            LocalName::Bytes(_) => false,
        }
    }
}

impl PartialEq<LocalName<'_>> for LocalName<'_> {
    #[inline]
    fn eq(&self, other: &LocalName<'_>) -> bool {
        use LocalName::{Bytes, Hash};

        match (self, other) {
            (Hash(s), Hash(o)) => {
                debug_assert!(!s.is_empty());
                s == o
            }
            (Bytes(s), Bytes(o)) => s.eq_ignore_ascii_case(o),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str() {
        assert_eq!(LocalNameHash::from("div"), LocalNameHash(9691u64));
    }

    #[test]
    fn hash_invalidation_for_non_ascii_chars() {
        assert!(LocalNameHash::from("div@&").is_empty());
    }

    #[test]
    fn hash_invalidation_for_long_values() {
        assert!(LocalNameHash::from("aaaaaaaaaaaaaa").is_empty());
    }

    #[test]
    fn bytes_variant_hash_matches_case_insensitive_eq() {
        use std::hash::{BuildHasher, RandomState};
        let s = RandomState::new();
        let a = LocalName::from_str_without_replacements("My-Widget", encoding_rs::UTF_8).unwrap();
        let b = LocalName::from_str_without_replacements("my-widget", encoding_rs::UTF_8).unwrap();
        assert!(matches!(a, LocalName::Bytes(_)));
        assert_eq!(a, b);
        assert_eq!(s.hash_one(&a), s.hash_one(&b));
    }
}
