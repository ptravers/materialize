// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::error::Error;
use std::fmt;

use anyhow::bail;
use mz_lowertest::MzReflect;
use mz_ore::cast::CastFrom;
use mz_proto::{RustType, TryFromProtoError};
use proptest::arbitrary::Arbitrary;
use proptest::strategy::{BoxedStrategy, Strategy};
use serde::{Deserialize, Serialize};

include!(concat!(env!("OUT_DIR"), "/mz_repr.adt.char.rs"));

// https://github.com/postgres/postgres/blob/REL_14_0/src/include/access/htup_details.h#L577-L584
const MAX_LENGTH: u32 = 10_485_760;

/// A marker type indicating that a Rust string should be interpreted as a
/// [`ScalarType::Char`].
///
/// [`ScalarType::Char`]: crate::ScalarType::Char
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Char<S: AsRef<str>>(pub S);

/// The `length` of a [`ScalarType::Char`].
///
/// This newtype wrapper ensures that the length is within the valid range.
///
/// [`ScalarType::Char`]: crate::ScalarType::Char
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize, MzReflect,
)]
pub struct CharLength(pub(crate) u32);

impl CharLength {
    /// A length of one.
    pub const ONE: CharLength = CharLength(1);

    /// Consumes the newtype wrapper, returning the inner `u32`.
    pub fn into_u32(self) -> u32 {
        self.0
    }
}

impl TryFrom<i64> for CharLength {
    type Error = InvalidCharLengthError;

    fn try_from(length: i64) -> Result<Self, Self::Error> {
        match u32::try_from(length) {
            Ok(length) if length > 0 && length < MAX_LENGTH => Ok(CharLength(length)),
            _ => Err(InvalidCharLengthError),
        }
    }
}

impl Arbitrary for CharLength {
    type Parameters = ();
    type Strategy = BoxedStrategy<CharLength>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        proptest::arbitrary::any::<u32>()
            // We cap the maximum CharLength to prevent generating massive
            // strings which can greatly slow down tests and are relatively
            // uninteresting.
            .prop_map(|len| CharLength(len % 300))
            .boxed()
    }
}

/// The error returned when constructing a [`CharLength`] from an invalid value.
#[derive(Debug, Clone)]
pub struct InvalidCharLengthError;

impl fmt::Display for InvalidCharLengthError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "length for type character must be between 1 and {}",
            MAX_LENGTH
        )
    }
}

impl Error for InvalidCharLengthError {}

/// Controls how to handle trailing whitespace at the end of bpchar data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum CharWhiteSpace {
    /// Trim all whitespace from strings, which is appropriate for storing
    /// bpchar data in Materialize. bpchar data is stored in datums with its
    /// trailing whitespace trimmed to enforce the same equality semantics as
    /// PG, while also allowing us to bit-wise equality on rows.
    Trim,
    /// Blank pad strings, which is appropriate for returning bpchar data out of Materialize.
    Pad,
}

impl CharWhiteSpace {
    fn process_str(&self, s: &str, length: Option<usize>) -> String {
        use CharWhiteSpace::*;
        match self {
            Trim => s.trim_end().to_string(),
            Pad => match length {
                Some(length) => format!("{:width$}", s, width = length),
                // This occurs when e.g. printing lists
                None => s.to_string(),
            },
        }
    }
}

/// Returns `s` as a `String` with options to enforce char and varchar
/// semantics.
///
/// # Arguments
/// * `s` - The `str` to format
/// * `length` - An optional maximum length for the string
/// * `fail_on_len` - Return an error if `s`'s character count exceeds the
///   specified maximum length.
/// * `white_space` - Express how to handle trailing whitespace on `s`
///
/// This function should only fail when `fail_on_len` is `true` _and_ `length`
/// is present and exceeded.
fn format_char_str(
    s: &str,
    length: Option<CharLength>,
    fail_on_len: bool,
    white_space: CharWhiteSpace,
) -> Result<String, anyhow::Error> {
    Ok(match length {
        // Note that length is 1-indexed, so finding `None` means the string's
        // characters don't exceed the length, while finding `Some` means it
        // does.
        Some(l) => {
            let l = usize::cast_from(l.into_u32());
            // The number of chars in a string is always less or equal to the length of the string.
            // Hence, if the string is shorter than the length, we do not have to check for
            // the maximum length.
            if s.len() < l {
                return Ok(white_space.process_str(s, Some(l)));
            }
            match s.char_indices().nth(l) {
                None => white_space.process_str(s, Some(l)),
                Some((idx, _)) => {
                    if !fail_on_len || s[idx..].chars().all(|c| c.is_ascii_whitespace()) {
                        white_space.process_str(&s[..idx], Some(l))
                    } else {
                        bail!("{} exceeds maximum length of {}", s, l)
                    }
                }
            }
        }
        None => white_space.process_str(s, None),
    })
}

/// Ensures that `s` has fewer than `length` characters, and returns a `String`
/// version of it with all whitespace trimmed from the end.
///
/// The value returned is appropriate to store in `Datum::String`, but _is not_
/// appropriate to return to clients.
///
/// This function should only fail when `fail_on_len` is `true` _and_ `length`
/// is present and exceeded.
pub fn format_str_trim(
    s: &str,
    length: Option<CharLength>,
    fail_on_len: bool,
) -> Result<String, anyhow::Error> {
    format_char_str(s, length, fail_on_len, CharWhiteSpace::Trim)
}

/// Ensures that `s` has fewer than `length` characters, and returns a `String`
/// version of it with blank padding so that its width is `length` characters.
///
/// The value returned is appropriate to return to clients, but _is not_
/// appropriate to store in `Datum::String`.
pub fn format_str_pad(s: &str, length: Option<CharLength>) -> String {
    format_char_str(s, length, false, CharWhiteSpace::Pad).unwrap()
}

impl RustType<ProtoCharLength> for CharLength {
    fn into_proto(&self) -> ProtoCharLength {
        ProtoCharLength { value: self.0 }
    }

    fn from_proto(proto: ProtoCharLength) -> Result<Self, TryFromProtoError> {
        Ok(CharLength(proto.value))
    }
}

#[cfg(test)]
mod tests {
    use mz_ore::assert_ok;
    use mz_proto::protobuf_roundtrip;
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[mz_ore::test]
        fn char_length_protobuf_roundtrip(expect in any::<CharLength>()) {
            let actual = protobuf_roundtrip::<_, ProtoCharLength>(&expect);
            assert_ok!(actual);
            assert_eq!(actual.unwrap(), expect);
        }
    }
}
