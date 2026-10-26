use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::{Uuid, Variant};

use crate::DomainError;

/// Generates distinct UUID identity types with a shared validation boundary.
///
/// The macro takes the type name, required UUID version literal, and a UUID
/// generator name; callers select those values in the invocations below.
macro_rules! identity {
    ($name:ident, $version:literal, $generate:ident) => {
        #[doc = concat!("A non-nil RFC 9562 UUIDv", stringify!($version), " identity.")]
        ///
        /// The private `Uuid` field keeps external callers on the checked
        /// constructor and parsing paths. Each invocation creates a distinct type.
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a fresh identity with this type's required UUID version.
            ///
            /// # Returns
            /// A new `Self` created by the UUID generator selected for this type.
            pub fn new() -> Self {
                Self(Uuid::$generate())
            }

            /// Checks a raw UUID before assigning it this domain identity type.
            ///
            /// # Arguments
            /// - `value`: an owned `Uuid` whose version and variant are untrusted.
            ///
            /// # Returns
            /// `Result<Self, DomainError>` containing the unchanged UUID on success.
            ///
            /// # Errors
            /// Returns `DomainError::InvalidUuid` for a nil UUID, a non-RFC variant,
            /// or a version different from the one selected for this type.
            pub fn from_uuid(value: Uuid) -> Result<Self, DomainError> {
                if value.is_nil()
                    || value.get_variant() != Variant::RFC4122
                    || value.get_version_num() != $version
                {
                    return Err(DomainError::InvalidUuid {
                        kind: stringify!($name),
                        expected_version: $version,
                        value,
                    });
                }
                Ok(Self(value))
            }

            /// Borrows the validated UUID without exposing mutable access.
            ///
            /// # Arguments
            /// - `self`: the borrowed `&Self` identity whose UUID is needed.
            ///
            /// # Returns
            /// A read-only `&Uuid` tied to the lifetime of this identity.
            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            /// Generates a fresh identity when a caller requests a default value.
            ///
            /// # Returns
            /// A new `Self` through the same generator as `Self::new()`; it is
            /// never a nil or fixed placeholder UUID.
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            /// Writes the underlying UUID in its standard display representation.
            ///
            /// # Arguments
            /// - `self`: the borrowed `&Self` identity to display.
            /// - `formatter`: a mutable `fmt::Formatter` receiving the UUID text.
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

            /// Parses UUID text and then applies this identity type's checks.
            ///
            /// # Arguments
            /// - `value`: borrowed `&str` accepted as UUID text by the UUID library.
            ///
            /// # Returns
            /// `Result<Self, DomainError>` containing a validated identity.
            ///
            /// # Errors
            /// Returns `InvalidUuidString` for malformed text, or `InvalidUuid`
            /// when the parsed UUID violates this type's version or variant rules.
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_uuid(Uuid::parse_str(value)?)
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = DomainError;

            /// Offers standard fallible conversion from a raw UUID.
            ///
            /// # Arguments
            /// - `value`: an owned `Uuid` to check through `Self::from_uuid`.
            ///
            /// # Returns
            /// `Result<Self, DomainError>` containing the same UUID on success.
            ///
            /// # Errors
            /// Returns `DomainError::InvalidUuid` if the identity checks fail.
            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                Self::from_uuid(value)
            }
        }

        impl From<$name> for Uuid {
            /// Extracts an identity's raw UUID without changing its bits.
            ///
            /// # Arguments
            /// - `value`: the generated identity type by value. Its `Copy`
            ///   implementation permits continued use of the caller's original.
            ///
            /// # Returns
            /// The stored `Uuid`; converting an already validated identity cannot fail.
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl Serialize for $name {
            /// Serializes the identity as its UUID display string.
            ///
            /// # Arguments
            /// - `self`: the borrowed `&Self` identity to encode.
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
            /// Reads UUID text through the same checked parsing path as `FromStr`.
            ///
            /// # Arguments
            /// - `deserializer`: a value of type `D` implementing
            ///   `Deserializer<'de>` for the incoming representation.
            ///
            /// # Returns
            /// `Result<Self, D::Error>` containing a validated identity.
            ///
            /// # Errors
            /// Returns a decoding error for non-string data, malformed UUID text,
            /// or a UUID that violates this identity type's rules.
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

identity!(ServerId, 4, new_v4);
identity!(SyncEpoch, 4, new_v4);
identity!(DeviceId, 4, new_v4);
identity!(ClientInstanceId, 4, new_v4);
identity!(ItemId, 7, now_v7);
identity!(VersionId, 7, now_v7);
identity!(ResourceId, 7, now_v7);
identity!(JobId, 7, now_v7);
