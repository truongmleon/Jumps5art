//! Preflight validation of outgoing values before the JSON encoder runs.
//! Rejects non-finite numbers and bounds nesting and traversal with a shared budget;
//! the codec separately checks the exact encoded size.

use std::cell::Cell;
use std::fmt;

use serde::ser::{self, Serialize};

use crate::{MAX_FRAME_BYTES, MAX_JSON_DEPTH};

/// A serializer that checks values without producing output bytes.
///
/// `depth` counts enclosing containers. `remaining: &'a Cell<usize>` shares a
/// minimum-byte allowance across copied checkers. Exact output size is checked
/// separately by the bounded JSON writer.
#[derive(Clone, Copy)]
pub(crate) struct FiniteCheck<'a> {
    depth: usize,
    remaining: &'a Cell<usize>,
}

/// Reasons preliminary serialization validation must stop.
///
/// Variants distinguish non-finite numbers, excessive depth, exhausted minimum-byte
/// budget, and an owned diagnostic from a custom `Serialize` implementation.
#[derive(Debug)]
pub(crate) enum PreflightError {
    NonFinite,
    DepthExceeded,
    TooLarge,
    Serialization(String),
}

impl fmt::Display for PreflightError {
    /// Write a readable description of the validation failure.
    ///
    /// `self: &PreflightError` supplies the variant/message;
    /// `formatter: &mut fmt::Formatter<'_>` receives text.
    /// Returns `fmt::Result`, propagating any formatting failure.
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
    /// Retain a custom serializer diagnostic as an owned error.
    ///
    /// `message: T` is any value implementing `fmt::Display`.
    /// Returns `PreflightError::Serialization` containing the formatted message.
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::Serialization(message.to_string())
    }
}

impl<'a> FiniteCheck<'a> {
    /// Start at root depth while borrowing the shared byte allowance.
    ///
    /// `remaining: &'a Cell<usize>` is the allowance shared by all child checkers.
    /// Returns `FiniteCheck<'a>` at depth zero, without resetting the allowance.
    pub(crate) fn new(remaining: &'a Cell<usize>) -> Self {
        Self {
            depth: 0,
            remaining,
        }
    }

    /// Subtract a minimum output size without integer underflow.
    ///
    /// `self: FiniteCheck<'a>` identifies the budget; `minimum_bytes: usize`
    /// is the charge for the current value.
    /// Returns `Result<(), PreflightError>` with `Ok(())` after subtraction, or
    /// `TooLarge` without changing the allowance when it cannot cover the charge.
    fn spend(self, minimum_bytes: usize) -> Result<(), PreflightError> {
        let Some(remaining) = self.remaining.get().checked_sub(minimum_bytes) else {
            return Err(PreflightError::TooLarge);
        };
        self.remaining.set(remaining);
        Ok(())
    }

