use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const JSON: u8 = 0x00;

const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub enum Frame {
    Json(Value),
    /// A frame whose length and type were fine and whose JSON was not. The link survives it: one
    /// unreadable message from the server is not a reason to drop every call in flight.
    Unreadable(String),
    /// Anything the protocol does not send in this direction.
    Other(u8),
}

/// One frame, or None when the connection closed between frames.
pub async fn read(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<Frame>> {
    let length = match reader.read_u32().await {
        Ok(length) => length,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    };
    if length == 0 || length > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a frame of {length} bytes, outside 1..={MAX_FRAME}"),
        ));
    }

    let mut body = vec![0u8; length as usize];
    reader.read_exact(&mut body).await?;

    Ok(Some(match body[0] {
        JSON => match serde_json::from_slice(&body[1..]) {
            Ok(value) => Frame::Json(value),
            Err(error) => Frame::Unreadable(error.to_string()),
        },
        other => Frame::Other(other),
    }))
}

pub fn encode(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.extend_from_slice(&((payload.len() as u32 + 1).to_be_bytes()));
    frame.push(kind);
    frame.extend_from_slice(payload);
    frame
}

pub fn json(message: &Value) -> Vec<u8> {
    encode(JSON, message.to_string().as_bytes())
}
