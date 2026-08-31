use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use serde::de::DeserializeOwned;

const READ_CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug)]
pub enum ScanOutcome<T> {
    /// The record was valid JSON and deserialized as the requested type.
    Parsed(T),
    /// The record was not valid JSON for the requested type.
    #[allow(dead_code)]
    Rejected(serde_json::Error),
}

/// Read-only scanner for newline-delimited JSON records, starting from the end.
pub struct ReverseJsonlScanner<R> {
    reader: R,
    next_chunk_end: u64,
    chunk_position: usize,
    chunk: Vec<u8>,
    record_reversed: Vec<u8>,
    max_record_bytes: Option<usize>,
    discarding_oversized_record: bool,
    reject_oversized_records: bool,
    max_records: Option<usize>,
    records_scanned: usize,
    at_scan_start: bool,
}

impl<R> ReverseJsonlScanner<R>
where
    R: Read + Seek,
{
    pub fn new(mut reader: R) -> io::Result<Self> {
        let next_chunk_end = reader.seek(SeekFrom::End(0))?;
        Self::new_at(reader, next_chunk_end)
    }

    /// Creates a reverse scanner whose logical end is the given byte offset.
    ///
    /// This lets callers scan a frozen JSONL prefix without reading records appended after that
    /// prefix was captured.
    pub fn new_at(mut reader: R, end_byte_offset: u64) -> io::Result<Self> {
        let file_len = reader.seek(SeekFrom::End(0))?;
        if end_byte_offset > file_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reverse JSONL scan end is past the file",
            ));
        }
        Ok(Self {
            reader,
            next_chunk_end: end_byte_offset,
            chunk_position: 0,
            chunk: vec![0; READ_CHUNK_SIZE],
            record_reversed: Vec::new(),
            max_record_bytes: None,
            discarding_oversized_record: false,
            reject_oversized_records: false,
            max_records: None,
            records_scanned: 0,
            at_scan_start: true,
        })
    }

    /// Skips records larger than the configured limit without buffering or parsing them.
    pub fn with_max_record_bytes(mut self, max_record_bytes: usize) -> Self {
        self.max_record_bytes = Some(max_record_bytes);
        self
    }

    /// Fails the scan when a record exceeds the configured byte limit instead of skipping it.
    pub fn with_oversized_records_rejected(mut self) -> Self {
        self.reject_oversized_records = true;
        self
    }

    /// Fails closed after scanning the configured number of physical JSONL records.
    ///
    /// Blank, malformed, and oversized records count toward this limit.
    pub fn with_max_records(mut self, max_records: usize) -> Self {
        self.max_records = Some(max_records);
        self
    }

    /// Returns the number of physical JSONL records inspected by this scanner.
    pub fn physical_records_scanned(&self) -> usize {
        self.records_scanned
    }

    /// Scans the next nonblank record.
    ///
    /// I/O failures are returned as [`Err`]. Invalid JSON records are returned as
    /// [`ScanOutcome::Rejected`], and the scanner remains usable.
    pub fn scan_next<T>(&mut self) -> io::Result<Option<ScanOutcome<T>>>
    where
        T: DeserializeOwned,
    {
        loop {
            if self.chunk_position == 0 {
                if self.next_chunk_end == 0 {
                    if self.discarding_oversized_record {
                        self.discarding_oversized_record = false;
                        self.note_physical_record()?;
                        if self.reject_oversized_records {
                            return Err(self.oversized_record_error());
                        }
                        return Ok(None);
                    }
                    return self.finish_record(/*at_eof*/ true);
                }

                let read_size = usize::try_from(self.next_chunk_end.min(READ_CHUNK_SIZE as u64))
                    .map_err(io::Error::other)?;
                self.next_chunk_end -= read_size as u64;
                self.reader.seek(SeekFrom::Start(self.next_chunk_end))?;
                self.reader.read_exact(&mut self.chunk[..read_size])?;
                self.chunk_position = read_size;
            }

            let chunk = &self.chunk[..self.chunk_position];
            if let Some(newline_position) = chunk.iter().rposition(|byte| *byte == b'\n') {
                let fragment = &chunk[newline_position + 1..];
                if !self.discarding_oversized_record {
                    if self.max_record_bytes.is_some_and(|max_record_bytes| {
                        self.record_reversed.len().saturating_add(fragment.len()) > max_record_bytes
                    }) {
                        self.record_reversed.clear();
                        self.discarding_oversized_record = true;
                    } else {
                        self.record_reversed.extend(fragment.iter().rev().copied());
                    }
                }
                self.chunk_position = newline_position;
                if self.discarding_oversized_record {
                    self.discarding_oversized_record = false;
                    self.at_scan_start = false;
                    self.note_physical_record()?;
                    if self.reject_oversized_records {
                        return Err(self.oversized_record_error());
                    }
                    continue;
                }
                if let Some(outcome) = self.finish_record(/*at_eof*/ false)? {
                    return Ok(Some(outcome));
                }
            } else {
                if !self.discarding_oversized_record {
                    if self.max_record_bytes.is_some_and(|max_record_bytes| {
                        self.record_reversed.len().saturating_add(chunk.len()) > max_record_bytes
                    }) {
                        self.record_reversed.clear();
                        self.discarding_oversized_record = true;
                    } else {
                        self.record_reversed.extend(chunk.iter().rev().copied());
                    }
                }
                self.chunk_position = 0;
            }
        }
    }

    fn oversized_record_error(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "reverse JSONL record exceeds the configured {} byte limit",
                self.max_record_bytes.unwrap_or_default()
            ),
        )
    }

    fn note_physical_record(&mut self) -> io::Result<()> {
        self.records_scanned = self.records_scanned.saturating_add(1);
        if self
            .max_records
            .is_some_and(|max_records| self.records_scanned > max_records)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reverse JSONL scan exceeds the configured {} record limit",
                    self.max_records.unwrap_or_default()
                ),
            ));
        }
        Ok(())
    }

    fn finish_record<T>(&mut self, at_eof: bool) -> io::Result<Option<ScanOutcome<T>>>
    where
        T: DeserializeOwned,
    {
        self.record_reversed.reverse();
        if (self.at_scan_start || at_eof) && self.record_reversed.is_empty() {
            self.at_scan_start = false;
            return Ok(None);
        }
        self.at_scan_start = false;
        self.note_physical_record()?;
        let outcome = if self.record_reversed.iter().all(u8::is_ascii_whitespace) {
            None
        } else {
            Some(match serde_json::from_slice::<T>(&self.record_reversed) {
                Ok(value) => ScanOutcome::Parsed(value),
                Err(error) => ScanOutcome::Rejected(error),
            })
        };
        self.record_reversed.clear();
        Ok(outcome)
    }
}

#[cfg(test)]
#[path = "reverse_jsonl_scanner_tests.rs"]
mod tests;
