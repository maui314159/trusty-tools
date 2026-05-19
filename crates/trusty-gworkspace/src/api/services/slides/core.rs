//! Slides core: get deck, create/add slides, add content via batchUpdate.
//!
//! Why: Minimum viable surface for an agent-authored slide deck workflow.
//! What: Three tools: `get_slides`, `manage_slides`, `add_slide_content`.
//! Test: Live only.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::client::BaseClient;
use crate::api::constants::SLIDES_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: Fetching a presentation's slide tree is the entry to every Slides workflow.
/// What: GETs `/presentations/{id}` and returns the full Slides JSON.
/// Test: Live API.
pub async fn get_slides(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "presentation_id")?;
    let url = format!("{SLIDES_API_BASE}/presentations/{id}");
    client.get(&url, account).await
}

/// Why: Slide-level structural ops (add, delete, duplicate, reorder) share one tool.
/// What: Routes per-action requests to the Slides v1 `presentations:batchUpdate` endpoint.
/// Test: Live API.
pub async fn manage_slides(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    match action {
        "create_presentation" => {
            let title = opt_str(&args, "title").unwrap_or("Untitled Presentation");
            let body = json!({ "title": title });
            let url = format!("{SLIDES_API_BASE}/presentations");
            client.post(&url, body, account).await
        }
        "create_slide" => {
            let id = require_str(&args, "presentation_id")?;
            let layout = opt_str(&args, "layout").unwrap_or("BLANK");
            let body = json!({
                "requests": [{
                    "createSlide": {
                        "slideLayoutReference": { "predefinedLayout": layout }
                    }
                }]
            });
            let url = format!("{SLIDES_API_BASE}/presentations/{id}:batchUpdate");
            client.post(&url, body, account).await
        }
        "delete_slide" => {
            let id = require_str(&args, "presentation_id")?;
            let slide_id = require_str(&args, "slide_id")?;
            let body = json!({
                "requests": [{ "deleteObject": { "objectId": slide_id } }]
            });
            let url = format!("{SLIDES_API_BASE}/presentations/{id}:batchUpdate");
            client.post(&url, body, account).await
        }
        other => Err(anyhow!("unknown action for manage_slides: {other}")),
    }
}

/// Why: Adding text/shapes/images to a slide is the most common authoring op.
/// What: Builds typed Slides batchUpdate requests (`createShape`, `insertText`, ...) per mode.
/// Test: Live API.
pub async fn add_slide_content(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "presentation_id")?;
    let slide_id = require_str(&args, "slide_id")?;
    let text = require_str(&args, "text")?;
    let textbox_id = format!("textbox_{}", Uuid::new_v4().simple());

    let body = json!({
        "requests": [
            {
                "createShape": {
                    "objectId": textbox_id,
                    "shapeType": "TEXT_BOX",
                    "elementProperties": {
                        "pageObjectId": slide_id,
                        "size": {
                            "width": { "magnitude": 350, "unit": "PT" },
                            "height": { "magnitude": 100, "unit": "PT" },
                        },
                        "transform": {
                            "scaleX": 1,
                            "scaleY": 1,
                            "translateX": 50,
                            "translateY": 50,
                            "unit": "PT",
                        }
                    }
                }
            },
            {
                "insertText": {
                    "objectId": textbox_id,
                    "text": text,
                }
            }
        ]
    });
    let url = format!("{SLIDES_API_BASE}/presentations/{id}:batchUpdate");
    client.post(&url, body, account).await
}
