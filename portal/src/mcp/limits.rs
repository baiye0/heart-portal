use anyhow::Result;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(crate) const MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const WORK_REQUESTS: usize = 32;
pub(crate) const MANAGEMENT_REQUESTS: usize = 8;
pub(crate) const PROCESS_GENERATIONS: usize = 32;
pub(crate) const KIT_GENERATIONS: usize = 2;
pub(super) const PENDING_REQUESTS: usize = 16;

/// Bound allocation before copying a line, including a stream without newlines.
pub(super) async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
) -> Result<usize> {
    line.clear();
    read_line_append(reader, line, limit).await
}

/// Cancellation-safe variant for multiplexed connections: preserve partial input.
pub(crate) async fn read_line_append<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
) -> Result<usize> {
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok(line.len());
        }
        let newline = bytes.iter().position(|b| *b == b'\n');
        let count = newline.map_or(bytes.len(), |position| position + 1);
        anyhow::ensure!(
            count <= limit.saturating_sub(line.len()),
            "MCP output exceeds the message size limit"
        );
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(line.len());
        }
    }
}

pub(super) struct OutputBudget {
    since: Instant,
    bytes: usize,
    lines: usize,
    max_bytes: usize,
}
impl OutputBudget {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            since: Instant::now(),
            bytes: 0,
            lines: 0,
            max_bytes,
        }
    }
    pub fn consume(&mut self, bytes: usize) -> Result<()> {
        if self.since.elapsed() >= Duration::from_secs(5) {
            self.since = Instant::now();
            self.bytes = 0;
            self.lines = 0;
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.lines += 1;
        anyhow::ensure!(
            self.bytes <= self.max_bytes && self.lines <= 2048,
            "MCP output rate exceeds the limit"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn partial_message_survives_cancellation_without_exceeding_limit() {
        let (mut peer, stream) = tokio::io::duplex(64);
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();
        peer.write_all(b"part").await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            read_line_append(&mut reader, &mut bytes, 16)
        )
        .await
        .is_err());
        peer.write_all(b"ial\n").await.unwrap();
        read_line_append(&mut reader, &mut bytes, 16).await.unwrap();
        assert_eq!(bytes, b"partial\n");
        peer.write_all(b"123456789").await.unwrap();
        assert!(read_line(&mut reader, &mut bytes, 8).await.is_err());
        assert!(bytes.len() <= 8);
    }
}
