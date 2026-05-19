//! Integration tests for trusty-rpc transports.
//!
//! Why: validate that the stdio transport actually round-trips with a real
//! subprocess and that the HTTP transport round-trips with a real TCP listener.
//! What: spawns a small bash echo loop and a minimal tokio HTTP server, then
//! exercises locally-defined transport shims (the production bin crate has no
//! library surface, so we mirror just enough behaviour for end-to-end coverage).
//! Test: this file IS the test.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

mod support;

#[cfg(unix)]
fn echo_server_argv() -> (&'static str, Vec<&'static str>) {
    // Reads one line at a time, replies with a JSON-RPC success echoing the id.
    let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
  if [ -z "$id" ]; then
    continue
  fi
  printf '{"jsonrpc":"2.0","id":%s,"result":{"echo":true}}\n' "$id"
done"#;
    ("bash", vec!["-c", script])
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_transport_roundtrip() {
    let (program, args) = echo_server_argv();
    let transport = support::spawn_stdio(program, &args).await;
    let resp = transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .await
        .unwrap();
    assert_eq!(resp["result"]["echo"], json!(true));
    assert_eq!(resp["id"], json!(1));
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_transport_notification_no_read() {
    let (program, args) = echo_server_argv();
    let transport = support::spawn_stdio(program, &args).await;
    let resp = transport
        .send(json!({"jsonrpc": "2.0", "method": "notify"}))
        .await
        .unwrap();
    assert_eq!(resp, Value::Null);
}

#[tokio::test]
async fn http_transport_roundtrip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/");

    let handle = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(socket);

        // Read headers.
        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            if n == 0 {
                return;
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).await.unwrap();
        }
        let req: Value = serde_json::from_slice(&body).unwrap();
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let body_out = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"echo": true}
        }))
        .unwrap();
        let resp_head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body_out.len()
        );

        let mut socket = reader.into_inner();
        socket.write_all(resp_head.as_bytes()).await.unwrap();
        socket.write_all(&body_out).await.unwrap();
        socket.flush().await.unwrap();
        let _ = socket.shutdown().await;
    });

    let transport = support::http(&url);
    let resp = transport
        .send(json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}))
        .await
        .unwrap();
    assert_eq!(resp["result"]["echo"], json!(true));
    assert_eq!(resp["id"], json!(7));

    handle.await.unwrap();
}
