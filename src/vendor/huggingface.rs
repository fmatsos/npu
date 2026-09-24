//! The Hugging Face Hub: its search API and the `config.json` shape a
//! `transformers`-compatible export carries.

/// The Hub's model search endpoint.
pub const HUB_API: &str = "https://huggingface.co/api/models";

/// Percent-encodes a query-string value.
#[must_use]
pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// GET `url`, with an optional bearer token, as text.
///
/// # Errors
///
/// `Error::Backend` when the request fails or the status is not 2xx.
pub fn fetch(url: &str, token: Option<&str>) -> crate::Result<String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .new_agent();
    let mut request = agent
        .get(url)
        .header("User-Agent", concat!("npu/", env!("CARGO_PKG_VERSION")));
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let failed =
        |e: &dyn std::fmt::Display| crate::Error::backend(format!("GET {url} failed: {e}"));
    request
        .call()
        .map_err(|e| failed(&e))?
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| failed(&e))
}

/// The shape a model export's `config.json` gives `npu backend tune`'s
/// budget arithmetic: the fields `transformers`-compatible configs share
/// (`num_hidden_layers`, `num_attention_heads`, `hidden_size`,
/// `max_position_embeddings`, with `head_dim` and `num_key_value_heads`
/// falling back to their usual defaults when absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub max_context: u64,
    /// fp16 KV cache bytes per token: 2 x layers x KV heads x head dim x 2.
    pub kv_per_token: u64,
    /// hidden size x layers: what the per-token static buffers scale with.
    pub width: u64,
}

/// Reads [`Shape`] out of a model export's `config.json`.
///
/// # Errors
///
/// `Error::Config` naming `source` when a required field is missing or not
/// an integer.
pub fn shape_of(config: &serde_json::Value, source: &std::path::Path) -> crate::Result<Shape> {
    let field = |name: &str| config.get(name).and_then(serde_json::Value::as_u64);
    let required = |name: &str| {
        field(name).ok_or_else(|| {
            crate::Error::Config(crate::error::ConfigError::in_file(
                source,
                None::<String>,
                format!("missing or non-integer \"{name}\""),
            ))
        })
    };
    let layers = required("num_hidden_layers")?;
    let heads = required("num_attention_heads")?;
    let hidden = required("hidden_size")?;
    let head_dim = field("head_dim").unwrap_or(hidden / heads.max(1));
    let kv_heads = field("num_key_value_heads").unwrap_or(heads);
    Ok(Shape {
        max_context: required("max_position_embeddings")?,
        kv_per_token: 2 * layers * kv_heads * head_dim * 2,
        width: hidden * layers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> &'static std::path::Path {
        std::path::Path::new("models/x.toml")
    }

    #[test]
    fn shape_defaults_head_dim_and_kv_heads() {
        let coder = serde_json::json!({"num_hidden_layers": 36, "num_key_value_heads": 2,
            "num_attention_heads": 16, "hidden_size": 2048, "max_position_embeddings": 32768});
        assert_eq!(
            shape_of(&coder, path()).map(|s| s.kv_per_token).ok(),
            Some(36_864)
        );
        let qwen = serde_json::json!({"num_hidden_layers": 36, "num_key_value_heads": 8,
            "num_attention_heads": 32, "hidden_size": 2560, "head_dim": 128,
            "max_position_embeddings": 40960});
        assert_eq!(
            shape_of(&qwen, path()).ok(),
            Some(Shape {
                max_context: 40_960,
                kv_per_token: 147_456,
                width: 92_160,
            })
        );
    }

    #[test]
    fn a_config_missing_a_field_is_a_config_error_naming_the_file() {
        let err = shape_of(&serde_json::json!({}), path()).err();
        assert!(matches!(
            &err,
            Some(crate::Error::Config(e)) if e.file.as_deref() == Some(std::path::Path::new("models/x.toml"))
        ));
    }

    #[test]
    fn a_query_string_value_is_percent_encoded() {
        assert_eq!(encode("qwen coder 7b"), "qwen%20coder%207b");
        assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
    }
}
