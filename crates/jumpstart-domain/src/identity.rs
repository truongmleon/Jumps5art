use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::{Uuid, Variant};

use crate::DomainError;

macro_rules! identity {
    ($name:ident, $version:literal, $generate:ident) => {
        #[doc = concat!("A non-nil RFC 9562 UUIDv", stringify!($version), " identity.")]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::$generate())
            }

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

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
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
                Self::from_uuid(Uuid::parse_str(value)?)
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = DomainError;

            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                Self::from_uuid(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
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

identity!(ServerId, 4, new_v4);
identity!(SyncEpoch, 4, new_v4);
identity!(DeviceId, 4, new_v4);
identity!(ClientInstanceId, 4, new_v4);
identity!(ItemId, 7, now_v7);
identity!(VersionId, 7, now_v7);
identity!(ResourceId, 7, now_v7);
identity!(JobId, 7, now_v7);
