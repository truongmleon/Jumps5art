//! A bounded, no-output traversal before JSON's non-finite-to-null conversion.
use std::cell::Cell;
use std::fmt;

use serde::ser::{self, Serialize};

use crate::{MAX_FRAME_BYTES, MAX_JSON_DEPTH};

#[derive(Clone, Copy)]
pub(crate) struct FiniteCheck<'a> {
    depth: usize,
    remaining: &'a Cell<usize>,
}

#[derive(Debug)]
pub(crate) enum PreflightError {
    NonFinite,
    DepthExceeded,
    TooLarge,
    Serialization(String),
}

impl fmt::Display for PreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite => formatter.write_str("non-finite floating-point number"),
            Self::DepthExceeded => formatter.write_str("JSON nesting exceeds 32 containers"),
            Self::TooLarge => formatter.write_str("JSON body exceeds maximum size"),
            Self::Serialization(message) => formatter.write_str(message),
        }
    }
}
impl std::error::Error for PreflightError {}
impl ser::Error for PreflightError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::Serialization(message.to_string())
    }
}

impl<'a> FiniteCheck<'a> {
    pub(crate) fn new(remaining: &'a Cell<usize>) -> Self {
        Self {
            depth: 0,
            remaining,
        }
    }

    fn spend(self, minimum_bytes: usize) -> Result<(), PreflightError> {
        let Some(remaining) = self.remaining.get().checked_sub(minimum_bytes) else {
            return Err(PreflightError::TooLarge);
        };
        self.remaining.set(remaining);
        Ok(())
    }

    fn container(self) -> Result<Self, PreflightError> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(PreflightError::DepthExceeded);
        }
        // Every rendered container occupies at least two bytes.
        self.spend(2)?;
        Ok(Self {
            depth: self.depth + 1,
            ..self
        })
    }
}

pub(crate) fn check<T: Serialize + ?Sized>(payload: &T) -> Result<(), PreflightError> {
    let remaining = Cell::new(MAX_FRAME_BYTES);
    payload.serialize(FiniteCheck::new(&remaining))
}

macro_rules! scalar {
    ($($name:ident($type:ty)),+ $(,)?) => {$ (
        fn $name(self, _value: $type) -> Result<(), PreflightError> { self.spend(1) }
    )+};
}

impl<'a> ser::Serializer for FiniteCheck<'a> {
    type Ok = ();
    type Error = PreflightError;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    scalar! {
        serialize_bool(bool), serialize_i8(i8), serialize_i16(i16),
        serialize_i32(i32), serialize_i64(i64), serialize_i128(i128),
        serialize_u8(u8), serialize_u16(u16), serialize_u32(u32),
        serialize_u64(u64), serialize_u128(u128), serialize_char(char)
    }

    fn serialize_str(self, value: &str) -> Result<(), PreflightError> {
        self.spend(value.len().saturating_add(2))
    }
    fn serialize_bytes(self, value: &[u8]) -> Result<(), PreflightError> {
        self.spend(value.len().saturating_add(2))
    }
    fn serialize_f32(self, value: f32) -> Result<(), PreflightError> {
        if value.is_finite() {
            self.spend(1)
        } else {
            Err(PreflightError::NonFinite)
        }
    }
    fn serialize_f64(self, value: f64) -> Result<(), PreflightError> {
        if value.is_finite() {
            self.spend(1)
        } else {
            Err(PreflightError::NonFinite)
        }
    }
    fn serialize_none(self) -> Result<(), PreflightError> {
        self.spend(4)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), PreflightError> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), PreflightError> {
        self.spend(4)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), PreflightError> {
        self.spend(4)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), PreflightError> {
        self.spend(variant.len().saturating_add(2))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), PreflightError> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), PreflightError> {
        let inner = self.container()?;
        inner.spend(variant.len().saturating_add(2))?;
        value.serialize(inner)
    }
    fn serialize_seq(self, _length: Option<usize>) -> Result<Self, PreflightError> {
        self.container()
    }
    fn serialize_tuple(self, _length: usize) -> Result<Self, PreflightError> {
        self.container()
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _length: usize,
    ) -> Result<Self, PreflightError> {
        self.container()
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _length: usize,
    ) -> Result<Self, PreflightError> {
        let inner = self.container()?;
        inner.spend(variant.len().saturating_add(2))?;
        inner.container()
    }
    fn serialize_map(self, _length: Option<usize>) -> Result<Self, PreflightError> {
        self.container()
    }
    fn serialize_struct(self, _name: &'static str, _length: usize) -> Result<Self, PreflightError> {
        self.container()
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _length: usize,
    ) -> Result<Self, PreflightError> {
        let inner = self.container()?;
        inner.spend(variant.len().saturating_add(2))?;
        inner.container()
    }
}

macro_rules! sequence {
    ($trait:ident, $method:ident) => {
        impl ser::$trait for FiniteCheck<'_> {
            type Ok = ();
            type Error = PreflightError;
            fn $method<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), PreflightError> {
                value.serialize(*self)
            }
            fn end(self) -> Result<(), PreflightError> {
                Ok(())
            }
        }
    };
}
sequence!(SerializeSeq, serialize_element);
sequence!(SerializeTuple, serialize_element);
sequence!(SerializeTupleStruct, serialize_field);
sequence!(SerializeTupleVariant, serialize_field);

impl ser::SerializeMap for FiniteCheck<'_> {
    type Ok = ();
    type Error = PreflightError;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), PreflightError> {
        key.serialize(*self)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), PreflightError> {
        value.serialize(*self)
    }
    fn end(self) -> Result<(), PreflightError> {
        Ok(())
    }
}

macro_rules! structure {
    ($trait:ident) => {
        impl ser::$trait for FiniteCheck<'_> {
            type Ok = ();
            type Error = PreflightError;
            fn serialize_field<T: Serialize + ?Sized>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> Result<(), PreflightError> {
                self.spend(key.len().saturating_add(2))?;
                value.serialize(*self)
            }
            fn end(self) -> Result<(), PreflightError> {
                Ok(())
            }
        }
    };
}
structure!(SerializeStruct);
structure!(SerializeStructVariant);
