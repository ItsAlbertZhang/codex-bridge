//! Backend-independent process, output and session control.
pub mod config;
pub mod daemon;
mod logging;
pub mod output;
pub mod session;
use serde_json::Value;
/// Backend fields have already been shaped, including their omission rules.
pub fn extend_fields(value: &mut Value, fields: Value) {
    if let Value::Object(fields) = fields {
        value.as_object_mut().expect("event object").extend(fields);
    }
}
