use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value, value::RawValue};
use std::{fmt, ops::Index};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JsString {
    units: Vec<u16>,
    text: Option<String>,
}

impl JsString {
    pub fn from_utf16(units: Vec<u16>) -> Self {
        let text = String::from_utf16(&units).ok();
        Self { units, text }
    }

    pub fn as_str(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn as_units(&self) -> &[u16] {
        &self.units
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.units)
    }

    fn array_index(&self) -> Option<u32> {
        let text = self.as_str()?;
        text.parse::<u32>()
            .ok()
            .filter(|index| *index != u32::MAX && index.to_string() == text)
    }
}

impl From<String> for JsString {
    fn from(text: String) -> Self {
        let units = text.encode_utf16().collect();
        Self {
            units,
            text: Some(text),
        }
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl PartialEq<str> for JsString {
    fn eq(&self, other: &str) -> bool {
        self.units.iter().copied().eq(other.encode_utf16())
    }
}

impl PartialEq<&str> for JsString {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AuditValue {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Utf16String(Vec<u16>),
    Array(Vec<AuditValue>),
    Object(IndexMap<JsString, AuditValue>),
}

impl AuditValue {
    pub fn from_utf16(units: Vec<u16>) -> Self {
        match String::from_utf16(&units) {
            Ok(text) => Self::String(text),
            Err(_) => Self::Utf16String(units),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_utf16(&self) -> Option<Vec<u16>> {
        match self {
            Self::String(text) => Some(text.encode_utf16().collect()),
            Self::Utf16String(units) => Some(units.clone()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(number) => number.as_f64(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Self>> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&IndexMap<JsString, Self>> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut IndexMap<JsString, Self>> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Self> {
        self.get_utf16(&key.encode_utf16().collect::<Vec<_>>())
    }

    pub fn get_utf16(&self, key: &[u16]) -> Option<&Self> {
        self.as_object().and_then(|entries| {
            entries
                .iter()
                .find(|(candidate, _)| candidate.as_units() == key)
                .map(|(_, value)| value)
        })
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Self::Number(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, Self::String(_) | Self::Utf16String(_))
    }

    pub(crate) fn normalize(&mut self) {
        match self {
            Self::Number(number) => {
                *self = number_value(number.as_f64().unwrap_or(f64::NAN)).into()
            }
            Self::Array(items) => items.iter_mut().for_each(Self::normalize),
            Self::Object(entries) => {
                entries.values_mut().for_each(Self::normalize);
                entries.sort_by(|a, _, b, _| match (a.array_index(), b.array_index()) {
                    (Some(a), Some(b)) => a.cmp(&b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
            }
            _ => (),
        }
    }
}

impl From<Value> for AuditValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(value) => Self::Number(value),
            Value::String(value) => Self::String(value),
            Value::Array(items) => Self::Array(items.into_iter().map(Self::from).collect()),
            Value::Object(entries) => Self::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.into(), Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl PartialEq<Value> for AuditValue {
    fn eq(&self, other: &Value) -> bool {
        self == &Self::from(other.clone())
    }
}

impl Index<&str> for AuditValue {
    type Output = Self;

    fn index(&self, key: &str) -> &Self {
        self.get(key).unwrap_or(&Self::Null)
    }
}

impl Index<usize> for AuditValue {
    type Output = Self;

    fn index(&self, index: usize) -> &Self {
        self.as_array()
            .and_then(|items| items.get(index))
            .unwrap_or(&Self::Null)
    }
}

impl fmt::Display for AuditValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&serde_json::to_string(self).map_err(|_| fmt::Error)?)
    }
}

impl Serialize for AuditValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Bool(value) => value.serialize(serializer),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => value.serialize(serializer),
            Self::Utf16String(units) => serialize_utf16(units, serializer),
            Self::Array(items) => items.serialize(serializer),
            Self::Object(entries) => {
                let mut encoded = String::from("{");
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        encoded.push(',');
                    }
                    encoded.push_str(&quote_utf16(key.as_units()));
                    encoded.push(':');
                    encoded.push_str(
                        &serde_json::to_string(value).map_err(serde::ser::Error::custom)?,
                    );
                }
                encoded.push('}');
                RawValue::from_string(encoded)
                    .map_err(serde::ser::Error::custom)?
                    .serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for AuditValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let text = raw.get();
        let value = match text.as_bytes()[0] {
            b'{' => parse_object(text)
                .map(Self::Object)
                .map_err(|error| error.to_string()),
            b'[' => serde_json::from_str(text)
                .map(Self::Array)
                .map_err(|error: serde_json::Error| error.to_string()),
            b'"' => decode_json_string(text).map(Self::from_utf16),
            _ => serde_json::from_str::<Value>(text)
                .map(Self::from)
                .map_err(|error| error.to_string()),
        };
        value.map_err(serde::de::Error::custom)
    }
}

impl Serialize for JsString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_utf16(&self.units, serializer)
    }
}

impl<'de> Deserialize<'de> for JsString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        decode_json_string(raw.get())
            .map(Self::from_utf16)
            .map_err(serde::de::Error::custom)
    }
}

fn serialize_utf16<S: Serializer>(units: &[u16], serializer: S) -> Result<S::Ok, S::Error> {
    RawValue::from_string(quote_utf16(units))
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

fn quote_utf16(units: &[u16]) -> String {
    use fmt::Write;

    let mut encoded = String::from("\"");
    for character in char::decode_utf16(units.iter().copied()) {
        match character {
            Ok(character) => match character {
                '"' => encoded.push_str("\\\""),
                '\\' => encoded.push_str("\\\\"),
                '\u{0008}' => encoded.push_str("\\b"),
                '\u{000c}' => encoded.push_str("\\f"),
                '\n' => encoded.push_str("\\n"),
                '\r' => encoded.push_str("\\r"),
                '\t' => encoded.push_str("\\t"),
                character if character <= '\u{001f}' => {
                    write!(&mut encoded, "\\u{:04x}", character as u32).unwrap();
                }
                character => encoded.push(character),
            },
            Err(error) => {
                write!(&mut encoded, "\\u{:04x}", error.unpaired_surrogate()).unwrap();
            }
        }
    }
    encoded.push('"');
    encoded
}

fn decode_json_string(text: &str) -> Result<Vec<u16>, String> {
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return Err("expected a JSON string".into());
    }
    let mut units = Vec::new();
    let mut index = 1;
    while index < bytes.len() - 1 {
        if bytes[index] == b'\\' {
            index += 1;
            let escape = *bytes.get(index).ok_or("incomplete JSON escape")?;
            match escape {
                b'"' => units.push(u16::from(b'"')),
                b'\\' => units.push(u16::from(b'\\')),
                b'/' => units.push(u16::from(b'/')),
                b'b' => units.push(8),
                b'f' => units.push(12),
                b'n' => units.push(10),
                b'r' => units.push(13),
                b't' => units.push(9),
                b'u' => {
                    let end = index.checked_add(5).ok_or("invalid Unicode escape")?;
                    if end >= bytes.len() {
                        return Err("incomplete Unicode escape".into());
                    }
                    let mut code = 0u16;
                    for byte in &bytes[index + 1..end] {
                        code = code
                            .checked_mul(16)
                            .and_then(|value| {
                                value.checked_add((*byte as char).to_digit(16)? as u16)
                            })
                            .ok_or("invalid Unicode escape")?;
                    }
                    units.push(code);
                    index = end - 1;
                }
                _ => return Err("invalid JSON escape".into()),
            }
            index += 1;
        } else {
            let character = text[index..]
                .chars()
                .next()
                .ok_or("invalid UTF-8 in JSON string")?;
            if character <= '\u{001f}' {
                return Err("unescaped control character in JSON string".into());
            }
            let mut pair = [0; 2];
            units.extend_from_slice(character.encode_utf16(&mut pair));
            index += character.len_utf8();
        }
    }
    Ok(units)
}

fn parse_object(text: &str) -> Result<IndexMap<JsString, AuditValue>, String> {
    let mut index = 0;
    skip_json_space(text.as_bytes(), &mut index);
    expect_byte(text.as_bytes(), &mut index, b'{')?;
    let mut entries = IndexMap::new();
    let mut after_comma = false;
    loop {
        skip_json_space(text.as_bytes(), &mut index);
        if text.as_bytes().get(index) == Some(&b'}') {
            if after_comma {
                return Err("trailing comma in JSON object".into());
            }
            index += 1;
            break;
        }
        let key_start = index;
        index = scan_string(text.as_bytes(), index)?;
        let key = JsString::from_utf16(decode_json_string(&text[key_start..index])?);
        skip_json_space(text.as_bytes(), &mut index);
        expect_byte(text.as_bytes(), &mut index, b':')?;
        skip_json_space(text.as_bytes(), &mut index);
        let value_start = index;
        index = scan_value(text.as_bytes(), index)?;
        let value =
            serde_json::from_str(&text[value_start..index]).map_err(|error| error.to_string())?;
        entries.insert(key, value);
        skip_json_space(text.as_bytes(), &mut index);
        match text.as_bytes().get(index) {
            Some(b',') => {
                index += 1;
                after_comma = true;
            }
            Some(b'}') => {
                index += 1;
                break;
            }
            _ => return Err("expected comma or closing brace".into()),
        }
    }
    skip_json_space(text.as_bytes(), &mut index);
    if index == text.len() {
        Ok(entries)
    } else {
        Err("trailing content after JSON object".into())
    }
}

fn skip_json_space(bytes: &[u8], index: &mut usize) {
    while matches!(bytes.get(*index), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        *index += 1;
    }
}

fn expect_byte(bytes: &[u8], index: &mut usize, expected: u8) -> Result<(), String> {
    if bytes.get(*index) == Some(&expected) {
        *index += 1;
        Ok(())
    } else {
        Err(format!("expected '{}'", expected as char))
    }
}

fn scan_string(bytes: &[u8], start: usize) -> Result<usize, String> {
    if bytes.get(start) != Some(&b'"') {
        return Err("expected a JSON string".into());
    }
    let mut index = start + 1;
    while let Some(byte) = bytes.get(index) {
        match byte {
            b'"' => return Ok(index + 1),
            b'\\' => index += 2,
            0..=31 => return Err("unescaped control character in JSON string".into()),
            _ => index += 1,
        }
    }
    Err("unterminated JSON string".into())
}

fn scan_value(bytes: &[u8], start: usize) -> Result<usize, String> {
    match bytes.get(start) {
        Some(b'"') => scan_string(bytes, start),
        Some(b'{' | b'[') => {
            let mut stack = vec![if bytes[start] == b'{' { b'}' } else { b']' }];
            let mut index = start + 1;
            while let Some(byte) = bytes.get(index) {
                match byte {
                    b'"' => index = scan_string(bytes, index)?,
                    b'{' => {
                        stack.push(b'}');
                        index += 1;
                    }
                    b'[' => {
                        stack.push(b']');
                        index += 1;
                    }
                    b'}' | b']' => {
                        if stack.pop() != Some(*byte) {
                            return Err("mismatched JSON container".into());
                        }
                        index += 1;
                        if stack.is_empty() {
                            return Ok(index);
                        }
                    }
                    _ => index += 1,
                }
            }
            Err("unterminated JSON container".into())
        }
        Some(_) => {
            let mut index = start;
            while let Some(byte) = bytes.get(index) {
                if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                    break;
                }
                index += 1;
            }
            if index == start {
                Err("expected a JSON value".into())
            } else {
                Ok(index)
            }
        }
        None => Err("expected a JSON value".into()),
    }
}

pub(crate) fn number_value(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    let mut buffer = ryu_js::Buffer::new();
    Value::Number(buffer.format_finite(value).parse().unwrap())
}

pub(crate) fn normalize_json(value: &mut Value) {
    match value {
        Value::Number(number) => *value = number_value(number.as_f64().unwrap_or(f64::NAN)),
        Value::Array(items) => items.iter_mut().for_each(normalize_json),
        Value::Object(entries) => {
            entries.values_mut().for_each(normalize_json);
            let mut items: Vec<_> = std::mem::take(entries).into_iter().collect();
            items.sort_by(|(a, _), (b, _)| match (array_index(a), array_index(b)) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
            entries.extend(items);
        }
        _ => (),
    }
}

fn array_index(key: &str) -> Option<u32> {
    key.parse::<u32>()
        .ok()
        .filter(|index| *index != u32::MAX && index.to_string() == key)
}

macro_rules! comparable_value {
    ($($kind:ty),*) => {$ (
        impl PartialEq<$kind> for AuditValue {
            fn eq(&self, other: &$kind) -> bool {
                self == &serde_json::to_value(other).unwrap()
            }
        }
    )*};
}

comparable_value!(bool, i32, i64, u64, usize, f64, String, &str);
