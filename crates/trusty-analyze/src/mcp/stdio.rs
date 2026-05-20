//! Line-delimited JSON-RPC 2.0 over stdin/stdout.
//!
//! Why: MCP clients (Claude Code, Inspector) launch the server as a subprocess
//! and exchange one JSON object per line. Notification responses are silently
//! dropped. Parse errors are reported with id=null per the JSON-RPC spec.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::{error_codes, AnalyzerMcpServer, JsonRpcError, Request, Response};

/// Run the stdio loop until stdin closes. Each line is parsed as a JSON-RPC
/// request, dispatched, and the (optional) response written back.
pub async fn run(server: AnalyzerMcpServer) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resp: Response = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => server.dispatch(req).await,
            Err(e) => Response {
                jsonrpc: "2.0".into(),
                id: serde_json::Value::Null,
                result: None,
                error: Some(JsonRpcError {
                    code: error_codes::INVALID_REQUEST,
                    message: format!("parse error: {e}"),
                    data: None,
                }),
                suppress: false,
            },
        };
        if resp.suppress {
            continue;
        }
        let mut bytes = serde_json::to_vec(&resp)?;
        bytes.push(b'\n');
        stdout.write_all(&bytes).await?;
        stdout.flush().await?;
    }
    Ok(())
}
