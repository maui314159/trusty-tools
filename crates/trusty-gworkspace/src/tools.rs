//! MCP `tools/list` response — JSON Schema for every exposed tool.
//!
//! Why: Claude Code (and any MCP client) needs a machine-readable contract
//! describing what arguments each tool accepts so the model can fill them
//! correctly. Centralising in one file makes it easy to grep for a tool
//! name and inspect its schema next to the dispatcher.
//! What: `tool_list_response()` returns a JSON object of the shape
//! `{"tools": [{"name", "description", "inputSchema"}, ...]}`.
//! Test: Unit test below asserts the tool count and that every entry has
//! the three required fields.

use serde_json::{Value, json};

fn account_schema() -> Value {
    json!({
        "type": "string",
        "description": "The Google account profile to use. Defaults to the default profile.",
    })
}

fn action_enum(actions: &[&str]) -> Value {
    json!({
        "type": "string",
        "description": "Operation to perform.",
        "enum": actions,
    })
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        }
    })
}

/// Build the full `tools/list` response.
///
/// Why: One function = one source of truth for the MCP contract.
/// What: Returns all 40+ tools across accounts, calendar, gmail, drive,
/// docs, sheets, slides, tasks.
/// Test: `tool_list_has_expected_count` asserts >= 40 tools.
pub fn tool_list_response() -> Value {
    let mut tools = Vec::<Value>::new();

    // ----- Accounts -----
    tools.push(tool(
        "list_accounts",
        "List configured Google Workspace account profiles available on this machine.",
        json!({ "account": account_schema() }),
        &[],
    ));

    // ----- Calendar -----
    tools.push(tool(
        "manage_calendars",
        "Create, read, update, or delete Google Calendars.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["list", "create", "update", "delete"]),
            "calendar_id": { "type": "string", "description": "Calendar ID (required for update/delete)." },
            "summary": { "type": "string", "description": "Calendar title (create)." },
            "description": { "type": "string" },
            "time_zone": { "type": "string" },
            "updates": { "type": "object", "description": "Patch body for update." },
        }),
        &["action"],
    ));
    tools.push(tool(
        "manage_events",
        "Create, read, update, or delete events within a Google Calendar.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["list", "create", "update", "delete"]),
            "calendar_id": { "type": "string", "description": "Calendar ID. Defaults to 'primary'." },
            "event_id": { "type": "string" },
            "event": { "type": "object", "description": "Event resource (create)." },
            "updates": { "type": "object" },
            "time_min": { "type": "string", "description": "RFC3339 lower bound (list)." },
            "time_max": { "type": "string", "description": "RFC3339 upper bound (list)." },
            "query": { "type": "string", "description": "Free-text search (list)." },
            "max_results": { "type": "integer" },
        }),
        &["action"],
    ));
    tools.push(tool(
        "query_free_busy",
        "Query free/busy status across calendars for a time range.",
        json!({
            "account": account_schema(),
            "time_min": { "type": "string", "description": "RFC3339 start." },
            "time_max": { "type": "string", "description": "RFC3339 end." },
            "calendar_ids": { "type": "array", "items": { "type": "string" } },
        }),
        &["time_min", "time_max"],
    ));

    // ----- Gmail -----
    tools.push(tool(
        "search_gmail_messages",
        "Search Gmail messages using Gmail query syntax (e.g. 'from:foo subject:bar').",
        json!({
            "account": account_schema(),
            "query": { "type": "string" },
            "max_results": { "type": "integer" },
        }),
        &[],
    ));
    tools.push(tool(
        "get_gmail_message_content",
        "Fetch the full content of a Gmail message by ID.",
        json!({
            "account": account_schema(),
            "message_id": { "type": "string" },
        }),
        &["message_id"],
    ));
    tools.push(tool(
        "download_gmail_attachment",
        "Download a Gmail attachment by message + attachment ID, optionally writing to disk.",
        json!({
            "account": account_schema(),
            "message_id": { "type": "string" },
            "attachment_id": { "type": "string" },
            "save_path": { "type": "string", "description": "If set, decoded bytes are written here." },
            "return_content": { "type": "boolean", "description": "If true, returns the base64 body inline." },
        }),
        &["message_id", "attachment_id"],
    ));
    tools.push(tool(
        "list_message_attachments",
        "Enumerate attachments on a Gmail message.",
        json!({
            "account": account_schema(),
            "message_id": { "type": "string" },
        }),
        &["message_id"],
    ));
    tools.push(tool(
        "compose_email",
        "Send, draft, or send-an-existing-draft email via Gmail.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["send", "draft", "send_draft"]),
            "to": { "type": "string" },
            "cc": { "type": "string" },
            "bcc": { "type": "string" },
            "subject": { "type": "string" },
            "body": { "type": "string" },
            "html": { "type": "boolean" },
            "draft_id": { "type": "string", "description": "Required when action=send_draft." },
        }),
        &[],
    ));
    tools.push(tool(
        "modify_gmail_messages",
        "Batch-add or remove labels across a set of Gmail messages.",
        json!({
            "account": account_schema(),
            "message_ids": { "type": "array", "items": { "type": "string" } },
            "add_label_ids": { "type": "array", "items": { "type": "string" } },
            "remove_label_ids": { "type": "array", "items": { "type": "string" } },
        }),
        &["message_ids"],
    ));
    tools.push(tool(
        "format_email_content",
        "Convert markdown-flavoured text to a simple HTML body suitable for compose_email.",
        json!({
            "body": { "type": "string" },
            "mode": { "type": "string", "enum": ["auto", "passthrough"] },
        }),
        &["body"],
    ));
    tools.push(tool(
        "manage_gmail_labels",
        "CRUD Gmail labels.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["list", "create", "update", "delete"]),
            "label_id": { "type": "string" },
            "name": { "type": "string" },
            "label_list_visibility": { "type": "string" },
            "message_list_visibility": { "type": "string" },
            "updates": { "type": "object" },
        }),
        &["action"],
    ));
    tools.push(tool(
        "manage_gmail_filters",
        "List, create, or delete Gmail filters.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["list", "create", "delete"]),
            "filter": { "type": "object", "description": "Filter resource (create)." },
            "filter_id": { "type": "string" },
        }),
        &["action"],
    ));
    tools.push(tool(
        "manage_gmail_settings",
        "Get or update Gmail account settings (vacation, autoForwarding, imap, pop, language).",
        json!({
            "account": account_schema(),
            "setting": { "type": "string", "enum": ["vacation", "auto_forwarding", "imap", "pop", "language"] },
            "action": action_enum(&["get", "update"]),
            "value": { "type": "object" },
        }),
        &["setting"],
    ));

    // ----- Drive -----
    tools.push(tool(
        "list_drive_contents",
        "List the contents of a Drive folder (defaults to root).",
        json!({
            "account": account_schema(),
            "folder_id": { "type": "string" },
            "max_results": { "type": "integer" },
        }),
        &[],
    ));
    tools.push(tool(
        "search_drive_files",
        "Search Drive using v3 query syntax.",
        json!({
            "account": account_schema(),
            "query": { "type": "string" },
            "max_results": { "type": "integer" },
        }),
        &["query"],
    ));
    tools.push(tool(
        "get_drive_file_content",
        "Fetch the textual content of a Drive file (auto-exports Google native docs).",
        json!({
            "account": account_schema(),
            "file_id": { "type": "string" },
            "export_mime_type": { "type": "string", "description": "Override export MIME for Google native files." },
        }),
        &["file_id"],
    ));
    tools.push(tool(
        "list_shared_drives",
        "List shared drives the account has access to.",
        json!({ "account": account_schema() }),
        &[],
    ));
    tools.push(tool(
        "manage_drive_file",
        "Create folders, rename/move/copy/trash/delete files in Drive.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["create_folder", "rename", "trash", "delete", "copy", "move"]),
            "file_id": { "type": "string" },
            "name": { "type": "string" },
            "parent_id": { "type": "string" },
        }),
        &["action"],
    ));
    tools.push(tool(
        "manage_file_permissions",
        "List, create, update, or delete sharing permissions on a Drive file.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["list", "create", "update", "delete"]),
            "file_id": { "type": "string" },
            "permission_id": { "type": "string" },
            "role": { "type": "string", "description": "reader|commenter|writer|owner|organizer" },
            "type": { "type": "string", "description": "user|group|domain|anyone" },
            "email_address": { "type": "string" },
            "domain": { "type": "string" },
            "send_notification_email": { "type": "string", "enum": ["true", "false"] },
        }),
        &["action", "file_id"],
    ));

    // ----- Docs -----
    tools.push(tool(
        "create_document",
        "Create a new empty Google Doc with the given title.",
        json!({ "account": account_schema(), "title": { "type": "string" } }),
        &[],
    ));
    tools.push(tool(
        "append_to_document",
        "Append text to the end of a Google Doc.",
        json!({
            "account": account_schema(),
            "document_id": { "type": "string" },
            "text": { "type": "string" },
        }),
        &["document_id", "text"],
    ));
    tools.push(tool(
        "get_document",
        "Fetch the full Google Doc JSON.",
        json!({ "account": account_schema(), "document_id": { "type": "string" } }),
        &["document_id"],
    ));
    tools.push(tool(
        "get_document_structure",
        "Return the structural outline of a Google Doc (headings, paragraphs, tables) without inline runs.",
        json!({ "account": account_schema(), "document_id": { "type": "string" } }),
        &["document_id"],
    ));
    tools.push(tool(
        "replace_text_in_document",
        "Replace every occurrence of `find` with `replace` in a Google Doc.",
        json!({
            "account": account_schema(),
            "document_id": { "type": "string" },
            "find": { "type": "string" },
            "replace": { "type": "string" },
        }),
        &["document_id", "find", "replace"],
    ));
    tools.push(tool(
        "insert_text_in_document",
        "Insert text at a specific index in a Google Doc.",
        json!({
            "account": account_schema(),
            "document_id": { "type": "string" },
            "text": { "type": "string" },
            "index": { "type": "integer" },
        }),
        &["document_id", "text"],
    ));
    tools.push(tool(
        "delete_range_in_document",
        "Delete a content range from a Google Doc.",
        json!({
            "account": account_schema(),
            "document_id": { "type": "string" },
            "start_index": { "type": "integer" },
            "end_index": { "type": "integer" },
        }),
        &["document_id", "start_index", "end_index"],
    ));
    tools.push(tool(
        "manage_document_comments",
        "List/create/reply/resolve/delete comments on a Google Doc.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["list", "create", "reply", "resolve", "delete"]),
            "document_id": { "type": "string" },
            "comment_id": { "type": "string" },
            "content": { "type": "string" },
        }),
        &["action", "document_id"],
    ));
    tools.push(tool(
        "format_document_range",
        "Apply bold/italic/underline/font size/named style to a range in a Google Doc.",
        json!({
            "account": account_schema(),
            "document_id": { "type": "string" },
            "start_index": { "type": "integer" },
            "end_index": { "type": "integer" },
            "bold": { "type": "boolean" },
            "italic": { "type": "boolean" },
            "underline": { "type": "boolean" },
            "font_size": { "type": "number" },
            "named_style": { "type": "string", "description": "e.g. HEADING_1, NORMAL_TEXT" },
        }),
        &["document_id", "start_index", "end_index"],
    ));
    tools.push(tool(
        "set_document_style",
        "Update document-level style properties (page size, margins, etc.).",
        json!({
            "account": account_schema(),
            "document_id": { "type": "string" },
            "style": { "type": "object" },
            "fields": { "type": "string", "description": "Field mask, defaults to '*'." },
        }),
        &["document_id"],
    ));
    tools.push(tool(
        "insert_table_in_document",
        "Insert a table at the given index in a Google Doc.",
        json!({
            "account": account_schema(),
            "document_id": { "type": "string" },
            "rows": { "type": "integer" },
            "columns": { "type": "integer" },
            "index": { "type": "integer" },
        }),
        &["document_id"],
    ));
    tools.push(tool(
        "find_tables_in_document",
        "Enumerate tables in a Google Doc.",
        json!({ "account": account_schema(), "document_id": { "type": "string" } }),
        &["document_id"],
    ));
    tools.push(tool(
        "manage_table_structure",
        "Insert or delete a row or column in a Google Doc table.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["insert_row", "insert_column", "delete_row", "delete_column"]),
            "document_id": { "type": "string" },
            "table_start_index": { "type": "integer" },
            "row": { "type": "integer" },
            "column": { "type": "integer" },
            "below": { "type": "boolean" },
            "right": { "type": "boolean" },
        }),
        &["action", "document_id", "table_start_index"],
    ));

    // ----- Sheets -----
    tools.push(tool(
        "get_spreadsheet",
        "Fetch a spreadsheet's metadata (and optionally grid data).",
        json!({
            "account": account_schema(),
            "spreadsheet_id": { "type": "string" },
            "include_grid_data": { "type": "boolean" },
        }),
        &["spreadsheet_id"],
    ));
    tools.push(tool(
        "manage_spreadsheet",
        "Create a spreadsheet or add/delete sheets within one.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["create", "add_sheet", "delete_sheet"]),
            "spreadsheet_id": { "type": "string" },
            "title": { "type": "string" },
            "sheet_id": { "type": "integer" },
        }),
        &["action"],
    ));
    tools.push(tool(
        "modify_sheet_values",
        "Read, write, append, or clear cell values in a sheet range.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["read", "write", "update", "append", "clear"]),
            "spreadsheet_id": { "type": "string" },
            "range": { "type": "string", "description": "A1 notation, e.g. 'Sheet1!A1:C10'" },
            "values": { "type": "array", "items": { "type": "array" } },
        }),
        &["spreadsheet_id", "range"],
    ));
    tools.push(tool(
        "format_sheet",
        "Apply a batchUpdate to a spreadsheet (formatting, conditional rules, etc.).",
        json!({
            "account": account_schema(),
            "spreadsheet_id": { "type": "string" },
            "requests": { "type": "array", "items": { "type": "object" } },
        }),
        &["spreadsheet_id", "requests"],
    ));

    // ----- Slides -----
    tools.push(tool(
        "get_slides",
        "Fetch a Google Slides presentation JSON.",
        json!({ "account": account_schema(), "presentation_id": { "type": "string" } }),
        &["presentation_id"],
    ));
    tools.push(tool(
        "manage_slides",
        "Create a presentation or create/delete slides within one.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["create_presentation", "create_slide", "delete_slide"]),
            "presentation_id": { "type": "string" },
            "slide_id": { "type": "string" },
            "title": { "type": "string" },
            "layout": { "type": "string" },
        }),
        &["action"],
    ));
    tools.push(tool(
        "add_slide_content",
        "Add a text box with the given text to a slide.",
        json!({
            "account": account_schema(),
            "presentation_id": { "type": "string" },
            "slide_id": { "type": "string" },
            "text": { "type": "string" },
        }),
        &["presentation_id", "slide_id", "text"],
    ));

    // ----- Tasks -----
    tools.push(tool(
        "manage_task_lists",
        "CRUD Google Tasks lists.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["list", "create", "update", "delete"]),
            "tasklist_id": { "type": "string" },
            "title": { "type": "string" },
            "updates": { "type": "object" },
        }),
        &["action"],
    ));
    tools.push(tool(
        "manage_tasks",
        "CRUD or complete/move tasks within a Google Tasks list.",
        json!({
            "account": account_schema(),
            "action": action_enum(&["list", "create", "update", "delete", "complete", "move"]),
            "tasklist_id": { "type": "string" },
            "task_id": { "type": "string" },
            "task": { "type": "object" },
            "updates": { "type": "object" },
            "parent": { "type": "string" },
            "previous": { "type": "string" },
        }),
        &["action"],
    ));
    tools.push(tool(
        "list_tasks",
        "List tasks from the default Google Tasks list (id, title, due, status, notes).",
        json!({
            "account": account_schema(),
            "tasklist_id": {
                "type": "string",
                "description": "Optional task list ID; defaults to the user's @default list.",
            },
            "max_results": {
                "type": "integer",
                "description": "Maximum number of tasks to return (default 20).",
                "minimum": 1,
                "maximum": 100,
            },
            "show_completed": {
                "type": "boolean",
                "description": "Include completed tasks (default false).",
            },
        }),
        &[],
    ));
    tools.push(tool(
        "complete_task",
        "Mark a single Google Task as completed.",
        json!({
            "account": account_schema(),
            "tasklist_id": {
                "type": "string",
                "description": "Optional task list ID; defaults to @default.",
            },
            "task_id": {
                "type": "string",
                "description": "The task ID (from list_tasks).",
            },
        }),
        &["task_id"],
    ));

    json!({ "tools": tools })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_has_expected_count() {
        let v = tool_list_response();
        let tools = v["tools"].as_array().expect("tools array");
        assert!(
            tools.len() >= 40,
            "expected >= 40 tools, got {}",
            tools.len()
        );
        for t in tools {
            assert!(t["name"].is_string(), "every tool has a name");
            assert!(t["description"].is_string(), "every tool has a description");
            assert!(
                t["inputSchema"]["type"] == "object",
                "every tool has object inputSchema"
            );
        }
    }

    #[test]
    fn every_tool_name_is_unique() {
        use std::collections::HashSet;
        let v = tool_list_response();
        let mut seen = HashSet::new();
        for t in v["tools"].as_array().unwrap() {
            let name = t["name"].as_str().unwrap().to_string();
            assert!(seen.insert(name.clone()), "duplicate tool: {name}");
        }
    }
}
