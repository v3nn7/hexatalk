//! Drop-in mirror of the `Value` / `FunctionResult` types from the `convex`
//! crate (0.10), so the existing parsers (`src/net/convex_parse.rs`) and
//! call sites keep compiling after the backend moved to our own REST/WS API.
//! Only the variants, conversions and helpers actually used in the tree are
//! provided, plus JSON bridging (`From<serde_json::Value>` / `to_json`) used
//! by the REST dispatch layer to build request bodies and to rebuild
//! responses into the camelCase shapes the parsers expect.

use std::collections::BTreeMap;

/// A value that can be passed as an argument or returned from a backend
/// function. Mirrors `convex::Value` variant-for-variant.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub enum Value {
    Null,
    Int64(i64),
    Float64(f64),
    Boolean(bool),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

/// The outcome of a function run. Mirrors `convex::FunctionResult` — with
/// `ConvexError` carrying the error payload as a `Value` (the legacy call
/// sites only ever `format!("{err:?}")` it).
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub enum FunctionResult {
    Value(Value),
    ErrorMessage(String),
    ConvexError(Value),
}

impl Value {
    /// Plain-JSON rendering used when building REST request bodies:
    /// integers stay integers, floats floats, bytes become an array of
    /// byte values (REST args never carry binary in this codebase).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Int64(n) => serde_json::json!(n),
            Value::Float64(n) => serde_json::Number::from_f64(*n)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Boolean(b) => serde_json::Value::Bool(*b),
            Value::String(s) => serde_json::Value::String(s.clone()),
            Value::Bytes(b) => {
                serde_json::Value::Array(b.iter().map(|x| serde_json::json!(x)).collect())
            }
            Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(Value::to_json).collect())
            }
            Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect(),
            ),
        }
    }
}

impl From<serde_json::Value> for Value {
    /// Whole JSON numbers map to `Int64`, fractional ones to `Float64`
    /// (same convention the Convex client used for its wire values).
    fn from(json: serde_json::Value) -> Value {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Boolean(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int64(i)
                } else {
                    Value::Float64(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Value::String(s),
            serde_json::Value::Array(items) => {
                Value::Array(items.into_iter().map(Value::from).collect())
            }
            serde_json::Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, Value::from(v)))
                    .collect::<BTreeMap<_, _>>(),
            ),
        }
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map(Into::into).unwrap_or(Value::Null)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Value {
        Value::Int64(v)
    }
}

impl From<usize> for Value {
    fn from(v: usize) -> Value {
        Value::Int64(v as i64)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Value {
        Value::Float64(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Boolean(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::String(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::String(v)
    }
}

impl From<&String> for Value {
    fn from(v: &String) -> Value {
        Value::String(v.clone())
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Value {
        Value::Bytes(v)
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Value {
        Value::Array(v)
    }
}

impl From<BTreeMap<String, Value>> for Value {
    fn from(v: BTreeMap<String, Value>) -> Value {
        Value::Object(v)
    }
}
