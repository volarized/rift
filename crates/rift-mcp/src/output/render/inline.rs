//! Serde writer that turns a value into short inline text, one string per field.
//!
//! Warning evidence goes through it, so every payload shape reads on one line without a
//! per-variant match. Fields keep their declaration order. Text rules:
//!
//! - a scalar, a unit variant, and a newtype are their plain text; `None`, unit, and an empty
//!   list are empty and the field is left out;
//! - a list joins its items with `, `;
//! - a package identity is `name@version (manager)`;
//! - a package context entry is `name@version` or `name requirement`, then ` (manager)`, with
//!   the availability after the manager unless it is canonical;
//! - a documentation source identity is its path or unit;
//! - any other struct is `(key value, key value)`, or just the value when it has one field.

use std::fmt::Display;

use serde::ser::{Impossible, Serialize, SerializeSeq, SerializeStruct};

use super::facts::package_text;
use crate::output::text::TextError;

/// Separates the items of a list and the fields of a nested struct.
const ITEM_SEPARATOR: &str = ", ";

/// One field of a struct: its wire name and its inline text.
pub(super) type Field = (&'static str, String);

/// The inline form of a serialized value.
pub(super) enum Inline {
    /// A scalar, a list, or a nested struct, already written.
    Text(String),
    /// A struct that was not written yet: its name and its fields in declaration order.
    Record(&'static str, Vec<Field>),
}

/// The fields of a serialized struct, each as inline text, in declaration order.
///
/// # Errors
///
/// Fails when `value` is not a struct or holds a shape inline text does not write: a map, a
/// tuple, bytes, or a variant with a payload.
pub(super) fn fields_of<T: Serialize + ?Sized>(value: &T) -> Result<Vec<Field>, TextError> {
    match value.serialize(Inliner)? {
        Inline::Record(_, fields) => Ok(fields),
        Inline::Text(_) => Err(TextError::Unsupported("value that is not a struct")),
    }
}

impl Inline {
    fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Record(name, fields) => record_text(name, &fields),
        }
    }
}

/// The text of a nested struct `name` with `fields`.
fn record_text(name: &str, fields: &[Field]) -> String {
    let get = |key: &str| {
        fields
            .iter()
            .find(|(field, _)| *field == key)
            .map_or("", |(_, text)| text.as_str())
    };
    match (name, fields) {
        ("PackageIdentity", _) => {
            format!("{}@{} ({})", get("name"), get("version"), get("manager"))
        }
        ("PackageContextEntry", _) => package_text(
            (get("manager"), get("name")),
            (get("version"), get("requirement")),
            get("availability"),
        ),
        ("DocumentationSourceIdentity", [.., (_, place)]) | (_, [(_, place)]) => place.clone(),
        _ => {
            let pairs: Vec<String> = fields
                .iter()
                .map(|(key, text)| format!("{key} {text}"))
                .collect();
            format!("({})", pairs.join(ITEM_SEPARATOR))
        }
    }
}

/// The serde serializer that writes [`Inline`] values.
struct Inliner;

/// Items of a list, collected as inline text.
struct Items(Vec<String>);

/// Fields of a struct, collected as inline text.
struct Fields {
    name: &'static str,
    fields: Vec<Field>,
}

type Refused = Impossible<Inline, TextError>;

fn text(value: impl Display) -> Inline {
    Inline::Text(value.to_string())
}

/// Generates the scalar `Serializer` methods that write their `Display` form.
macro_rules! display_methods {
    ($($method:ident($type:ty)),* $(,)?) => {
        $(fn $method(self, value: $type) -> Result<Inline, TextError> { Ok(text(value)) })*
    };
}

impl serde::Serializer for Inliner {
    type Ok = Inline;
    type Error = TextError;
    type SerializeSeq = Items;
    type SerializeTuple = Refused;
    type SerializeTupleStruct = Refused;
    type SerializeTupleVariant = Refused;
    type SerializeMap = Refused;
    type SerializeStruct = Fields;
    type SerializeStructVariant = Refused;

    display_methods! {
        serialize_bool(bool), serialize_i8(i8), serialize_i16(i16), serialize_i32(i32),
        serialize_i64(i64), serialize_u8(u8), serialize_u16(u16), serialize_u32(u32),
        serialize_u64(u64), serialize_f32(f32), serialize_f64(f64), serialize_char(char),
        serialize_str(&str),
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<Inline, TextError> {
        Err(TextError::Unsupported("bytes"))
    }

    fn serialize_none(self) -> Result<Inline, TextError> {
        Ok(text(""))
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Inline, TextError> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Inline, TextError> {
        Ok(text(""))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Inline, TextError> {
        Ok(text(""))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Inline, TextError> {
        Ok(text(variant))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Inline, TextError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Inline, TextError> {
        Err(TextError::Unsupported("variant with a payload"))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Items, TextError> {
        Ok(Items(Vec::new()))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Refused, TextError> {
        Err(TextError::Unsupported("tuple"))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Refused, TextError> {
        Err(TextError::Unsupported("tuple struct"))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Refused, TextError> {
        Err(TextError::Unsupported("tuple variant"))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Refused, TextError> {
        Err(TextError::Unsupported("map"))
    }

    fn serialize_struct(self, name: &'static str, len: usize) -> Result<Fields, TextError> {
        Ok(Fields {
            name,
            fields: Vec::with_capacity(len),
        })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Refused, TextError> {
        Err(TextError::Unsupported("struct variant"))
    }
}

impl SerializeSeq for Items {
    type Ok = Inline;
    type Error = TextError;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), TextError> {
        let item = value.serialize(Inliner)?.into_text();
        if !item.is_empty() {
            self.0.push(item);
        }
        Ok(())
    }

    fn end(self) -> Result<Inline, TextError> {
        Ok(text(self.0.join(ITEM_SEPARATOR)))
    }
}

impl SerializeStruct for Fields {
    type Ok = Inline;
    type Error = TextError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), TextError> {
        let field = value.serialize(Inliner)?.into_text();
        if !field.is_empty() {
            self.fields.push((key, field));
        }
        Ok(())
    }

    fn end(self) -> Result<Inline, TextError> {
        Ok(Inline::Record(self.name, self.fields))
    }
}
