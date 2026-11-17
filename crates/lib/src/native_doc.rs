//! Key and number adapters for gaps in the pinned Eidetica Doc API.

use eidetica::crdt::Doc;

pub(crate) const WIDE_NUMBER: &str = "~number";

// `Doc::set` interprets every dotted key as a path. Keep other keys literal
// except for the prefix reserved by the escaping and numeric encodings.
pub(crate) fn field_key(key: &str) -> String {
    if !key.contains('.') && !key.starts_with('~') {
        return key.to_owned();
    }
    use std::fmt::Write;
    let mut encoded = String::from("~");
    for byte in key.bytes() {
        write!(encoded, "{byte:02x}").expect("String write");
    }
    encoded
}

pub(crate) fn unhex(hex: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        hex.len().is_multiple_of(2) && hex.is_ascii(),
        "invalid escaped document key"
    );
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(String::from_utf8(bytes)?)
}

pub(crate) fn decode_field_key(key: &str) -> anyhow::Result<String> {
    let decoded = match key.strip_prefix('~') {
        Some(hex) => unhex(hex)?,
        None => key.to_owned(),
    };
    anyhow::ensure!(field_key(&decoded) == key, "noncanonical document key");
    Ok(decoded)
}

pub(crate) fn wide_number(number: &serde_json::Number) -> Doc {
    let mut doc = Doc::atomic();
    doc.set(WIDE_NUMBER, number.to_string());
    doc
}
