//! `IBusText` in its D-Bus form.
//!
//! IBus serializes its objects as `(s a{sv} ...)` — the GType name, an
//! (always empty here) attachment dict, then the object's own fields. A text
//! is `(sa{sv}sv)`: the string plus an `IBusAttrList` wrapped in a variant,
//! itself `(sa{sv}av)` holding `IBusAttribute`s `(sa{sv}uuuu)`. See
//! `ibus_serializable_serialize()` and the `*_serialize` functions of
//! `ibustext.c`, `ibusattrlist.c` and `ibusattribute.c`.

use zbus::zvariant::{Array, Dict, Signature, StructureBuilder, Value};

/// `IBUS_ATTR_TYPE_UNDERLINE`
const ATTR_TYPE_UNDERLINE: u32 = 1;
/// `IBUS_ATTR_UNDERLINE_SINGLE`
const ATTR_UNDERLINE_SINGLE: u32 = 1;

fn serializable(type_name: &'static str) -> StructureBuilder<'static> {
    StructureBuilder::new()
        .append_field(Value::from(type_name))
        .append_field(Value::Dict(Dict::new(&Signature::Str, &Signature::Variant)))
}

fn variant(value: Value<'static>) -> Value<'static> {
    Value::Value(Box::new(value))
}

fn attribute(kind: u32, value: u32, start: u32, end: u32) -> Value<'static> {
    serializable("IBusAttribute")
        .add_field(kind)
        .add_field(value)
        .add_field(start)
        .add_field(end)
        .build()
        .expect("IBusAttribute structure")
        .into()
}

fn text(s: &str, attrs: Vec<Value<'static>>) -> Value<'static> {
    let mut list = Array::new(&Signature::Variant);
    for attr in attrs {
        list.append(variant(attr))
            .expect("IBusAttribute is a variant");
    }

    let attr_list = serializable("IBusAttrList")
        .append_field(Value::Array(list))
        .build()
        .expect("IBusAttrList structure");

    serializable("IBusText")
        .append_field(Value::from(s.to_owned()))
        .append_field(variant(attr_list.into()))
        .build()
        .expect("IBusText structure")
        .into()
}

/// A plain `IBusText` for commit strings.
pub fn plain(s: &str) -> Value<'static> {
    text(s, Vec::new())
}

/// An `IBusText` underlined over its whole length, as preedit is drawn.
pub fn preedit(s: &str) -> Value<'static> {
    let len = s.chars().count() as u32;
    let attrs = if len == 0 {
        Vec::new()
    } else {
        // IBus attribute ranges count characters, not bytes.
        vec![attribute(
            ATTR_TYPE_UNDERLINE,
            ATTR_UNDERLINE_SINGLE,
            0,
            len,
        )]
    };

    text(s, attrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(value: &Value<'static>) -> Vec<Value<'static>> {
        match value {
            Value::Structure(s) => s.try_clone().unwrap().into_fields(),
            Value::Value(inner) => fields(inner),
            other => panic!("not a structure: {other:?}"),
        }
    }

    #[test]
    fn text_signature_matches_ibus() {
        assert_eq!(plain("한").value_signature().to_string(), "(sa{sv}sv)");
    }

    #[test]
    fn plain_text_fields() {
        let fields = fields(&plain("한글"));

        assert_eq!(fields[0], Value::from("IBusText"));
        assert_eq!(fields[2], Value::from("한글"));

        let attr_list = self::fields(&fields[3]);
        assert_eq!(attr_list[0], Value::from("IBusAttrList"));
        match &attr_list[2] {
            Value::Array(attrs) => assert!(attrs.is_empty()),
            other => panic!("attrs is not an array: {other:?}"),
        }
    }

    #[test]
    fn preedit_is_underlined_by_chars() {
        let fields = fields(&preedit("한글"));
        let attr_list = self::fields(&fields[3]);

        let attrs = match &attr_list[2] {
            Value::Array(attrs) => attrs.inner().to_vec(),
            other => panic!("attrs is not an array: {other:?}"),
        };
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].value_signature().to_string(), "v");

        let attr = self::fields(&attrs[0]);
        assert_eq!(attr[0], Value::from("IBusAttribute"));
        // underline, single, chars [0, 2) — not the 6 bytes of "한글"
        assert_eq!(attr[2], Value::U32(ATTR_TYPE_UNDERLINE));
        assert_eq!(attr[3], Value::U32(ATTR_UNDERLINE_SINGLE));
        assert_eq!(attr[4], Value::U32(0));
        assert_eq!(attr[5], Value::U32(2));
    }

    #[test]
    fn empty_preedit_has_no_attributes() {
        let attr_list = fields(&fields(&preedit(""))[3]);
        match &attr_list[2] {
            Value::Array(attrs) => assert!(attrs.is_empty()),
            other => panic!("attrs is not an array: {other:?}"),
        }
    }
}
