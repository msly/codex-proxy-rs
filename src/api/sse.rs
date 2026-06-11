use crate::api::trim_ascii;

#[derive(Debug, Default)]
pub(super) struct SseDataParser {
    line_buf: Vec<u8>,
    data_lines: Vec<Vec<u8>>,
}

impl SseDataParser {
    pub(super) fn push(&mut self, bytes: &[u8], mut on_payload: impl FnMut(&[u8])) {
        for byte in bytes {
            if *byte == b'\n' {
                self.flush_line(&mut on_payload);
            } else {
                self.line_buf.push(*byte);
            }
        }
    }

    pub(super) fn finish(&mut self, mut on_payload: impl FnMut(&[u8])) {
        if !self.line_buf.is_empty() {
            self.flush_line(&mut on_payload);
        }
        self.flush_event(&mut on_payload);
    }

    fn flush_line(&mut self, on_payload: &mut impl FnMut(&[u8])) {
        if self.line_buf.ends_with(b"\r") {
            self.line_buf.pop();
        }
        let line = trim_ascii(&self.line_buf);
        if line.is_empty() {
            self.flush_event(on_payload);
        } else if let Some(data) = line.strip_prefix(b"data:") {
            self.data_lines.push(trim_ascii(data).to_vec());
        }
        self.line_buf.clear();
    }

    fn flush_event(&mut self, on_payload: &mut impl FnMut(&[u8])) {
        if self.data_lines.is_empty() {
            return;
        }

        if self.data_lines.len() == 1 {
            emit_payload(&self.data_lines[0], on_payload);
        } else {
            let joined = self.data_lines.join(b"\n".as_slice());
            if serde_json::from_slice::<serde_json::Value>(&joined).is_ok() {
                emit_payload(&joined, on_payload);
            } else {
                for line in &self.data_lines {
                    emit_payload(line, on_payload);
                }
            }
        }
        self.data_lines.clear();
    }
}

fn emit_payload(payload: &[u8], on_payload: &mut impl FnMut(&[u8])) {
    let payload = trim_ascii(payload);
    if payload.is_empty() || payload == b"[DONE]" {
        return;
    }
    on_payload(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = SseDataParser::default();
        let mut out = Vec::new();
        for chunk in chunks {
            parser.push(chunk, |payload| {
                out.push(String::from_utf8_lossy(payload).into_owned());
            });
        }
        parser.finish(|payload| {
            out.push(String::from_utf8_lossy(payload).into_owned());
        });
        out
    }

    #[test]
    fn parser_emits_single_data_payloads() {
        let out = collect(&[b"event: x\ndata: {\"a\":1}\n\n"]);
        assert_eq!(out, vec![r#"{"a":1}"#]);
    }

    #[test]
    fn parser_joins_multiline_json_payloads() {
        let out = collect(&[b"data: {\"a\":\n", b"data: 1}\n\n"]);
        assert_eq!(out, vec!["{\"a\":\n1}"]);
    }

    #[test]
    fn parser_splits_multiline_non_json_payloads() {
        let out = collect(&[b"data: one\ndata: two\n\n"]);
        assert_eq!(out, vec!["one", "two"]);
    }

    #[test]
    fn parser_ignores_done_payloads() {
        let out = collect(&[b"data: [DONE]\n\n"]);
        assert!(out.is_empty());
    }

    #[test]
    fn parser_flushes_trailing_payload_without_blank_line() {
        let out = collect(&[b"data: {\"a\":1}"]);
        assert_eq!(out, vec![r#"{"a":1}"#]);
    }
}