    /// Open one nested container and charge its two delimiter bytes.
    ///
    /// `self: FiniteCheck<'a>` supplies the current depth and shared allowance.
    /// Returns `Result<FiniteCheck<'a>, PreflightError>` at depth plus one, or
    /// `DepthExceeded`/`TooLarge` if opening the container would exceed a limit.
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

/// Traverse a payload before JSON could replace non-finite numbers with null.
///
/// `payload: &T` borrows a possibly unsized value implementing `Serialize`.
/// Returns `Result<(), PreflightError>` with `Ok(())` on success; emits no bytes.
/// Propagates non-finite, nesting, minimum-byte-budget, or custom serializer errors.
pub(crate) fn check<T: Serialize + ?Sized>(payload: &T) -> Result<(), PreflightError> {
    let remaining = Cell::new(MAX_FRAME_BYTES);
    payload.serialize(FiniteCheck::new(&remaining))
}

macro_rules! scalar {
    ($($name:ident($type:ty)),+ $(,)?) => {$ (
        /// Check a scalar by charging at least one output byte.
        ///
        /// `self: FiniteCheck<'a>` shares the budget; `_value: $type` is the
        /// boolean, integer, or character type chosen by the macro invocation.
        /// Returns `Result<(), PreflightError>` with `Ok(())` or `TooLarge`.
        /// These scalar types need no floating-point finite-number check.
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

    /// Charge a string's UTF-8 text and quotes before JSON escaping.
    ///
    /// `self: FiniteCheck<'a>` shares the allowance; `value: &str` borrows text.
    /// Returns `Result<(), PreflightError>` with `Ok(())` or `TooLarge`.
    /// The bounded writer later counts any additional JSON escape bytes.
    fn serialize_str(self, value: &str) -> Result<(), PreflightError> {
        self.spend(value.len().saturating_add(2))
    }
    /// Charge a minimum size for bytes represented as a JSON array.
    ///
    /// `self: FiniteCheck<'a>` shares the allowance; `value: &[u8]` borrows bytes.
    /// Returns `Result<(), PreflightError>` with `Ok(())` or `TooLarge`.
    /// The actual JSON writer separately counts digits and commas.
    fn serialize_bytes(self, value: &[u8]) -> Result<(), PreflightError> {
        self.spend(value.len().saturating_add(2))
    }
    /// Reject non-finite single-precision numbers before charging their output.
    ///
    /// `self: FiniteCheck<'a>` shares the allowance; `value: f32` is the number.
    /// Returns `Result<(), PreflightError>` with `Ok(())`, `NonFinite` for NaN/infinity,
    /// or `TooLarge` when the minimum-byte charge cannot be covered.
    fn serialize_f32(self, value: f32) -> Result<(), PreflightError> {
        if value.is_finite() {
            self.spend(1)
        } else {
            Err(PreflightError::NonFinite)
        }
    }
    /// Reject non-finite double-precision numbers before charging their output.
    ///
    /// `self: FiniteCheck<'a>` shares the allowance; `value: f64` is the number.
    /// Returns `Result<(), PreflightError>` with `Ok(())`, `NonFinite` for NaN/infinity,
    /// or `TooLarge` when the minimum-byte charge cannot be covered.
    fn serialize_f64(self, value: f64) -> Result<(), PreflightError> {
        if value.is_finite() {
            self.spend(1)
        } else {
            Err(PreflightError::NonFinite)
        }
    }
    /// Charge four bytes for an absent optional value encoded as null.
    ///
    /// Consumes `self: FiniteCheck<'a>`, which shares the remaining allowance.
    /// Returns `Result<(), PreflightError>` with `Ok(())` or `TooLarge`.
    fn serialize_none(self) -> Result<(), PreflightError> {
        self.spend(4)
    }
    /// Check a present optional value without adding a JSON wrapper.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `value: &T` borrows a
    /// possibly unsized `Serialize` value.
    /// Returns `Result<(), PreflightError>` from the child check, propagating its errors.
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), PreflightError> {
        value.serialize(self)
    }
    /// Charge four bytes for unit `()` encoded as JSON null.
    ///
    /// Consumes `self: FiniteCheck<'a>`, which shares the remaining allowance.
    /// Returns `Result<(), PreflightError>` with `Ok(())` or `TooLarge`.
    fn serialize_unit(self) -> Result<(), PreflightError> {
        self.spend(4)
    }
    /// Charge a fieldless struct encoded as JSON null.
    ///
    /// `self: FiniteCheck<'a>` shares the budget; `_name: &'static str` is
    /// unused Serde type metadata.
    /// Returns `Result<(), PreflightError>` after a four-byte charge, or `TooLarge`.
    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), PreflightError> {
        self.spend(4)
    }
    /// Charge the quoted name of an enum variant without payload fields.
    ///
    /// `self: FiniteCheck<'a>` shares the budget; `_name: &'static str` and
    /// `_index: u32` are unused metadata; `variant: &'static str` is the encoded name.
    /// Returns `Result<(), PreflightError>` with `Ok(())` or `TooLarge`.
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), PreflightError> {
        self.spend(variant.len().saturating_add(2))
    }
    /// Check a one-field wrapper through its contained value.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_name: &'static str` is
    /// unused metadata; `value: &T` borrows the contained `Serialize` value.
    /// Returns `Result<(), PreflightError>` from the child, without adding a container.
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), PreflightError> {
        value.serialize(self)
    }
    /// Open an external variant object, charge its key, and check its payload.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_name: &'static str` and
    /// `_index: u32` are unused metadata; `variant: &'static str` names the key;
    /// `value: &T` borrows the `Serialize` payload.
    /// Returns `Result<(), PreflightError>`, propagating container, budget, or child errors.
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
    /// Start an array checker at the next depth with the shared allowance.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_length: Option<usize>`
    /// is the unused optional element-count hint.
    /// Returns `Result<FiniteCheck<'a>, PreflightError>` with the child checker,
    /// or a container depth/budget error.
    fn serialize_seq(self, _length: Option<usize>) -> Result<Self, PreflightError> {
        self.container()
    }
    /// Start the array used to encode a tuple.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_length: usize` is the
    /// unused known field-count hint.
    /// Returns `Result<FiniteCheck<'a>, PreflightError>` at the next depth,
    /// or a container depth/budget error.
    fn serialize_tuple(self, _length: usize) -> Result<Self, PreflightError> {
        self.container()
    }
    /// Start the array used to encode a named tuple struct.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_name: &'static str` and
    /// `_length: usize` are unused type/count metadata.
    /// Returns `Result<FiniteCheck<'a>, PreflightError>` at the next depth,
    /// or a container depth/budget error.
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _length: usize,
    ) -> Result<Self, PreflightError> {
        self.container()
    }
    /// Open the external variant object and its tuple-payload array.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_name: &'static str`,
    /// `_index: u32`, and `_length: usize` are unused metadata;
    /// `variant: &'static str` names the outer key.
    /// Returns `Result<FiniteCheck<'a>, PreflightError>` inside both containers,
    /// or a depth/budget error while opening them or charging the key.
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
    /// Start an object checker for separately supplied keys and values.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_length: Option<usize>`
    /// is the unused optional entry-count hint.
    /// Returns `Result<FiniteCheck<'a>, PreflightError>` at the next depth,
    /// or a container depth/budget error.
    fn serialize_map(self, _length: Option<usize>) -> Result<Self, PreflightError> {
        self.container()
    }
    /// Start an object checker for a struct's named fields.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_name: &'static str` and
    /// `_length: usize` are unused type/count metadata.
    /// Returns `Result<FiniteCheck<'a>, PreflightError>` at the next depth,
    /// or a container depth/budget error.
    fn serialize_struct(self, _name: &'static str, _length: usize) -> Result<Self, PreflightError> {
        self.container()
    }
    /// Open an external variant object and an object for its named fields.
    ///
    /// `self: FiniteCheck<'a>` supplies context; `_name: &'static str`,
    /// `_index: u32`, and `_length: usize` are unused metadata;
    /// `variant: &'static str` names the outer key.
    /// Returns `Result<FiniteCheck<'a>, PreflightError>` inside both objects,
    /// or a depth/budget error while opening them or charging the key.
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
            /// Check one array or tuple field with the surrounding container's context.
            ///
            /// `self: &mut FiniteCheck<'_>` shares depth/budget; `value: &T` borrows
            /// an element or tuple field implementing `Serialize`.
            /// Returns `Result<(), PreflightError>` from the child check, propagating its errors.
            fn $method<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), PreflightError> {
                value.serialize(*self)
            }
            /// Finish the current checked container without emitting output bytes.
            ///
            /// Consumes `self: FiniteCheck<'_>` for this container.
            /// Returns `Result<(), PreflightError>` with `Ok(())`; field/element errors
            /// were propagated when encountered.
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
    /// Check a map key before the real JSON serializer validates key encoding.
    ///
    /// `self: &mut FiniteCheck<'_>` shares context; `key: &T` borrows a
    /// `Serialize` key.
    /// Returns `Result<(), PreflightError>` from the child check, propagating its errors.
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), PreflightError> {
        key.serialize(*self)
    }
    /// Check a map value at its object's child depth.
    ///
    /// `self: &mut FiniteCheck<'_>` shares context; `value: &T` borrows a
    /// `Serialize` value.
    /// Returns `Result<(), PreflightError>` from the child check, propagating its errors.
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), PreflightError> {
        value.serialize(*self)
    }
    /// Finish the current checked container without emitting output bytes.
    ///
    /// Consumes `self: FiniteCheck<'_>` for this container.
    /// Returns `Result<(), PreflightError>` with `Ok(())`; field/element errors
    /// were propagated when encountered.
    fn end(self) -> Result<(), PreflightError> {
        Ok(())
    }
}

macro_rules! structure {
    ($trait:ident) => {
        impl ser::$trait for FiniteCheck<'_> {
            type Ok = ();
            type Error = PreflightError;
            /// Charge a named field's key and check its value with the shared allowance.
            ///
            /// `self: &mut FiniteCheck<'_>` supplies context; `key: &'static str`
            /// is the field name; `value: &T` borrows its `Serialize` value.
            /// Returns `Result<(), PreflightError>`, propagating a budget or child-check failure.
            fn serialize_field<T: Serialize + ?Sized>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> Result<(), PreflightError> {
                self.spend(key.len().saturating_add(2))?;
                value.serialize(*self)
            }
            /// Finish the current checked container without emitting output bytes.
            ///
            /// Consumes `self: FiniteCheck<'_>` for this container.
            /// Returns `Result<(), PreflightError>` with `Ok(())`; field/element errors
            /// were propagated when encountered.
            fn end(self) -> Result<(), PreflightError> {
                Ok(())
            }
        }
    };
}
structure!(SerializeStruct);
structure!(SerializeStructVariant);
