use std::collections::HashMap;

use serde_json::{Map, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Converted {
    Object,
    Other,
}

// Zod 4.6.1's from-json-schema converter can wrap an object or reject its schema.
// TypeScript's nativeInputSchema uses a loose object in either case.
pub(super) fn converts_to_object(schema: &Value) -> bool {
    Converter {
        root: schema,
        references: HashMap::new(),
    }
    .convert(schema)
        == Some(Converted::Object)
}

struct Converter<'a> {
    root: &'a Value,
    references: HashMap<String, Option<Converted>>,
}

impl Converter<'_> {
    fn convert(&mut self, value: &Value) -> Option<Converted> {
        if value.is_boolean() {
            return Some(Converted::Other);
        }
        let schema = value.as_object()?;
        let mut converted = self.base(schema)?;
        let explicit = schema.contains_key("type")
            || schema.contains_key("enum")
            || schema.contains_key("const");
        for keyword in ["anyOf", "oneOf", "allOf"] {
            if let Some(variants) = schema.get(keyword).and_then(Value::as_array) {
                let mut last = Converted::Other;
                for variant in variants {
                    last = self.convert(variant)?;
                }
                converted = match (keyword, explicit, variants.len()) {
                    ("allOf", true, 0) => converted,
                    ("allOf", false, 1) => last,
                    _ => Converted::Other,
                };
            }
        }
        if schema.contains_key("default") || schema.get("readOnly") == Some(&Value::Bool(true)) {
            converted = Converted::Other;
        }
        Some(converted)
    }

    fn base(&mut self, schema: &Map<String, Value>) -> Option<Converted> {
        if let Some(not) = schema.get("not") {
            return not
                .as_object()
                .is_some_and(Map::is_empty)
                .then_some(Converted::Other);
        }
        if [
            "unevaluatedItems",
            "unevaluatedProperties",
            "if",
            "then",
            "else",
            "dependentSchemas",
            "dependentRequired",
        ]
        .iter()
        .any(|keyword| schema.contains_key(*keyword))
        {
            return None;
        }
        if let Some(reference) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            if let Some(converted) = self.references.get(reference) {
                return Some(converted.unwrap_or(Converted::Other));
            }
            self.references.insert(reference.to_owned(), None);
            let path = reference.strip_prefix('#')?;
            let mut segments = path.split('/').filter(|segment| !segment.is_empty());
            let target = match segments.next() {
                None => self.root,
                Some(namespace) => {
                    let expected = match self.root["$schema"].as_str() {
                        Some(
                            "http://json-schema.org/draft-07/schema#"
                            | "http://json-schema.org/draft-04/schema#",
                        ) => "definitions",
                        _ => "$defs",
                    };
                    if namespace != expected {
                        return None;
                    }
                    let name = segments.next()?.replace("~1", "/").replace("~0", "~");
                    self.root
                        .get("$defs")
                        .or_else(|| self.root.get("definitions"))?
                        .get(&name)?
                }
            };
            let converted = self.convert(target)?;
            self.references
                .insert(reference.to_owned(), Some(converted));
            return Some(converted);
        }
        if let Some(values) = schema.get("enum") {
            values.as_array()?;
            return Some(Converted::Other);
        }
        if schema.contains_key("const") {
            return Some(Converted::Other);
        }
        match schema.get("type") {
            None => Some(Converted::Other),
            Some(Value::String(kind)) => self.typed(schema, kind),
            Some(Value::Array(kinds)) => {
                let mut converted = Converted::Other;
                for kind in kinds {
                    converted = self.typed(schema, kind.as_str()?)?;
                }
                Some(if kinds.len() == 1 {
                    converted
                } else {
                    Converted::Other
                })
            }
            _ => None,
        }
    }

    fn typed(&mut self, schema: &Map<String, Value>, kind: &str) -> Option<Converted> {
        match kind {
            "object" => {
                if let Some(additional) = schema
                    .get("additionalProperties")
                    .filter(|s| !s.is_boolean())
                {
                    self.convert(additional)?;
                }
                if let Some(properties) = schema.get("properties") {
                    for property in properties.as_object()?.values() {
                        self.convert(property)?;
                    }
                }
                let mut converted = Converted::Object;
                if let Some(patterns) = schema.get("patternProperties") {
                    let patterns = patterns.as_object()?;
                    for (pattern, value) in patterns {
                        regress::Regex::new(pattern).ok()?;
                        self.convert(value)?;
                    }
                    if !patterns.is_empty() {
                        converted = Converted::Other;
                    }
                }
                if let Some(names) = schema
                    .get("propertyNames")
                    .filter(|s| **s != Value::Bool(true))
                {
                    let mut names = names.clone();
                    if let Some(object) = names.as_object_mut() {
                        object
                            .entry("type")
                            .or_insert_with(|| Value::String("string".into()));
                    }
                    self.convert(&names)?;
                    converted = Converted::Other;
                }
                if ["minProperties", "maxProperties"]
                    .iter()
                    .any(|key| schema.get(*key).is_some_and(Value::is_number))
                {
                    converted = Converted::Other;
                }
                Some(converted)
            }
            "array" => {
                let items = schema.get("items");
                if let Some(prefix) = schema.get("prefixItems").and_then(Value::as_array) {
                    for item in prefix {
                        self.convert(item)?;
                    }
                    if let Some(rest) = items.filter(|s| !s.is_array()) {
                        self.convert(rest)?;
                    }
                } else if let Some(items) = items.and_then(Value::as_array) {
                    for item in items {
                        self.convert(item)?;
                    }
                    if let Some(rest) = schema.get("additionalItems") {
                        self.convert(rest)?;
                    }
                } else if let Some(items) = items {
                    self.convert(items)?;
                }
                if let Some(contains) = schema.get("contains") {
                    self.convert(contains)?;
                }
                Some(Converted::Other)
            }
            "string" => {
                if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
                    regress::Regex::new(pattern).ok()?;
                }
                Some(Converted::Other)
            }
            "number" | "integer" | "boolean" | "null" => Some(Converted::Other),
            _ => None,
        }
    }
}
