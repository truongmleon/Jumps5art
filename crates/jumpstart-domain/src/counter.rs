use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::DomainError;

/// SQLite's non-negative integer range is also the version 1 wire counter limit.
pub const MAX_COUNTER_VALUE: u64 = i64::MAX as u64;

/// Parses canonical ASCII decimal text before a counter's range check.
///
/// # Arguments
/// - `value`: borrowed `&str` containing digits with no sign, whitespace, or
///   leading zeroes except for the single digit `0`.
///
/// # Returns
/// `Result<u64, DomainError>` containing the parsed unsigned integer. The caller
/// still checks the domain's signed 64-bit maximum and its type-specific minimum.
///
/// # Errors
/// Returns `DomainError::InvalidCounterString` for malformed text or `u64` overflow.
fn parse_decimal(value: &str) -> Result<u64, DomainError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || !bytes.iter().all(u8::is_ascii_digit)
        || (bytes.len() > 1 && bytes[0] == b'0')
    {
        return Err(DomainError::InvalidCounterString);
    }
    value.parse().map_err(|_| DomainError::InvalidCounterString)
}

/// Generates counter types with a shared representation and checked minimum.
///
/// The macro accepts a type name and a literal lower bound; both generated types
/// retain a private `u64` and use string-based Serde encoding.
macro_rules! counter {
    ($name:ident, $minimum:literal) => {
        #[doc = concat!("A counter in ", stringify!($minimum), "..=i64::MAX, serialized as decimal text.")]
        ///
        /// The private `u64` field can be initialized externally only through
        /// checked constructors or validated text decoding.
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            /// Largest value supported by this counter and the version 1 protocol.
            pub const MAX: Self = Self(MAX_COUNTER_VALUE);

            /// Checks that an unsigned integer fits this counter's domain range.
            ///
            /// # Arguments
            /// - `value`: the candidate `u64`, before applying the minimum and
            ///   signed 64-bit maximum.
            ///
            /// # Returns
            /// `Result<Self, DomainError>` with a validated counter on success.
            ///
            /// # Errors
            /// Returns `DomainError::CounterOutOfRange` outside this type's bounds.
            pub fn from_u64(value: u64) -> Result<Self, DomainError> {
                if !($minimum..=MAX_COUNTER_VALUE).contains(&value) {
                    return Err(DomainError::CounterOutOfRange {
                        value,
                        minimum: $minimum,
                    });
                }
                Ok(Self(value))
            }

            /// Copies the validated numeric value out of this small counter type.
            ///
            /// # Arguments
            /// - `self`: the counter by value; the type is `Copy`, so a caller
            ///   can continue using its original counter.
            ///
            /// # Returns
            /// The stored `u64`, already within this type's supported range.
            pub fn value(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            /// Writes the value in canonical unsigned decimal notation.
            ///
            /// # Arguments
            /// - `self`: the borrowed `&Self` counter to display.
            /// - `formatter`: a mutable `fmt::Formatter` receiving the digits.
            ///
            /// # Returns
            /// `fmt::Result`, with `Ok(())` when formatting succeeds.
            ///
            /// # Errors
            /// Returns the formatter's error if writing fails.
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = DomainError;

            /// Parses decimal text and checks this counter's numeric bounds.
            ///
            /// # Arguments
            /// - `value`: borrowed `&str` containing the candidate decimal text.
            ///
            /// # Returns
            /// `Result<Self, DomainError>` containing the validated counter.
            ///
            /// # Errors
            /// Returns `InvalidCounterString` for noncanonical text or `u64`
            /// overflow, or `CounterOutOfRange` when the parsed number is unsupported.
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_u64(parse_decimal(value)?)
            }
        }

        impl TryFrom<u64> for $name {
            type Error = DomainError;

            /// Provides standard fallible conversion using the checked constructor.
            ///
            /// # Arguments
            /// - `value`: the candidate `u64` to validate.
            ///
            /// # Returns
            /// `Result<Self, DomainError>` with the same value on success.
            ///
            /// # Errors
            /// Returns `DomainError::CounterOutOfRange` outside this type's bounds.
            fn try_from(value: u64) -> Result<Self, Self::Error> {
                Self::from_u64(value)
            }
        }

        impl Serialize for $name {
            /// Encodes decimal text so JSON clients preserve the integer exactly.
            ///
            /// # Arguments
            /// - `self`: the borrowed `&Self` counter to encode.
            /// - `serializer`: a value of type `S` implementing `Serializer`.
            ///
            /// # Returns
            /// `Result<S::Ok, S::Error>` with the serializer's successful output.
            ///
            /// # Errors
            /// Returns the serializer's error if it cannot encode the string.
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            /// Reads decimal text through the same validation as `FromStr`.
            ///
            /// # Arguments
            /// - `deserializer`: a value of type `D` implementing
            ///   `Deserializer<'de>` for the incoming representation.
            ///
            /// # Returns
            /// `Result<Self, D::Error>` containing a validated counter.
            ///
            /// # Errors
            /// Returns a decoding error for non-string data, noncanonical decimal
            /// text, or a number outside this type's bounds.
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

counter!(Counter, 0);
counter!(PositiveCounter, 1);

impl Counter {
    /// The zero counter, used where no positive sequence has yet been recorded.
    pub const ZERO: Self = Self(0);
}

impl PositiveCounter {
    /// The first valid positive sequence or revision value.
    pub const ONE: Self = Self(1);
}

impl From<PositiveCounter> for Counter {
    /// Widens a positive counter into the counter type that also permits zero.
    ///
    /// # Arguments
    /// - `value`: an owned `PositiveCounter` already checked against both bounds.
    ///
    /// # Returns
    /// A `Counter` with exactly the same `u64` value; this conversion cannot fail.
    fn from(value: PositiveCounter) -> Self {
        Self(value.0)
    }
}
