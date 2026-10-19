//! Byte framing shared by providers using Server-Sent Events.
//!
//! Only `data:` payloads are exposed; event names, IDs, retry hints, and comments
//! are ignored. Providers interpret JSON, completion signals, errors, and EOF.
//! Framing stays independent of HTTP clients, async runtimes, and model types.

use std::collections::VecDeque;

/// Frame bytes before UTF-8 decoding: HTTP chunks need not end at a character
/// boundary. CR, LF and CRLF are line endings, including across chunks.
#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    skip_lf: bool,
    frames: VecDeque<Vec<u8>>,
    first_line: bool,
}

impl Decoder {
    /// Removes the next complete event's data payload, without decoding UTF-8.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        self.frames.pop_front()
    }

    /// Buffers arbitrary HTTP body chunks and queues blank-line-terminated data.
    /// An unfinished event remains buffered: EOF is interpreted by the caller.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                let mut line = std::mem::take(&mut self.line);
                if !self.first_line {
                    self.first_line = true;
                    if line.starts_with(&[0xef, 0xbb, 0xbf]) {
                        line.drain(..3);
                    }
                }
                if line.is_empty() {
                    if !self.data.is_empty() {
                        self.data.pop();
                        self.frames.push_back(std::mem::take(&mut self.data));
                    }
                } else if line.starts_with(b"data:") {
                    let value = line[5..].strip_prefix(b" ").unwrap_or(&line[5..]);
                    self.data.extend_from_slice(value);
                    self.data.push(b'\n');
                }
            } else {
                self.line.push(byte);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decoder_handles_every_byte_boundary_and_line_ending() {
        let input = "\u{feff}:ignore\r\nevent: message\rdata: {\"value\":\r\ndata: \"考\"}\n\ndata: [DONE]\r\r";
        let mut decoder = Decoder::default();
        for byte in input.as_bytes() {
            decoder.feed(&[*byte]);
        }
        assert_eq!(
            decoder.next_frame().unwrap(),
            "{\"value\":\n\"考\"}".as_bytes()
        );
        assert_eq!(decoder.next_frame().unwrap(), b"[DONE]");
        assert!(decoder.next_frame().is_none());
    }
}
