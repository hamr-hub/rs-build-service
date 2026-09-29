//! 极简 text/event-stream 客户端：把 SSE 解析成 (event, data) 帧。

use anyhow::Result;
use futures::{Stream, StreamExt};
use reqwest::Response;

/// 一帧 SSE 消息。
#[derive(Debug, Clone)]
pub struct Frame {
    pub event: String,
    pub data: String,
}

/// 解析 reqwest 响应体为 Frame 流。
pub fn frames(response: Response) -> impl Stream<Item = Result<Frame>> {
    let mut parser = Parser::default();
    response.bytes_stream().flat_map(move |chunk| {
        let mut out = Vec::new();
        match chunk {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                for line in text.split('\n') {
                    if let Some(frame) = parser.line(line) {
                        out.push(Ok(frame));
                    }
                }
            }
            Err(e) => out.push(Err(anyhow::anyhow!("stream error: {e}"))),
        }
        futures::stream::iter(out)
    })
}

#[derive(Default)]
struct Parser {
    event: Option<String>,
    data: Option<String>,
}

impl Parser {
    /// 喂入一行（含可能的 \r）；消息结束（空行）时产出 Frame。
    fn line(&mut self, raw: &str) -> Option<Frame> {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            return self.take();
        }
        if let Some(value) = line.strip_prefix("event:") {
            self.event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            self.data = Some(value.trim().to_string());
        }
        None
    }

    fn take(&mut self) -> Option<Frame> {
        if self.event.is_none() && self.data.is_none() {
            return None;
        }
        let frame = Frame {
            event: self.event.take().unwrap_or_else(|| "message".to_string()),
            data: self.data.take().unwrap_or_default(),
        };
        Some(frame)
    }
}
