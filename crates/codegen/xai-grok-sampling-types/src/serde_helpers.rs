use serde::{Deserialize, Deserializer};

pub fn empty_string_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.filter(|s| !s.is_empty()))
}

/// Deserialize `Option<Option<T>>`: absent (`None`) leaves, `null` (`Some(None)`) clears, a value sets.
/// Requires `#[serde(default, deserialize_with = "…")]`.
pub fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

/// Deserialize a `u64` that may arrive as any JSON number: integral values pass through,
/// floating-point values are truncated. Some OpenAI-compatible upstreams emit epoch fields
/// such as `created` as a float (`1795357515.0`); strict `u64` parsing would fail the whole
/// response with `invalid type: floating point …, expected u64`.
pub fn lenient_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => {
            if let Some(u) = number.as_u64() {
                Ok(u)
            } else if let Some(f) = number.as_f64() {
                if f.is_finite() && (0.0..=u64::MAX as f64).contains(&f) {
                    Ok(f as u64)
                } else {
                    Err(serde::de::Error::custom(format!("u64 out of range: {f}")))
                }
            } else {
                Err(serde::de::Error::custom(format!("invalid u64: {number}")))
            }
        }
        other => Err(serde::de::Error::custom(format!("invalid u64: {other}"))),
    }
}
