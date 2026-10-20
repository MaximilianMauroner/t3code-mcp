use serde::Serialize;
use serde_json::value::{RawValue, to_raw_value};

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct ToolResult(Box<RawValue>);

impl ToolResult {
    pub fn new(value: &impl Serialize) -> serde_json::Result<Self> {
        to_raw_value(value).map(Self)
    }

    pub fn structured(value: &impl Serialize) -> serde_json::Result<Self> {
        #[derive(Serialize)]
        struct Content {
            r#type: &'static str,
            text: String,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Result<'a, T> {
            content: [Content; 1],
            structured_content: &'a T,
        }
        Self::new(&Result {
            content: [Content {
                r#type: "text",
                text: serde_json::to_string_pretty(value)?,
            }],
            structured_content: value,
        })
    }

    pub fn as_raw(&self) -> &RawValue {
        &self.0
    }

    pub(super) fn into_raw(self) -> Box<RawValue> {
        self.0
    }
}
