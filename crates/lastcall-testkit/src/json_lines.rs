//! A small synchronous newline-JSON reader over `std::os::unix::net::UnixStream`, adapted from
//! herdr's `JsonLineReader` / `send_request` / `open_subscription` (`tests/api_ping.rs`,
//! Apache-2.0; see NOTICE). Used by the real-herdr integration test to capture raw wire lines
//! (for fixture recording) independently of the engine's async transport.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

/// A connected socket with a line buffer.
pub struct JsonLineReader {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl JsonLineReader {
    pub fn connect(socket_path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(socket_path)?,
            buf: Vec::new(),
        })
    }

    /// Write one line (a newline is appended).
    pub fn send_line(&mut self, json: &str) -> std::io::Result<()> {
        self.stream.write_all(json.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()
    }

    /// The next raw line without its newline. `Ok(None)` on timeout; `Err` on a closed
    /// stream (`UnexpectedEof`) or I/O failure.
    pub fn read_raw_line(&mut self, timeout: Duration) -> std::io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line)
                    .trim_end_matches(['\n', '\r'])
                    .to_string();
                if text.trim().is_empty() {
                    continue;
                }
                return Ok(Some(text));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            // macOS answers EINVAL to SO_RCVTIMEO once the peer has closed the socket; the
            // following read then reports EOF, which is the answer the caller wants.
            if let Err(err) = self
                .stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                && err.kind() != std::io::ErrorKind::InvalidInput
            {
                return Err(err);
            }
            let mut bytes = [0u8; 4096];
            match self.stream.read(&mut bytes) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "stream closed while waiting for a line",
                    ));
                }
                Ok(n) => self.buf.extend_from_slice(&bytes[..n]),
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// The next line parsed as JSON. `Ok(None)` on timeout.
    pub fn read_json_line(&mut self, timeout: Duration) -> std::io::Result<Option<Value>> {
        match self.read_raw_line(timeout)? {
            Some(line) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            None => Ok(None),
        }
    }
}

/// One-shot: connect, send, read one raw line, close. `Ok(None)` on timeout.
pub fn send_request_raw(
    socket_path: &Path,
    json: &str,
    timeout: Duration,
) -> std::io::Result<Option<String>> {
    let mut reader = JsonLineReader::connect(socket_path)?;
    reader.send_line(json)?;
    reader.read_raw_line(timeout)
}

/// One-shot: connect, send, read one JSON line, close. `Ok(None)` on timeout.
pub fn send_request(
    socket_path: &Path,
    json: &str,
    timeout: Duration,
) -> std::io::Result<Option<Value>> {
    let mut reader = JsonLineReader::connect(socket_path)?;
    reader.send_line(json)?;
    reader.read_json_line(timeout)
}

/// Persistent: connect and send; the caller reads the ack and then the stream.
pub fn open_subscription(socket_path: &Path, json: &str) -> std::io::Result<JsonLineReader> {
    let mut reader = JsonLineReader::connect(socket_path)?;
    reader.send_line(json)?;
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn json_lines_reads_lines_across_chunks_and_reports_eof() {
        let dir = crate::tmp::TempDir::socket_dir();
        let path = dir.join("j.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut first = [0u8; 64];
            let n = conn.read(&mut first).unwrap();
            assert!(String::from_utf8_lossy(&first[..n]).contains("\"ping\""));
            conn.write_all(b"{\"id\":\"a\",\"result\":{\"type\":\"po")
                .unwrap();
            conn.flush().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            conn.write_all(b"ng\"}}\n\n{\"event\":\"x\",\"data\":{}}\n")
                .unwrap();
            conn.flush().unwrap();
        });
        let mut reader =
            open_subscription(&path, r#"{"id":"a","method":"ping","params":{}}"#).unwrap();
        let first = reader
            .read_json_line(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(first["result"]["type"], "pong");
        let second = reader
            .read_raw_line(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(second, r#"{"event":"x","data":{}}"#);
        server.join().unwrap();
        let err = reader
            .read_raw_line(Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn json_lines_times_out_with_none() {
        let dir = crate::tmp::TempDir::socket_dir();
        let path = dir.join("t.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            // Under the unit tier's 50 ms budget (docs/dev/testing.md).
            std::thread::sleep(Duration::from_millis(40));
            drop(conn);
        });
        let mut reader = JsonLineReader::connect(&path).unwrap();
        assert!(
            reader
                .read_raw_line(Duration::from_millis(15))
                .unwrap()
                .is_none()
        );
        server.join().unwrap();
    }
}
