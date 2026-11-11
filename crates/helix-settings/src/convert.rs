//! GVariant → D-Bus value conversion.
//!
//! GVariant and D-Bus share a type system except for GVariant's maybe type
//! (`m`), which D-Bus cannot carry; values containing one convert to `None`.

use gio::glib::{Variant, VariantClass};
use zbus::zvariant::{
    Array, Dict, ObjectPath, OwnedValue, Signature, StructureBuilder, Value,
};

/// Convert a GVariant to an owned D-Bus value.
pub fn to_dbus(v: &Variant) -> Option<OwnedValue> {
    OwnedValue::try_from(convert(v)?).ok()
}

fn signature(type_str: &str) -> Option<Signature> {
    if type_str.contains('m') {
        return None;
    }
    type_str.parse().ok()
}

fn convert(v: &Variant) -> Option<Value<'static>> {
    Some(match v.classify() {
        VariantClass::Boolean => Value::from(v.get::<bool>()?),
        VariantClass::Byte => Value::from(v.get::<u8>()?),
        VariantClass::Int16 => Value::from(v.get::<i16>()?),
        VariantClass::Uint16 => Value::from(v.get::<u16>()?),
        VariantClass::Int32 => Value::from(v.get::<i32>()?),
        VariantClass::Uint32 => Value::from(v.get::<u32>()?),
        VariantClass::Int64 => Value::from(v.get::<i64>()?),
        VariantClass::Uint64 => Value::from(v.get::<u64>()?),
        VariantClass::Double => Value::from(v.get::<f64>()?),
        VariantClass::String => Value::from(v.str()?.to_owned()),
        VariantClass::ObjectPath => {
            Value::from(ObjectPath::try_from(v.str()?.to_owned()).ok()?)
        }
        VariantClass::Signature => Value::from(signature(v.str()?)?),
        VariantClass::Variant => Value::Value(Box::new(convert(&v.as_variant()?)?)),
        VariantClass::Array => convert_array(v)?,
        VariantClass::Tuple => {
            let mut builder = StructureBuilder::new();
            for i in 0..v.n_children() {
                builder = builder.append_field(convert(&v.child_value(i))?);
            }
            if v.n_children() == 0 {
                return None;
            }
            Value::from(builder.build().ok()?)
        }
        // Maybe types, and dict entries outside an array, have no D-Bus form.
        _ => return None,
    })
}

fn convert_array(v: &Variant) -> Option<Value<'static>> {
    let type_str = v.type_().as_str().to_owned();
    let element = type_str.strip_prefix('a')?;
    if let Some(inner) = element.strip_prefix('{').and_then(|e| e.strip_suffix('}')) {
        let (k, val) = split_dict_entry(inner)?;
        let mut dict = Dict::new(&signature(k)?, &signature(val)?);
        for i in 0..v.n_children() {
            let entry = v.child_value(i);
            dict.append(convert(&entry.child_value(0))?, convert(&entry.child_value(1))?)
                .ok()?;
        }
        return Some(Value::from(dict));
    }
    let mut array = Array::new(&signature(element)?);
    for i in 0..v.n_children() {
        array.append(convert(&v.child_value(i))?).ok()?;
    }
    Some(Value::from(array))
}

/// Split a dict entry body like `sv` or `s(ii)` into key and value types.
/// Dict keys are always basic (single-character) types.
fn split_dict_entry(inner: &str) -> Option<(&str, &str)> {
    let mut chars = inner.char_indices();
    chars.next()?;
    let (idx, _) = chars.next()?;
    Some((&inner[..idx], &inner[idx..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::glib::{variant::ToVariant, VariantDict};
    use std::collections::HashMap;

    fn sig(v: &OwnedValue) -> String {
        v.value_signature().to_string()
    }

    #[test]
    fn basic_and_container_values() {
        assert!(bool::try_from(&*to_dbus(&true.to_variant()).unwrap()).unwrap());
        assert_eq!(i32::try_from(&*to_dbus(&42i32.to_variant()).unwrap()).unwrap(), 42);
        let s = to_dbus(&"prefer-dark".to_variant()).unwrap();
        assert_eq!(<&str>::try_from(&*s).unwrap(), "prefer-dark");

        let list = to_dbus(&vec!["a".to_owned(), "b".to_owned()].to_variant()).unwrap();
        assert_eq!(sig(&list), "as");
        let back: Vec<String> = Vec::try_from(list).unwrap();
        assert_eq!(back, vec!["a", "b"]);

        let tuple = to_dbus(&(0.25f64, 0.5f64, 0.75f64).to_variant()).unwrap();
        assert_eq!(sig(&tuple), "(ddd)");

        let pairs = to_dbus(&vec![("xkb".to_owned(), "us".to_owned())].to_variant()).unwrap();
        assert_eq!(sig(&pairs), "a(ss)");

        let map: HashMap<String, u32> = HashMap::from([("k".to_owned(), 7)]);
        let dict = to_dbus(&map.to_variant()).unwrap();
        assert_eq!(sig(&dict), "a{su}");

        let vardict = VariantDict::new(None);
        vardict.insert("x", 1i32);
        let asv = to_dbus(&vardict.end()).unwrap();
        assert_eq!(sig(&asv), "a{sv}");
    }

    #[test]
    fn maybe_types_are_not_representable() {
        assert!(to_dbus(&Some("x").to_variant()).is_none());
        assert!(to_dbus(&vec![Some(1i32)].to_variant()).is_none());
    }
}
