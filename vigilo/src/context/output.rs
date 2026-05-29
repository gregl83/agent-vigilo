use std::{
    collections::BTreeMap,
    io::{
        BufWriter,
        Stdout,
        Write,
        stdout,
    },
    sync::Mutex,
};

use clap::ValueEnum;
use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::{
    debug,
    error,
};

/// Structured stdout encoding for command payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// JSON structured output for exact machine parsing.
    Json,
    /// Compact structured output for AI agents and LLM tool calls.
    Toon,
}

fn string_to_toon(value: &str) -> anyhow::Result<String> {
    let needs_quotes = value.is_empty()
        || value.trim() != value
        || value.chars().any(|ch| {
            ch.is_control() || matches!(ch, '"' | ',' | ':' | '[' | ']' | '{' | '}' | '\\')
        });

    if needs_quotes {
        Ok(serde_json::to_string(value)?)
    } else {
        Ok(value.to_string())
    }
}

fn scalar_to_toon(value: &Value) -> anyhow::Result<String> {
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) => string_to_toon(value),
        Value::Array(_) | Value::Object(_) => Ok(serde_json::to_string(value)?),
    }
}

fn key_to_toon(key: &str) -> anyhow::Result<String> {
    string_to_toon(key)
}

fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn sorted_object_entries(map: &serde_json::Map<String, Value>) -> BTreeMap<&String, &Value> {
    map.iter().collect()
}

fn write_toon_value(
    out: &mut String,
    key: Option<&str>,
    value: &Value,
    indent: usize,
) -> anyhow::Result<()> {
    let prefix = "  ".repeat(indent);
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str(&prefix);
                if let Some(key) = key {
                    out.push_str(&key_to_toon(key)?);
                    out.push_str(": ");
                }
                out.push_str("{}");
                out.push('\n');
                return Ok(());
            }

            if let Some(key) = key {
                out.push_str(&prefix);
                out.push_str(&key_to_toon(key)?);
                out.push(':');
                out.push('\n');
            }

            for (child_key, child_value) in sorted_object_entries(map) {
                write_toon_value(
                    out,
                    Some(child_key),
                    child_value,
                    indent + usize::from(key.is_some()),
                )?;
            }
        }
        Value::Array(items) => {
            if items.iter().all(is_scalar) {
                out.push_str(&prefix);
                if let Some(key) = key {
                    out.push_str(&key_to_toon(key)?);
                }
                out.push('[');
                out.push_str(&items.len().to_string());
                out.push_str("]:");
                let values = items
                    .iter()
                    .map(scalar_to_toon)
                    .collect::<anyhow::Result<Vec<_>>>()?;
                if !values.is_empty() {
                    out.push(' ');
                    out.push_str(&values.join(","));
                }
                out.push('\n');
            } else {
                out.push_str(&prefix);
                if let Some(key) = key {
                    out.push_str(&key_to_toon(key)?);
                    out.push('[');
                    out.push_str(&items.len().to_string());
                    out.push_str("]:");
                    out.push('\n');
                } else {
                    out.push('[');
                    out.push_str(&items.len().to_string());
                    out.push_str("]:");
                    out.push('\n');
                }

                for (idx, item) in items.iter().enumerate() {
                    let item_key = format!("- {}", idx);
                    write_toon_value(out, Some(&item_key), item, indent + 1)?;
                }
            }
        }
        _ => {
            out.push_str(&prefix);
            if let Some(key) = key {
                out.push_str(&key_to_toon(key)?);
                out.push_str(": ");
            }
            out.push_str(&scalar_to_toon(value)?);
            out.push('\n');
        }
    }

    Ok(())
}

fn to_toon(value: &Value) -> anyhow::Result<String> {
    let mut out = String::new();
    write_toon_value(&mut out, None, value, 0)?;
    if out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

fn render_value(value: &Value, format: OutputFormat) -> anyhow::Result<String> {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(value).map_err(Into::into),
        OutputFormat::Toon => to_toon(value),
    }
}

