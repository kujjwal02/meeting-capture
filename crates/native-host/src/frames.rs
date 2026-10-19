//! Native messaging frames: a 32-bit length, then that many bytes of UTF-8 JSON.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// The largest frame either side accepts. Chrome takes at most 1 MB from a host.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// The byte order of a frame's length.
#[derive(Debug, Clone, Copy)]
pub enum LengthOrder {
    /// Chrome's side: the machine's own order.
    Native,
    /// The app socket's side, the same on every machine.
    Little,
}

/// Reads one frame. Returns `None` when the stream ends between frames.
///
/// # Errors
///
/// Returns the read error, `UnexpectedEof` for a stream that ends inside a frame, and
/// `InvalidData` for a frame over `MAX_FRAME_BYTES`.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    order: LengthOrder,
) -> io::Result<Option<Vec<u8>>> {
    let mut length_bytes = [0u8; 4];
    match reader.read_exact(&mut length_bytes).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = match order {
        LengthOrder::Native => u32::from_ne_bytes(length_bytes),
        LengthOrder::Little => u32::from_le_bytes(length_bytes),
    };
    let length = usize::try_from(length).map_err(io::Error::other)?;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {length} bytes is over the {MAX_FRAME_BYTES} byte limit"),
        ));
    }
    let mut frame = vec![0u8; length];
    reader.read_exact(&mut frame).await?;
    Ok(Some(frame))
}

/// Writes one frame and flushes it.
///
/// # Errors
///
/// Returns the write error, and `InvalidData` for a frame over `MAX_FRAME_BYTES`.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    order: LengthOrder,
    frame: &[u8],
) -> io::Result<()> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame of {} bytes is over the {MAX_FRAME_BYTES} byte limit",
                frame.len()
            ),
        ));
    }
    let length = u32::try_from(frame.len()).map_err(io::Error::other)?;
    let length_bytes = match order {
        LengthOrder::Native => length.to_ne_bytes(),
        LengthOrder::Little => length.to_le_bytes(),
    };
    writer.write_all(&length_bytes).await?;
    writer.write_all(frame).await?;
    writer.flush().await
}
