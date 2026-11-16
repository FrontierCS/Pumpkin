//! How a grant is written: its name, and one key, its function's kind, that holds the function,
//! as `{"name": "train", "job": {…}}`.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, VariantAccess};
use serde::{Deserialize, Deserializer, Serialize};

use super::{Argument, JobFunction, Label, Name, NowFunction, Session};

/// A function that the role holding it calls by `name` while it works, or, for a session, the
/// session's functions, each called by `name` and its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Grant {
    /// Unique among the role's grants.
    pub name: Name,
    #[serde(flatten)]
    pub function: Function,
}

/// What a grant's function is, by its kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Function {
    Job(JobFunction),
    Session(Session),
    /// A body with no sandbox, on the run's state, answered in the call's reply.
    Now(NowFunction),
}

/// A `def` of a file of the workflow's, which a host calls by its file and name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub file: Label,
    pub def: Name,
    /// The parameters that the caller gives, by name: the positional ones after the payload.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub arguments: BTreeMap<Name, Argument>,
}

impl<'de> Deserialize<'de> for Grant {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(GrantVisitor)
    }
}

struct GrantVisitor;

impl<'de> de::Visitor<'de> for GrantVisitor {
    type Value = Grant;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a grant: its `name`, and one key, its function's kind")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Grant, A::Error> {
        let mut name = None;
        let mut function: Option<(String, Function)> = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "name" {
                if name.is_some() {
                    return Err(de::Error::duplicate_field("name"));
                }
                name = Some(map.next_value()?);
            } else if let Some((kind, _)) = &function {
                return Err(de::Error::custom(format_args!(
                    "`{key}` follows `{kind}`, the function's kind: give a grant its `name` and \
                     one function"
                )));
            } else {
                let read = Function::deserialize(Kind {
                    key: key.clone(),
                    map: &mut map,
                })?;
                function = Some((key, read));
            }
        }
        let name = name.ok_or_else(|| de::Error::missing_field("name"))?;
        let (_, function) = function.ok_or_else(|| {
            de::Error::custom("the grant gives no function: give one, under its kind's key")
        })?;
        Ok(Grant { name, function })
    }
}

// A function's kind, read from a grant's key, and the function, from that key's value.
struct Kind<'m, A> {
    key: String,
    map: &'m mut A,
}

impl<'de, A: MapAccess<'de>> Deserializer<'de> for Kind<'_, A> {
    type Error = A::Error;

    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, A::Error> {
        visitor.visit_enum(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option
        unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier
        ignored_any
    }
}

impl<'de, A: MapAccess<'de>> EnumAccess<'de> for Kind<'_, A> {
    type Error = A::Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), A::Error> {
        let key: de::value::StringDeserializer<A::Error> = self.key.clone().into_deserializer();
        Ok((seed.deserialize(key)?, self))
    }
}

impl<'de, A: MapAccess<'de>> VariantAccess<'de> for Kind<'_, A> {
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), A::Error> {
        unreachable!("each of `Function`'s kinds holds one value")
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, A::Error> {
        self.map.next_value_seed(seed)
    }

    fn tuple_variant<V: de::Visitor<'de>>(
        self,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, A::Error> {
        unreachable!("each of `Function`'s kinds holds one value")
    }

    fn struct_variant<V: de::Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, A::Error> {
        unreachable!("each of `Function`'s kinds holds one value")
    }
}
