//! Raw-preserving decoding for the session's extensible enum boundaries.

// Inspect the discriminant first: a fallback on *any* serde error would also
// accept malformed known records. Known payloads still use native serde.
macro_rules! session_wire_enum {
    ($(#[$attr:meta])* $vis:vis enum $name:ident $( [ $($serde_attr:meta),* ] )? {
        $( $(#[$variant_attr:meta])* $variant:ident $( { $($field:ident: $ty:ty),* $(,)? } )? ),* $(,)?
    }) => {
        $(#[$attr])*
        $vis enum $name {
            $( $(#[$variant_attr])* $variant $( { $($field: $ty),* } )?, )*
            /// A later writer's variant; retained verbatim, never interpreted.
            Unknown { kind: String, raw: serde_json::Value },
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    $( Self::$variant $( { $($field),* } )? =>
                        $crate::session::wire::session_wire_enum!(@serialize serializer, $name, $variant $( { $($field),* } )?), )*
                    Self::Unknown { raw, .. } => serde::Serialize::serialize(raw, serializer),
                }
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                use serde::de::Error as _;
                let raw = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
                let kind = match &raw {
                    serde_json::Value::String(kind) => kind.clone(),
                    serde_json::Value::Object(fields) if fields.len() == 1 =>
                        fields.keys().next().expect("one field").clone(),
                    _ => return Err(D::Error::custom("enum must be a string or one named object")),
                };
                if !matches!(kind.as_str(), $(stringify!($variant))|*) {
                    return Ok(Self::Unknown { kind, raw });
                }
                #[derive(serde::Deserialize)]
                $( $(#[$serde_attr])* )?
                enum Known {
                    $( $variant $( { $($field: $ty),* } )?, )*
                }
                let known: Known = serde_json::from_value(raw).map_err(D::Error::custom)?;
                Ok(match known {
                    $( Known::$variant $( { $($field),* } )? => Self::$variant $( { $($field),* } )?, )*
                })
            }
        }
    };
    (@serialize $serializer:ident, $name:ident, $variant:ident) => {
        $serializer.serialize_unit_variant(stringify!($name), 0, stringify!($variant))
    };
    (@serialize $serializer:ident, $name:ident, $variant:ident { $($field:ident),* }) => {{
        use serde::ser::SerializeStructVariant as _;
        let mut variant = $serializer.serialize_struct_variant(
            stringify!($name), 0, stringify!($variant), [$(stringify!($field)),*].len(),
        )?;
        $(variant.serialize_field(stringify!($field), $field)?;)*
        variant.end()
    }};
}

pub(crate) use session_wire_enum;
