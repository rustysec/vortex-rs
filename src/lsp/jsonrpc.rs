//! Minimal JSON-RPC 2.0 framing used by the language client.
//!
//! LSP messages are `Content-Length`-headed JSON payloads over stdio.  This
//! module only knows about framing and the three message shapes; everything
//! language-specific lives in the parent module.

use std::io::ErrorKind;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A request: has an id and expects a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: i64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A notification: no id, no response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A response to a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Build a request envelope.
pub fn request(id: i64, method: &str, params: Option<Value>) -> Request {
    Request {
        jsonrpc: "2.0".into(),
        id,
        method: method.to_string(),
        params,
    }
}

/// Build a notification envelope.
pub fn notification(method: &str, params: Option<Value>) -> Notification {
    Notification {
        jsonrpc: "2.0".into(),
        method: method.to_string(),
        params,
    }
}

/// Serialize a message with its `Content-Length` header.
pub fn encode(message: &impl Serialize) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap_or_else(|_| b"{}".to_vec());
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut out = Vec::with_capacity(header.len() + body.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&body);
    out
}

/// Read one message, returning the parsed JSON value.
///
/// Returns `Ok(None)` at a clean end of stream.  Unknown headers are ignored, as
/// the spec requires.
pub async fn read_message<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    loop {
        let line = read_header_line(reader).await?;
        let Some(line) = line else {
            return Ok(None);
        };
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = length else {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "message without Content-Length",
        ));
    };
    if length > 64 * 1024 * 1024 {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("implausibly large message ({length} bytes)"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    match serde_json::from_slice::<Value>(&body) {
        Ok(value) => Ok(Some(value)),
        Err(error) => Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("malformed JSON payload: {error}"),
        )),
    }
}

/// Read one CRLF-terminated header line; an empty line ends the header block.
async fn read_header_line<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = reader.read(&mut byte).await?;
        if read == 0 {
            return Ok(if line.is_empty() {
                None
            } else {
                Some(finish(line))
            });
        }
        match byte[0] {
            b'\n' => return Ok(Some(finish(line))),
            b'\r' => {
                // Consume the LF of a CRLF pair, if it follows.
                let peeked = reader.read(&mut byte).await?;
                if peeked == 0 || byte[0] != b'\n' {
                    return Ok(Some(finish(line)));
                }
                return Ok(Some(finish(line)));
            }
            other => line.push(other),
        }
    }
}

fn finish(line: Vec<u8>) -> String {
    String::from_utf8_lossy(&line).trim().to_string()
}

/// Write a message to a process' stdin.
pub async fn write_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    message: &impl Serialize,
) -> std::io::Result<()> {
    writer.write_all(&encode(message)).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_with_a_content_length_header() {
        let encoded = encode(&notification("initialized", None));
        let text = String::from_utf8(encoded).unwrap();
        assert!(text.starts_with("Content-Length: "), "{text}");
        assert!(text.contains("\r\n\r\n"), "{text}");
        assert!(text.contains("\"method\":\"initialized\""), "{text}");
    }

    #[test]
    fn omits_empty_params() {
        let encoded = String::from_utf8(encode(&notification("x/y", None))).unwrap();
        assert!(!encoded.contains("params"), "{encoded}");
    }

    #[tokio::test]
    async fn reads_a_message_back() {
        let bytes = encode(&notification("window/logMessage", None));
        let mut cursor = std::io::Cursor::new(bytes);
        let message = read_message(&mut cursor).await.unwrap().expect("a message");
        assert_eq!(message["method"], "window/logMessage");
    }

    #[tokio::test]
    async fn reads_two_messages_in_a_row() {
        let mut bytes = encode(&notification("a/b", None));
        bytes.extend(encode(&notification("c/d", None)));
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(
            read_message(&mut cursor).await.unwrap().unwrap()["method"],
            "a/b"
        );
        assert_eq!(
            read_message(&mut cursor).await.unwrap().unwrap()["method"],
            "c/d"
        );
        assert!(
            read_message(&mut cursor).await.unwrap().is_none(),
            "stream ends"
        );
    }

    #[tokio::test]
    async fn rejects_a_message_without_a_length() {
        let mut cursor = std::io::Cursor::new(b"\r\n{}".to_vec());
        let error = read_message(&mut cursor).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn ignores_unknown_headers() {
        let payload = b"Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let mut cursor = std::io::Cursor::new(payload.to_vec());
        assert!(read_message(&mut cursor).await.unwrap().is_some());
    }
}
