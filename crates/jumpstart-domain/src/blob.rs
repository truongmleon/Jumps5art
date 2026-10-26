//! Content identities for exact file bytes using tagged BLAKE3 digests.
//! Provides hashing, digest access, validated text parsing, and Serde encoding.
//!
//! Names and locations can change while file contents stay the same.
//! A byte-based identity supports content verification and reuse without tying
//! stored bytes to a particular library item or owning app.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::DomainError;

/// A 256-bit BLAKE3 digest of exact original bytes, tagged on disk and on the wire.
///
/// The private `[u8; 32]` stores the digest; text uses `b3:` followed by exactly
/// 64 lowercase hexadecimal digits. A digest alone does not prove that stored
/// content has been verified against it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlobId([u8; 32]);

impl BlobId {
    /// Wraps an already computed digest without hashing content again.
    ///
    /// # Arguments
    /// - `bytes`: the digest as an owned `[u8; 32]` array.
    ///
    /// # Returns
    /// A `BlobId` containing those exact digest bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Computes the unkeyed 256-bit BLAKE3 identity of a byte sequence.
    ///
    /// # Arguments
    /// - `bytes`: a borrowed `&[u8]` containing the exact content to hash.
    ///
    /// # Returns
    /// A `BlobId` for the supplied bytes, including the empty sequence.
    pub fn hash_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Borrows the digest for storage or comparison without formatting it.
    ///
    /// # Arguments
    /// - `self`: the borrowed `&BlobId` whose digest is needed.
    ///
    /// # Returns
    /// A read-only `&[u8; 32]` tied to the lifetime of `self`.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for BlobId {
    /// Writes the algorithm tag and lowercase hexadecimal digest.
    ///
    /// # Arguments
    /// - `self`: the borrowed `&BlobId` to display.
    /// - `formatter`: a mutable `fmt::Formatter` receiving the text.
    ///
    /// # Returns
    /// `fmt::Result`, with `Ok(())` after every character is written.
    ///
    /// # Errors
    /// Returns the formatter's error if writing the tag or digest fails.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("b3:")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for BlobId {
    type Err = DomainError;

    /// Parses the exact version 1 tagged digest representation.
    ///
    /// # Arguments
    /// - `value`: borrowed `&str` expected to contain `b3:` and 64 lowercase
    ///   hexadecimal digits, without surrounding whitespace.
    ///
    /// # Returns
    /// `Result<BlobId, DomainError>` containing the decoded digest on success.
    ///
    /// # Errors
    /// Returns `DomainError::InvalidBlobId` for a wrong tag, length, or digit.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let hex = value
            .strip_prefix("b3:")
            .filter(|hex| hex.len() == 64)
            .ok_or(DomainError::InvalidBlobId)?;
        let mut bytes = [0; 32];
        for (destination, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            let digit = |byte| match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err(DomainError::InvalidBlobId),
            };
            *destination = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for BlobId {
    /// Serializes this identity as its tagged digest string.
    ///
    /// # Arguments
    /// - `self`: the borrowed `&BlobId` to encode.
    /// - `serializer`: a value of type `S` implementing Serde's `Serializer`.
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

impl<'de> Deserialize<'de> for BlobId {
    /// Reads a string and applies the same checks as `BlobId::from_str`.
    ///
    /// # Arguments
    /// - `deserializer`: a value of type `D` implementing
    ///   `Deserializer<'de>` for the incoming representation.
    ///
    /// # Returns
    /// `Result<BlobId, D::Error>` containing a validated digest identity.
    ///
    /// # Errors
    /// Returns the deserializer's error for a non-string input or invalid ID.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}
