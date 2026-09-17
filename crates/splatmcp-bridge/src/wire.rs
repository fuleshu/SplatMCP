//! One-JSON-object-per-line framing.
//!
//! Both sides are on loopback, but a captured frame can be several megabytes of
//! base64, so reads are bounded explicitly instead of trusting the peer.

use std::io::{BufRead, ErrorKind, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{BridgeError, Result};

/// Largest single frame accepted in either direction. A 4K PNG capture is a few
/// megabytes, so this leaves generous headroom while still refusing runaway input.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Serialises a message and writes it as one line.
pub fn write_message<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<()> {
    let mut line = serde_json::to_vec(message)
        .map_err(|error| BridgeError::Protocol(format!("could not encode message: {error}")))?;
    if line.len() > MAX_FRAME_BYTES {
        return Err(BridgeError::FrameTooLarge {
            max: MAX_FRAME_BYTES,
        });
    }
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

/// Reads one line, refusing to buffer more than `max` bytes.
///
/// Returns `Ok(None)` at end of input, which is how both sides notice that the
/// other end went away.
pub fn read_line_limited<R: BufRead>(reader: &mut R, max: usize) -> Result<Option<String>> {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(BridgeError::Io(error)),
        };
        if available.is_empty() {
            if buffer.is_empty() {
                return Ok(None);
            }
            break;
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(position) => {
                if buffer.len() + position > max {
                    return Err(BridgeError::FrameTooLarge { max });
                }
                buffer.extend_from_slice(&available[..position]);
                reader.consume(position + 1);
                break;
            }
            None => {
                let length = available.len();
                if buffer.len() + length > max {
                    return Err(BridgeError::FrameTooLarge { max });
                }
                buffer.extend_from_slice(available);
                reader.consume(length);
            }
        }
    }
    let text = String::from_utf8(buffer)
        .map_err(|_| BridgeError::Protocol("frame is not valid UTF-8".to_owned()))?;
    Ok(Some(text))
}

/// Reads one line and decodes it as JSON.
pub fn read_message<R: BufRead, T: DeserializeOwned>(reader: &mut R) -> Result<Option<T>> {
    let Some(line) = read_line_limited(reader, MAX_FRAME_BYTES)? else {
        return Ok(None);
    };
    if line.trim().is_empty() {
        return Ok(None);
    }
    let value: T = serde_json::from_str(&line)
        .map_err(|error| BridgeError::Protocol(format!("could not decode message: {error}")))?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Method, Request, Response};
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn a_request_survives_a_round_trip() {
        let request = Request::new(4, "token", Method::ViewerCapture, json!({"width": 640}));
        let mut bytes = Vec::new();
        write_message(&mut bytes, &request).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);

        let mut cursor = Cursor::new(bytes);
        let decoded: Request = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded.id, 4);
        assert_eq!(decoded.method, Method::ViewerCapture);
        assert_eq!(decoded.params["width"], 640);
        assert!(read_message::<_, Request>(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn several_messages_are_read_in_order() {
        let mut bytes = Vec::new();
        for id in 0..3u64 {
            write_message(&mut bytes, &Response::success(id, json!({"id": id}))).unwrap();
        }
        let mut cursor = Cursor::new(bytes);
        for id in 0..3u64 {
            let response: Response = read_message(&mut cursor).unwrap().unwrap();
            assert_eq!(response.id, id);
            assert_eq!(response.result["id"], id);
        }
    }

    #[test]
    fn a_long_line_is_refused_before_it_is_buffered() {
        let mut payload = vec![b'x'; 4096];
        payload.push(b'\n');
        let error = read_line_limited(&mut Cursor::new(payload), 64).unwrap_err();
        assert!(matches!(error, BridgeError::FrameTooLarge { .. }));

        // The same applies when the terminator never arrives.
        let error = read_line_limited(&mut Cursor::new(vec![b'y'; 4096]), 64).unwrap_err();
        assert!(matches!(error, BridgeError::FrameTooLarge { .. }));
    }

    #[test]
    fn malformed_json_is_a_protocol_error() {
        let mut cursor = Cursor::new(b"{\"id\": 1, \"method\": nope}\n".to_vec());
        let error = read_message::<_, Request>(&mut cursor).unwrap_err();
        assert!(matches!(error, BridgeError::Protocol(_)));

        let mut cursor = Cursor::new(b"{\"id\": 1, \"method\": \"unknown_method\"}\n".to_vec());
        assert!(read_message::<_, Request>(&mut cursor).is_err());
    }

    #[test]
    fn a_trailing_line_without_newline_is_still_read() {
        let mut cursor = Cursor::new(b"{\"id\":9,\"ok\":true,\"result\":null}".to_vec());
        let response: Response = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(response.id, 9);
    }
}
