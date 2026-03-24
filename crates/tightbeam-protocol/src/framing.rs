use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024; // 16 MiB

/// Reads a length-prefixed frame: [4-byte u32 BE length][payload].
/// Returns `Ok(None)` on clean EOF (disconnect at frame boundary).
pub async fn read_frame(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<Option<Vec<u8>>, std::io::Error> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

/// Reads the 4-byte u32 BE length header of a frame.
/// Use with manual payload reads for streaming large binary data.
pub async fn read_frame_header(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<u32, std::io::Error> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf).await?;
    Ok(u32::from_be_bytes(buf))
}

/// Writes the 4-byte u32 BE length header of a frame.
/// Use with manual payload writes for streaming large binary data.
pub async fn write_frame_header(
    writer: &mut (impl AsyncWriteExt + Unpin),
    len: u32,
) -> Result<(), std::io::Error> {
    writer.write_all(&len.to_be_bytes()).await?;
    Ok(())
}

/// Writes a length-prefixed frame: [4-byte u32 BE length][payload].
pub async fn write_frame(
    writer: &mut (impl AsyncWriteExt + Unpin),
    payload: &[u8],
) -> Result<(), std::io::Error> {
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_frame() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _) = tokio::io::split(server);
        let (_, mut writer) = tokio::io::split(client);

        let payload = b"hello world";
        write_frame(&mut writer, payload).await.unwrap();
        drop(writer);

        let result = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(result, payload);
    }

    #[tokio::test]
    async fn empty_reader_returns_none() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _) = tokio::io::split(server);
        drop(client);

        let result = read_frame(&mut reader).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn empty_payload_round_trips() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _) = tokio::io::split(server);
        let (_, mut writer) = tokio::io::split(client);

        write_frame(&mut writer, b"").await.unwrap();
        drop(writer);

        let result = read_frame(&mut reader).await.unwrap().unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn multiple_frames_round_trip() {
        let (client, server) = tokio::io::duplex(4096);
        let (mut reader, _) = tokio::io::split(server);
        let (_, mut writer) = tokio::io::split(client);

        write_frame(&mut writer, b"first").await.unwrap();
        write_frame(&mut writer, b"second").await.unwrap();
        write_frame(&mut writer, b"third").await.unwrap();
        drop(writer);

        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"first");
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"second");
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"third");
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _) = tokio::io::split(server);
        let (_, mut writer) = tokio::io::split(client);

        // Write a length prefix exceeding MAX_FRAME_SIZE
        let huge_len: u32 = MAX_FRAME_SIZE + 1;
        writer.write_all(&huge_len.to_be_bytes()).await.unwrap();
        drop(writer);

        let err = read_frame(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("frame too large"));
    }

    #[tokio::test]
    async fn partial_header_eof_returns_none() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _) = tokio::io::split(server);
        let (_, mut writer) = tokio::io::split(client);

        // Write only 2 of 4 length-prefix bytes, then disconnect
        writer.write_all(&[0x00, 0x05]).await.unwrap();
        drop(writer);

        // read_exact on the 4-byte header gets UnexpectedEof → Ok(None)
        let result = read_frame(&mut reader).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn incomplete_payload_returns_error() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _) = tokio::io::split(server);
        let (_, mut writer) = tokio::io::split(client);

        // Write valid header claiming 10 bytes, but only send 5
        let len: u32 = 10;
        writer.write_all(&len.to_be_bytes()).await.unwrap();
        writer.write_all(&[0u8; 5]).await.unwrap();
        drop(writer);

        // Header read_exact succeeds, but payload read_exact gets UnexpectedEof → Err
        let err = read_frame(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn frame_header_round_trips() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _) = tokio::io::split(server);
        let (_, mut writer) = tokio::io::split(client);

        write_frame_header(&mut writer, 42).await.unwrap();
        write_frame_header(&mut writer, 0).await.unwrap();
        write_frame_header(&mut writer, 16_777_216).await.unwrap();
        drop(writer);

        assert_eq!(read_frame_header(&mut reader).await.unwrap(), 42);
        assert_eq!(read_frame_header(&mut reader).await.unwrap(), 0);
        assert_eq!(read_frame_header(&mut reader).await.unwrap(), 16_777_216);
    }
}
