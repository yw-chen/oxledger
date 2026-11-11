// SPDX-License-Identifier: GPL-2.0-or-later
//! Server-sent events from an AI provider (design 05 §3.13.2 P-7): line by
//! line `event:` / `data:`, a blank line ends an event, `:` lines are
//! comments. Bytes arrive in arbitrary chunks.

/// One event: its `event:` name (if any) and its `data:` lines joined by `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// A line longer than this is not SSE we accept.
pub const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes; returns the events completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, Malformed> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let Some(nl) = self.buf.iter().position(|b| *b == b'\n') else {
                if self.buf.len() > MAX_LINE {
                    return Err(Malformed);
                }
                return Ok(out);
            };
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8(line).map_err(|_| Malformed)?;
            if let Some(e) = self.line(&line) {
                out.push(e);
            }
        }
    }

    /// The stream ended: an event still open (no blank line yet) is
    /// delivered (servers often omit the last blank line).
    pub fn finish(&mut self) -> Result<Option<SseEvent>, Malformed> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let line = String::from_utf8(rest).map_err(|_| Malformed)?;
            let line = line.trim_end_matches('\r').to_string();
            if let Some(e) = self.line(&line) {
                return Ok(Some(e));
            }
        }
        Ok(self.dispatch())
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {} // id, retry, unknown
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut p = SseParser::new();
        let mut v = Vec::new();
        for c in chunks {
            v.extend(p.push(c).unwrap());
        }
        v.extend(p.finish().unwrap());
        v
    }

    #[test]
    fn events_across_chunks() {
        let v = all(&[
            b"event: message_start\r\nda",
            b"ta: {\"a\":1}\r\n\r\n: ping\n\nevent: x\ndata: one\ndata:two\n\n",
            b"data: [DONE]",
        ]);
        assert_eq!(
            v,
            vec![
                SseEvent {
                    event: Some("message_start".into()),
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    event: Some("x".into()),
                    data: "one\ntwo".into()
                },
                SseEvent {
                    event: None,
                    data: "[DONE]".into()
                },
            ]
        );
    }

    #[test]
    fn utf8_split_inside_a_character() {
        let s = "data: 中文\n\n".as_bytes();
        let v = all(&[&s[..7], &s[7..]]);
        assert_eq!(v[0].data, "中文");
    }

    #[test]
    fn overlong_line_is_malformed() {
        let mut p = SseParser::new();
        assert_eq!(p.push(&vec![b'a'; MAX_LINE + 1]), Err(Malformed));
    }

    #[test]
    fn event_without_data_is_dropped() {
        assert!(all(&[b"event: ping\n\n"]).is_empty());
    }
}
