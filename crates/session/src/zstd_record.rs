//! P5 / TASK-406：zstd 物理记录提交边界、checksum 与失败回滚。

use std::fs::File;
use std::io::{self, Cursor, Write};

pub(super) const NEW_FORMAT_MAGIC: [u8; 4] = *b"IHFR";
const COMMIT_MAGIC: [u8; 4] = *b"IHCM";
const BOUNDARY_BYTES: usize = 12;
const DEFAULT_LEVEL: i32 = 3;

pub(super) struct RecordScan {
    pub records: Vec<Vec<u8>>,
    pub valid_len: usize,
    pub torn_tail: bool,
}

pub(super) fn persist_payload(file: &mut File, payload: &[u8]) -> io::Result<()> {
    persist_record(file, &encode_record(payload)?)
}

pub(super) fn encode_record(payload: &[u8]) -> io::Result<Vec<u8>> {
    let frame = encode_checked(payload)?;
    let frame_len = u64::try_from(frame.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "zstd frame too large"))?;
    let mut record = Vec::with_capacity(BOUNDARY_BYTES * 2 + frame.len());
    record.extend_from_slice(&NEW_FORMAT_MAGIC);
    record.extend_from_slice(&frame_len.to_le_bytes());
    record.extend_from_slice(&frame);
    record.extend_from_slice(&COMMIT_MAGIC);
    record.extend_from_slice(&frame_len.to_le_bytes());
    Ok(record)
}

pub(super) fn scan_records(bytes: &[u8]) -> io::Result<RecordScan> {
    let mut offset = 0;
    let mut records = Vec::new();
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        if remaining.len() < BOUNDARY_BYTES {
            return Ok(RecordScan {
                records,
                valid_len: offset,
                torn_tail: true,
            });
        }
        if remaining[..4] != NEW_FORMAT_MAGIC {
            return Err(invalid_data("corrupt zstd record prefix"));
        }
        let frame_len = read_len(&remaining[4..12])?;
        let total = BOUNDARY_BYTES
            .checked_add(frame_len)
            .and_then(|value| value.checked_add(BOUNDARY_BYTES))
            .ok_or_else(|| invalid_data("zstd record length overflow"))?;
        if remaining.len() < total {
            return Ok(RecordScan {
                records,
                valid_len: offset,
                torn_tail: true,
            });
        }
        let trailer = BOUNDARY_BYTES + frame_len;
        if remaining[trailer..trailer + 4] != COMMIT_MAGIC
            || read_len(&remaining[trailer + 4..trailer + 12])? != frame_len
        {
            return Err(invalid_data("corrupt zstd record commit boundary"));
        }
        let frame = &remaining[BOUNDARY_BYTES..trailer];
        let discovered = zstd::zstd_safe::find_frame_compressed_size(frame)
            .map_err(|_| invalid_data("corrupt zstd frame"))?;
        if discovered != frame_len {
            return Err(invalid_data("zstd frame length mismatch"));
        }
        records.push(
            zstd::stream::decode_all(Cursor::new(frame))
                .map_err(|_| invalid_data("zstd frame checksum mismatch"))?,
        );
        offset += total;
    }
    Ok(RecordScan {
        records,
        valid_len: offset,
        torn_tail: false,
    })
}

fn encode_checked(payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), DEFAULT_LEVEL)?;
    encoder.include_checksum(true)?;
    encoder.write_all(payload)?;
    encoder.finish()
}

trait DurableSink {
    fn current_len(&self) -> io::Result<u64>;
    fn append_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

impl DurableSink for File {
    fn current_len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn append_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.set_len(len)
    }
}

fn persist_record(sink: &mut dyn DurableSink, record: &[u8]) -> io::Result<()> {
    let start = sink.current_len()?;
    let result = sink.append_all(record).and_then(|()| sink.sync());
    if let Err(error) = result {
        sink.truncate(start)
            .and_then(|()| sink.sync())
            .map_err(|rollback| {
                io::Error::other(format!(
                    "zstd append failed ({error}); rollback failed ({rollback})"
                ))
            })?;
        return Err(error);
    }
    Ok(())
}

fn read_len(bytes: &[u8]) -> io::Result<usize> {
    let raw = u64::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| invalid_data("invalid zstd record length"))?,
    );
    usize::try_from(raw).map_err(|_| invalid_data("zstd record length exceeds platform"))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FailingSink {
        bytes: Vec<u8>,
        fail_sync_once: bool,
        truncations: Vec<u64>,
    }

    impl DurableSink for FailingSink {
        fn current_len(&self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn append_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.bytes.extend_from_slice(bytes);
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            if self.fail_sync_once {
                self.fail_sync_once = false;
                return Err(io::Error::other("injected sync failure"));
            }
            Ok(())
        }

        fn truncate(&mut self, len: u64) -> io::Result<()> {
            self.truncations.push(len);
            self.bytes.truncate(len as usize);
            Ok(())
        }
    }

    #[test]
    fn sync_failure_rolls_back_to_the_prior_boundary() {
        let mut sink = FailingSink {
            bytes: b"prefix".to_vec(),
            fail_sync_once: true,
            truncations: Vec::new(),
        };
        let error = persist_record(&mut sink, b"record").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(sink.bytes, b"prefix");
        assert_eq!(sink.truncations, vec![6]);
    }

    #[test]
    fn record_roundtrip_is_checksummed_and_committed() {
        let record = encode_record(b"payload").unwrap();
        let scan = scan_records(&record).unwrap();
        assert_eq!(scan.records, vec![b"payload".to_vec()]);
        assert!(!scan.torn_tail);
        assert_eq!(scan.valid_len, record.len());
    }
}
