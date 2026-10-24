use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::DomainError;

/// SQLite's non-negative integer range is also the version 1 wire counter limit.
pub const MAX_COUNTER_VALUE: u64 = i64::MAX as u64;

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

macro_rules! counter {
    ($name:ident, $minimum:literal) => {
        #[doc = concat!("A counter in ", stringify!($minimum), "..=i64::MAX, serialized as decimal text.")]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub const MAX: Self = Self(MAX_COUNTER_VALUE);

            pub fn from_u64(value: u64) -> Result<Self, DomainError> {
                if !($minimum..=MAX_COUNTER_VALUE).contains(&value) {
                    return Err(DomainError::CounterOutOfRange {
                        value,
                        minimum: $minimum,
                    });
                }
                Ok(Self(value))
            }

            pub fn value(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = DomainError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_u64(parse_decimal(value)?)
            }
        }

        impl TryFrom<u64> for $name {
            type Error = DomainError;

            fn try_from(value: u64) -> Result<Self, Self::Error> {
                Self::from_u64(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
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
    pub const ZERO: Self = Self(0);
}

impl PositiveCounter {
    pub const ONE: Self = Self(1);
}

impl From<PositiveCounter> for Counter {
    fn from(value: PositiveCounter) -> Self {
        Self(value.0)
    }
}