pub struct Buffer {
    buffer: Mutex<BufWriter<Stdout>>,
    format: OutputFormat,
}

impl Buffer {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            buffer: Mutex::new(BufWriter::new(stdout())),
            format,
        }
    }

    /// Writes one line to the stdout buffer.
    pub fn write_line(&self, msg: impl AsRef<str>) -> anyhow::Result<()> {
        let mut guard = self
            .buffer
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to acquire buffer lock"))?;
        writeln!(guard, "{}", msg.as_ref())?;
        Ok(())
    }

    /// Writes structured payload using the configured output format.
    pub fn write_value(&self, value: &Value) -> anyhow::Result<()> {
        let rendered = render_value(value, self.format)?;
        self.write_line(rendered)
    }

    /// Flushes buffered stdout.
    pub fn flush(&self) -> anyhow::Result<()> {
        let mut guard = self
            .buffer
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to acquire buffer lock"))?;
        guard.flush()?;
        Ok(())
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if self.flush().is_err() {
            error!("error flushing output buffer during drop");
        } else {
            debug!("output buffer flushed successfully on drop");
        }
    }
}

pub struct Context {
    pub(crate) cell: OnceCell<Buffer>,
    pub(crate) format: OutputFormat,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&Buffer> {
        self.cell
            .get_or_try_init(|| async {
                debug!("initializing out write buffer");
                Ok(Buffer::new(self.format))
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        OutputFormat,
        render_value,
        to_toon,
    };

    #[test]
    fn json_renders_exact_machine_payload() {
        let payload = json!({
            "data": {"run_id": "run-1", "score": 0.95},
            "meta": {"summary_only": true}
        });

        assert_eq!(
            render_value(&payload, OutputFormat::Json).unwrap(),
            "{\n  \"data\": {\n    \"run_id\": \"run-1\",\n    \"score\": 0.95\n  },\n  \"meta\": {\n    \"summary_only\": true\n  }\n}"
        );
    }

    #[test]
    fn toon_renders_stable_nested_objects() {
        let payload = json!({
            "meta": {"summary_only": true},
            "data": {"run_id": "run-1", "score": 0.95}
        });

        assert_eq!(
            to_toon(&payload).unwrap(),
            "data:\n  run_id: run-1\n  score: 0.95\nmeta:\n  summary_only: true"
        );
    }

    #[test]
    fn toon_renders_scalar_arrays_with_count() {
        let payload = json!({"statuses": ["passed", "failed"]});

        assert_eq!(to_toon(&payload).unwrap(), "statuses[2]: passed,failed");
    }

    #[test]
    fn toon_renders_empty_collections_explicitly() {
        let payload = json!({
            "empty_object": {},
            "empty_array": []
        });

        assert_eq!(
            to_toon(&payload).unwrap(),
            "empty_array[0]:\nempty_object: {}"
        );
    }

    #[test]
    fn toon_renders_root_array_with_count() {
        let payload = json!([
            {"id": 1}
        ]);

        assert_eq!(to_toon(&payload).unwrap(), "[1]:\n  - 0:\n    id: 1");
    }

    #[test]
    fn toon_quotes_ambiguous_strings() {
        let payload = json!({
            "message": "line one\nline two",
            "target": "postgres://localhost,primary"
        });

        assert_eq!(
            to_toon(&payload).unwrap(),
            "message: \"line one\\nline two\"\ntarget: \"postgres://localhost,primary\""
        );
    }

    #[test]
    fn toon_quotes_ambiguous_keys() {
        let payload = json!({
            "data:source": "run",
            "meta": {
                "needs,quote": true
            }
        });

        assert_eq!(
            render_value(&payload, OutputFormat::Toon).unwrap(),
            "\"data:source\": run\nmeta:\n  \"needs,quote\": true"
        );
    }

    #[test]
    fn output_format_defaults_are_distinct() {
        assert_ne!(OutputFormat::Json, OutputFormat::Toon);
    }
}
