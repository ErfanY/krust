use std::collections::BTreeMap;

pub(crate) fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if data.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0usize;
    while i + 3 <= data.len() {
        let chunk = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(chunk & 0x3f) as usize] as char);
        i += 3;
    }

    let rem = data.len() - i;
    if rem == 1 {
        let chunk = (data[i] as u32) << 16;
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let chunk = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }

    out
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut cleaned: Vec<u8> = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if cleaned.is_empty() {
        return Ok(Vec::new());
    }
    if cleaned.len() % 4 == 1 {
        return Err("invalid base64 length".to_string());
    }
    while !cleaned.len().is_multiple_of(4) {
        cleaned.push(b'=');
    }

    let mut out = Vec::with_capacity((cleaned.len() / 4) * 3);
    let mut idx = 0usize;
    while idx < cleaned.len() {
        let c0 = cleaned[idx];
        let c1 = cleaned[idx + 1];
        let c2 = cleaned[idx + 2];
        let c3 = cleaned[idx + 3];
        idx += 4;

        let v0 = base64_value(c0).ok_or_else(|| "invalid base64 character".to_string())?;
        let v1 = base64_value(c1).ok_or_else(|| "invalid base64 character".to_string())?;
        let v2 = if c2 == b'=' {
            0
        } else {
            base64_value(c2).ok_or_else(|| "invalid base64 character".to_string())?
        };
        let v3 = if c3 == b'=' {
            0
        } else {
            base64_value(c3).ok_or_else(|| "invalid base64 character".to_string())?
        };

        let chunk = ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6) | v3 as u32;
        out.push(((chunk >> 16) & 0xff) as u8);
        if c2 != b'=' {
            out.push(((chunk >> 8) & 0xff) as u8);
        }
        if c3 != b'=' {
            out.push((chunk & 0xff) as u8);
        }
    }

    Ok(out)
}

fn is_mostly_printable_text(text: &str) -> bool {
    let mut total = 0usize;
    let mut printable = 0usize;
    for ch in text.chars() {
        total += 1;
        if ch == '\n' || ch == '\r' || ch == '\t' || !ch.is_control() {
            printable += 1;
        }
    }
    if total == 0 {
        return true;
    }
    printable * 100 / total >= 95
}

fn format_decoded_secret_value(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes)
        && is_mostly_printable_text(text)
    {
        let trimmed = text.trim();
        if !trimmed.is_empty()
            && ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']')))
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed)
            && let Ok(pretty) = serde_json::to_string_pretty(&json)
        {
            return pretty;
        }
        return text.to_string();
    }

    let preview_len = bytes.len().min(32);
    let preview = bytes
        .iter()
        .take(preview_len)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("");
    if bytes.len() > preview_len {
        format!("[binary {} bytes] hex-preview={}...", bytes.len(), preview)
    } else {
        format!("[binary {} bytes] hex-preview={}", bytes.len(), preview)
    }
}

pub(crate) fn to_pretty_yaml(value: &serde_json::Value) -> String {
    serde_yaml::to_string(value).unwrap_or_else(|_| "--- {}\n".to_string())
}

pub(crate) fn to_pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}\n".to_string())
}

fn decoded_secret_data_map(raw: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(data) = raw.get("data").and_then(serde_json::Value::as_object) {
        for (key, value) in data {
            let encoded = value.as_str().unwrap_or_default();
            let rendered = match base64_decode(encoded) {
                Ok(decoded) => format_decoded_secret_value(&decoded),
                Err(err) => format!("<decode error: {err}>"),
            };
            out.insert(key.clone(), rendered);
        }
    }
    out
}

pub(crate) fn decoded_secret_text(raw: &serde_json::Value) -> String {
    let decoded = decoded_secret_data_map(raw);
    serde_yaml::to_string(&decoded).unwrap_or_else(|_| "--- {}\n".to_string())
}

pub(crate) fn decoded_secret_json_text(raw: &serde_json::Value) -> String {
    let decoded = decoded_secret_data_map(raw);
    serde_json::to_string_pretty(&decoded).unwrap_or_else(|_| "{}\n".to_string())
}

fn yaml_value_to_secret_string(value: &serde_yaml::Value) -> Result<String, String> {
    match value {
        serde_yaml::Value::Null => Ok(String::new()),
        serde_yaml::Value::Bool(v) => Ok(v.to_string()),
        serde_yaml::Value::Number(v) => Ok(v.to_string()),
        serde_yaml::Value::String(v) => Ok(v.clone()),
        serde_yaml::Value::Sequence(_) | serde_yaml::Value::Mapping(_) => {
            serde_yaml::to_string(value)
                .map(|s| s.trim_end().to_string())
                .map_err(|err| err.to_string())
        }
        _ => Err("unsupported YAML value type".to_string()),
    }
}

pub(crate) fn parse_yaml_to_json(text: &str) -> Result<serde_json::Value, String> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(text).map_err(|err| err.to_string())?;
    serde_json::to_value(yaml).map_err(|err| err.to_string())
}

pub(crate) fn parse_json_to_json(text: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|err| err.to_string())
}

pub(crate) fn apply_decoded_secret_yaml(
    original: &serde_json::Value,
    edited: &str,
) -> Result<serde_json::Value, String> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(edited).map_err(|err| err.to_string())?;
    let mapping = yaml
        .as_mapping()
        .ok_or_else(|| "decoded secret edit must be a YAML mapping".to_string())?;

    let mut data = serde_json::Map::new();
    for (key, value) in mapping {
        let key_str = key
            .as_str()
            .ok_or_else(|| "all decoded secret keys must be strings".to_string())?;
        let raw_value = yaml_value_to_secret_string(value)?;
        data.insert(
            key_str.to_string(),
            serde_json::Value::String(base64_encode(raw_value.as_bytes())),
        );
    }

    let mut out = original.clone();
    if !out.is_object() {
        return Err("secret manifest is not an object".to_string());
    }
    if let Some(obj) = out.as_object_mut() {
        obj.insert("data".to_string(), serde_json::Value::Object(data));
        obj.remove("stringData");
    }
    Ok(out)
}

fn json_value_to_secret_string(value: &serde_json::Value) -> Result<String, String> {
    match value {
        serde_json::Value::Null => Ok(String::new()),
        serde_json::Value::Bool(v) => Ok(v.to_string()),
        serde_json::Value::Number(v) => Ok(v.to_string()),
        serde_json::Value::String(v) => Ok(v.clone()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string_pretty(value).map_err(|err| err.to_string())
        }
    }
}

pub(crate) fn apply_decoded_secret_json(
    original: &serde_json::Value,
    edited: &str,
) -> Result<serde_json::Value, String> {
    let json: serde_json::Value = serde_json::from_str(edited).map_err(|err| err.to_string())?;
    let mapping = json
        .as_object()
        .ok_or_else(|| "decoded secret edit must be a JSON object".to_string())?;

    let mut data = serde_json::Map::new();
    for (key, value) in mapping {
        let raw_value = json_value_to_secret_string(value)?;
        data.insert(
            key.to_string(),
            serde_json::Value::String(base64_encode(raw_value.as_bytes())),
        );
    }

    let mut out = original.clone();
    if !out.is_object() {
        return Err("secret manifest is not an object".to_string());
    }
    if let Some(obj) = out.as_object_mut() {
        obj.insert("data".to_string(), serde_json::Value::Object(data));
        obj.remove("stringData");
    }
    Ok(out)
}
