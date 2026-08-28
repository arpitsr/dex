use super::ToolError;
use serde_json::{Map, Value};

pub(crate) fn execute(name: &str, args: &Map<String, Value>) -> Result<String, ToolError> {
    super::execute(name, args)
}
