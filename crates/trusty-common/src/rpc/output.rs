//! Pretty-printers for `trpc` output.
//!
//! Why: raw JSON is hard to scan; users running the CLI interactively want a
//! quick overview of tool lists, tool results, and server info.
//! What: a handful of free functions that take a `Value` and print to stdout.
//! Colour is auto-disabled when stdout isn't a TTY (handled by `colored`).
//! Test: `print_*_smoke` unit tests below assert the functions run without
//! panicking on representative shapes.

use colored::Colorize;
use serde_json::Value;

/// Pretty-print arbitrary JSON to stdout (with trailing newline).
///
/// Why: shared fallback used by every other formatter when the shape isn't
/// what we expected.
/// What: serialises with two-space indentation; relies on `colored` auto-
/// detection for TTY-aware colouring (keys are left uncolored to keep this
/// portable across terminals).
/// Test: `print_json_smoke`.
pub fn print_json(val: &Value) {
    match serde_json::to_string_pretty(val) {
        Ok(s) => println!("{s}"),
        Err(_) => println!("{val}"),
    }
}

/// Print server info from an `initialize` response.
/// Why: After `initialize`, the user wants a glanceable summary of who they're talking to.
/// What: Pretty-prints `serverInfo` and `capabilities` fields using `colored`.
/// Test: Visual; exercised by manual CLI runs.
pub fn print_server_info(result: &Value) {
    let server_info = result.get("serverInfo").unwrap_or(result);
    let name = server_info
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");
    let version = server_info
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");
    let proto = result
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");

    println!("{}", "Server".bold().cyan());
    println!("  name:             {name}");
    println!("  version:          {version}");
    println!("  protocolVersion:  {proto}");

    if let Some(caps) = result.get("capabilities") {
        println!("  capabilities:     {}", compact(caps));
    }
}

/// Print a `tools/list` response in a human-readable form.
/// Why: `tools/list` responses are too large to read raw; tabulate them.
/// What: Renders a `name | description` table with terminal-aware widths.
/// Test: Visual; exercised by manual CLI runs.
pub fn print_tools_list(result: &Value) {
    let tools = match result.get("tools").and_then(|v| v.as_array()) {
        Some(t) => t,
        None => {
            print_json(result);
            return;
        }
    };

    if tools.is_empty() {
        println!("(no tools)");
        return;
    }

    for (i, tool) in tools.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let name = tool
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("(unnamed)");
        let desc = tool
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        println!("{}", name.bold().cyan());
        if !desc.is_empty() {
            for line in desc.lines() {
                println!("  {line}");
            }
        }

        if let Some(schema) = tool.get("inputSchema") {
            let required: Vec<&str> = schema
                .get("required")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let props = schema.get("properties").and_then(|v| v.as_object());
            if let Some(props) = props {
                let mut required_params: Vec<String> = Vec::new();
                let mut optional_params: Vec<String> = Vec::new();
                for (key, val) in props {
                    let ty = val.get("type").and_then(|v| v.as_str()).unwrap_or("any");
                    let entry = format!("{key}: {ty}");
                    if required.contains(&key.as_str()) {
                        required_params.push(entry);
                    } else {
                        optional_params.push(entry);
                    }
                }
                if !required_params.is_empty() {
                    println!("  {} {}", "required:".yellow(), required_params.join(", "));
                }
                if !optional_params.is_empty() {
                    println!("  {} {}", "optional:".dimmed(), optional_params.join(", "));
                }
            }
        }
    }
}

/// Print the result of a `tools/call` response.
///
/// MCP convention: `result.content` is an array of `{type, text}` objects.
/// We unwrap the text and, if it parses as JSON, pretty-print it.
/// Why: Tool results land in an MCP `content` array; unwrap them for the user.
/// What: Decodes the text/json content blocks and pretty-prints each.
/// Test: Visual; exercised by manual CLI runs.
pub fn print_tool_result(result: &Value) {
    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        for item in content {
            let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ty == "text" {
                let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                // Try to pretty-print embedded JSON.
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    print_json(&parsed);
                } else {
                    println!("{text}");
                }
            } else {
                print_json(item);
            }
        }
        if let Some(true) = result.get("isError").and_then(|v| v.as_bool()) {
            eprintln!("{}", "(tool reported isError=true)".red());
        }
        return;
    }
    print_json(result);
}

fn compact(val: &Value) -> String {
    serde_json::to_string(val).unwrap_or_else(|_| val.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn print_json_smoke() {
        // Should not panic.
        print_json(&json!({"a": 1, "b": [1, 2, 3]}));
    }

    #[test]
    fn print_server_info_smoke() {
        let v = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "test", "version": "0.0.1"}
        });
        print_server_info(&v);
    }

    #[test]
    fn print_tools_list_smoke() {
        let v = json!({
            "tools": [
                {
                    "name": "echo",
                    "description": "Echoes input",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"msg": {"type": "string"}},
                        "required": ["msg"]
                    }
                }
            ]
        });
        print_tools_list(&v);
    }

    #[test]
    fn print_tools_list_handles_empty() {
        let v = json!({"tools": []});
        print_tools_list(&v);
    }

    #[test]
    fn print_tool_result_unwraps_text_json() {
        let v = json!({
            "content": [{"type": "text", "text": "{\"hello\": \"world\"}"}]
        });
        print_tool_result(&v);
    }

    #[test]
    fn print_tool_result_falls_back_for_unknown_shape() {
        let v = json!({"raw": 1});
        print_tool_result(&v);
    }
}
