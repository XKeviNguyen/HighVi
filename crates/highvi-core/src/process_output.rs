use crate::PublicError;
use tokio::io::{AsyncRead, AsyncReadExt};

pub struct CappedOutput {
    pub bytes: Vec<u8>,
    pub overflowed: bool,
}

pub async fn read_capped<R>(mut reader: R, limit: usize) -> Result<CappedOutput, PublicError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut overflowed = false;
    let mut chunk = [0_u8; 8 * 1024];

    loop {
        let bytes_read = reader.read(&mut chunk).await.map_err(|_| {
            PublicError::new(
                "process_output_failed",
                "HighVi could not read output from the media downloader.",
            )
        })?;
        if bytes_read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let to_keep = remaining.min(bytes_read);
        bytes.extend_from_slice(&chunk[..to_keep]);
        overflowed |= to_keep < bytes_read;
    }

    Ok(CappedOutput { bytes, overflowed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_unbroken_output_is_drained_without_growing_the_buffer() {
        let input = vec![b'x'; 64 * 1024];
        let output = read_capped(input.as_slice(), 1024).await.unwrap();
        assert!(output.overflowed);
        assert_eq!(output.bytes.len(), 1024);
        assert!(output.bytes.iter().all(|byte| *byte == b'x'));
    }

    #[tokio::test]
    async fn in_limit_output_is_preserved() {
        let output = read_capped(b"metadata".as_slice(), 1024).await.unwrap();
        assert!(!output.overflowed);
        assert_eq!(output.bytes, b"metadata");
    }
}
