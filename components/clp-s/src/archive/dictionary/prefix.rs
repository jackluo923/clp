//! Resumable decoding of the variable dictionary's entry sequence.
//!
//! The decoder owns its input, zstd context, and decoded prefix. It never borrows the archive
//! source: other archive sections can be read between extensions without invalidating its state.

use super::*;

const INPUT_CHUNK: usize = 16 * 1024;
const OUTPUT_CHUNK: usize = 32 * 1024;

pub(in crate::archive) struct VariableDictionaryPrefix {
    range: Range<u64>,
    limits: DictionaryLimits,
    entry_count: u64,
    context: zstd::zstd_safe::DCtx<'static>,
    input: Vec<u8>,
    input_pos: usize,
    next_read: u64,
    frame_started: bool,
    frame_finished: bool,
    bytes: Vec<u8>,
    completed_bytes: Option<std::sync::Arc<Vec<u8>>>,
    parsed: usize,
    ranges: Vec<Range<usize>>,
    uuid_entries: Vec<usize>,
    uuids: Vec<u8>,
    total_value_bytes: u64,
    complete: bool,
}

impl fmt::Debug for VariableDictionaryPrefix {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VariableDictionaryPrefix")
            .field("range", &self.range)
            .field("entry_count", &self.entry_count)
            .field("loaded", &self.ranges.len())
            .field("complete", &self.complete)
            .finish_non_exhaustive()
    }
}

impl VariableDictionaryPrefix {
    /// Extends a cached prefix and returns a self-contained dictionary snapshot. The caller drops
    /// the cache on failure: a decompressor that reported an error must not be resumed.
    pub(in crate::archive) fn read<F>(
        cache: &mut Option<Self>,
        range: Range<u64>,
        limits: DictionaryLimits,
        minimum_entries: u64,
        mut read_at: F,
    ) -> Result<VariableDictionary, DictionaryError>
    where
        F: FnMut(u64, &mut [u8]) -> io::Result<()>,
    {
        if cache
            .as_ref()
            .is_none_or(|state| state.range != range || state.limits != limits)
        {
            *cache = Some(Self::new(range, limits, &mut read_at)?);
        }
        let state = cache.as_mut().expect("initialized dictionary prefix");
        state.extend(minimum_entries, &mut read_at)?;
        state.snapshot()
    }

    fn new<F>(
        range: Range<u64>,
        limits: DictionaryLimits,
        read_at: &mut F,
    ) -> Result<Self, DictionaryError>
    where
        F: FnMut(u64, &mut [u8]) -> io::Result<()>,
    {
        let compressed_size = range
            .end
            .checked_sub(range.start)
            .ok_or(DictionaryError::SizeOverflow)?;
        if compressed_size > limits.compressed {
            return Err(DictionaryError::CompressedSectionTooLarge {
                actual: compressed_size,
                limit: limits.compressed,
            });
        }
        let mut next_read = range.start;
        let first = read_word(&mut next_read, range.end, read_at)?;
        let (entry_count, uuids) = if first == BINARY_UUIDS_MARKER {
            let entry_count = read_word(&mut next_read, range.end, read_at)?;
            let uuid_count = read_word(&mut next_read, range.end, read_at)?;
            if uuid_count > entry_count || entry_count > limits.entries {
                return Err(DictionaryError::EntryCountTooLarge {
                    actual: uuid_count.max(entry_count),
                    limit: limits.entries.min(entry_count),
                });
            }
            let uuid_bytes = uuid_count
                .checked_mul(UUID_SIZE)
                .filter(|bytes| *bytes <= limits.decompressed)
                .ok_or(DictionaryError::SizeOverflow)?;
            let frame_len = read_word(&mut next_read, range.end, read_at)?;
            let end = next_read
                .checked_add(frame_len)
                .filter(|end| *end <= range.end)
                .ok_or(DictionaryError::SizeOverflow)?;
            let mut frame = allocate(frame_len)?;
            read_at(next_read, &mut frame).map_err(DictionaryError::Io)?;
            next_read = end;
            let mut columns = allocate(uuid_bytes)?;
            let column_bytes =
                usize::try_from(uuid_count).map_err(|_| DictionaryError::SizeOverflow)?;
            let mut rest = frame.as_slice();
            let mut context = zstd::zstd_safe::DCtx::create();
            if column_bytes != 0 {
                for column in columns.chunks_exact_mut(column_bytes) {
                    let size = zstd::zstd_safe::find_frame_compressed_size(rest).map_err(|_| {
                        invalid("binary UUIDs are not sixteen complete zstd frames")
                    })?;
                    let written = context
                        .decompress(column, &rest[..size])
                        .map_err(|_| invalid("a binary-UUID column does not decompress"))?;
                    if written != column_bytes {
                        return Err(invalid(
                            "a binary-UUID column holds the wrong number of bytes",
                        ));
                    }
                    rest = &rest[size..];
                }
            }
            if !rest.is_empty() {
                return Err(DictionaryError::TrailingCompressedData {
                    remaining: rest.len() as u64,
                });
            }
            (entry_count, columns)
        } else {
            (first, Vec::new())
        };
        if entry_count > limits.entries {
            return Err(DictionaryError::EntryCountTooLarge {
                actual: entry_count,
                limit: limits.entries,
            });
        }
        let minimum_size = entry_count
            .checked_mul(ENTRY_LENGTH_SIZE)
            .ok_or(DictionaryError::SizeOverflow)?;
        if minimum_size > limits.decompressed {
            return Err(DictionaryError::DecompressedSectionTooLarge {
                actual: minimum_size,
                limit: limits.decompressed,
            });
        }
        if entry_count == 0 && next_read != range.end {
            return Err(DictionaryError::TrailingCompressedData {
                remaining: range.end - next_read,
            });
        }
        Ok(Self {
            range,
            limits,
            entry_count,
            context: zstd::zstd_safe::DCtx::create(),
            input: Vec::new(),
            input_pos: 0,
            next_read,
            frame_started: false,
            frame_finished: entry_count == 0,
            bytes: Vec::new(),
            completed_bytes: None,
            parsed: 0,
            ranges: Vec::new(),
            uuid_entries: Vec::new(),
            uuids,
            total_value_bytes: 0,
            complete: entry_count == 0,
        })
    }

    fn extend<F>(&mut self, minimum_entries: u64, read_at: &mut F) -> Result<(), DictionaryError>
    where
        F: FnMut(u64, &mut [u8]) -> io::Result<()>,
    {
        let requested = minimum_entries.min(self.entry_count);
        while (self.ranges.len() as u64) < requested {
            let length_end = self
                .parsed
                .checked_add(8)
                .ok_or(DictionaryError::SizeOverflow)?;
            self.ensure(length_end, read_at)?;
            let length = u64::from_le_bytes(
                self.bytes[self.parsed..length_end]
                    .try_into()
                    .expect("eight bytes"),
            );
            let uuid = length == BINARY_UUID_LENGTH && !self.uuids.is_empty();
            let value_bytes = if uuid { 36 } else { length };
            if !uuid && value_bytes > self.limits.entry_bytes {
                return Err(DictionaryError::EntryTooLarge {
                    entry_id: self.ranges.len() as u64,
                    actual: value_bytes,
                    limit: self.limits.entry_bytes,
                });
            }
            let total = self
                .total_value_bytes
                .checked_add(value_bytes)
                .ok_or(DictionaryError::SizeOverflow)?;
            if total > self.limits.total_value_bytes {
                return Err(DictionaryError::TotalValueBytesTooLarge {
                    actual: total,
                    limit: self.limits.total_value_bytes,
                });
            }
            self.ranges
                .try_reserve(1)
                .map_err(|_| DictionaryError::AllocationFailed {
                    requested: self.ranges.len() + 1,
                })?;
            if uuid {
                if self.uuid_entries.len() >= self.uuids.len() / 16 {
                    return Err(invalid("dictionary marks more binary UUIDs than it stores"));
                }
                self.uuid_entries.try_reserve(1).map_err(|_| {
                    DictionaryError::AllocationFailed {
                        requested: self.uuid_entries.len() + 1,
                    }
                })?;
                self.uuid_entries.push(self.ranges.len());
                self.ranges.push(0..0);
                self.parsed = length_end;
            } else {
                let end = length_end
                    .checked_add(
                        usize::try_from(length).map_err(|_| DictionaryError::SizeOverflow)?,
                    )
                    .ok_or(DictionaryError::SizeOverflow)?;
                self.ensure(end, read_at)?;
                self.ranges.push(length_end..end);
                self.parsed = end;
            }
            self.total_value_bytes = total;
        }
        if self.ranges.len() as u64 == self.entry_count && !self.complete {
            while !self.frame_finished {
                self.pump(read_at)?;
            }
            if self.parsed != self.bytes.len() || self.uuid_entries.len() != self.uuids.len() / 16 {
                return Err(DictionaryError::TrailingDecompressedData);
            }
            let buffered = self.input.len() - self.input_pos;
            let remaining = (self.range.end - self.next_read) + buffered as u64;
            if remaining != 0 {
                return Err(DictionaryError::TrailingCompressedData { remaining });
            }
            self.complete = true;
        }
        Ok(())
    }

    fn ensure<F>(&mut self, size: usize, read_at: &mut F) -> Result<(), DictionaryError>
    where
        F: FnMut(u64, &mut [u8]) -> io::Result<()>,
    {
        if size as u64 > self.limits.decompressed {
            return Err(DictionaryError::DecompressedSectionTooLarge {
                actual: size as u64,
                limit: self.limits.decompressed,
            });
        }
        while self.bytes.len() < size {
            if self.frame_finished {
                return Err(truncated());
            }
            self.pump(read_at)?;
        }
        Ok(())
    }

    fn pump<F>(&mut self, read_at: &mut F) -> Result<(), DictionaryError>
    where
        F: FnMut(u64, &mut [u8]) -> io::Result<()>,
    {
        if self.input_pos == self.input.len() && self.next_read < self.range.end {
            let size = (self.range.end - self.next_read).min(INPUT_CHUNK as u64) as usize;
            self.input.resize(size, 0);
            read_at(self.next_read, &mut self.input).map_err(DictionaryError::Io)?;
            self.next_read += size as u64;
            self.input_pos = 0;
            if !self.frame_started {
                if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(&self.input) {
                    if size > self.limits.decompressed {
                        return Err(DictionaryError::DecompressedSectionTooLarge {
                            actual: size,
                            limit: self.limits.decompressed,
                        });
                    }
                }
                self.frame_started = true;
            }
        }
        // One extra byte permits detecting an oversized stream without allocating beyond a small
        // fixed staging buffer. Normal calls retain only bounded chunks of decoded data.
        let remaining = self
            .limits
            .decompressed
            .saturating_sub(self.bytes.len() as u64);
        let capacity = remaining.saturating_add(1).min(OUTPUT_CHUNK as u64) as usize;
        let mut output = [0_u8; OUTPUT_CHUNK];
        let mut input = zstd::zstd_safe::InBuffer::around(&self.input[self.input_pos..]);
        let mut out = zstd::zstd_safe::OutBuffer::around(&mut output[..capacity]);
        let pending = self
            .context
            .decompress_stream(&mut out, &mut input)
            .map_err(|code| invalid(zstd::zstd_safe::get_error_name(code)))?;
        let consumed = input.pos();
        let written = out.pos();
        self.input_pos += consumed;
        self.frame_finished = pending == 0;
        let decoded_size = (self.bytes.len() as u64)
            .checked_add(written as u64)
            .ok_or(DictionaryError::SizeOverflow)?;
        if decoded_size > self.limits.decompressed {
            return Err(DictionaryError::DecompressedSectionTooLarge {
                actual: decoded_size,
                limit: self.limits.decompressed,
            });
        }
        self.bytes
            .try_reserve(written)
            .map_err(|_| DictionaryError::AllocationFailed {
                requested: self.bytes.len() + written,
            })?;
        self.bytes.extend_from_slice(&output[..written]);
        if consumed == 0 && written == 0 && !self.frame_finished {
            return Err(truncated());
        }
        Ok(())
    }

    fn snapshot(&mut self) -> Result<VariableDictionary, DictionaryError> {
        // Once finished, the frame is immutable. Share it with every complete snapshot instead
        // of retaining both a decoder buffer and another complete value-byte allocation.
        let bytes = if self.complete {
            let shared = self
                .completed_bytes
                .get_or_insert_with(|| std::sync::Arc::new(std::mem::take(&mut self.bytes)));
            StorageBytes::Shared(std::sync::Arc::clone(shared))
        } else {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(self.parsed).map_err(|_| {
                DictionaryError::AllocationFailed {
                    requested: self.parsed,
                }
            })?;
            bytes.extend_from_slice(&self.bytes[..self.parsed]);
            StorageBytes::Owned(bytes)
        };
        let mut ranges = Vec::new();
        ranges.try_reserve_exact(self.ranges.len()).map_err(|_| {
            DictionaryError::AllocationFailed {
                requested: self.ranges.len(),
            }
        })?;
        ranges.extend(self.ranges.iter().cloned());
        for (uuid, &entry) in self.uuid_entries.iter().enumerate() {
            let start = uuid
                .checked_mul(36)
                .and_then(|offset| self.parsed.checked_add(offset))
                .ok_or(DictionaryError::SizeOverflow)?;
            ranges[entry] = start..start.checked_add(36).ok_or(DictionaryError::SizeOverflow)?;
        }
        Ok(VariableDictionary {
            complete: self.complete,
            storage: DictionaryStorage {
                bytes,
                ranges,
                verbatim: Vec::new(),
            },
            frame_len: self.parsed,
            uuids: self.uuids.clone(),
            rendered: std::sync::OnceLock::new(),
        })
    }
}

fn read_word<F>(offset: &mut u64, end: u64, read_at: &mut F) -> Result<u64, DictionaryError>
where
    F: FnMut(u64, &mut [u8]) -> io::Result<()>,
{
    let next = offset
        .checked_add(8)
        .filter(|next| *next <= end)
        .ok_or_else(truncated)?;
    let mut bytes = [0; 8];
    read_at(*offset, &mut bytes).map_err(DictionaryError::Io)?;
    *offset = next;
    Ok(u64::from_le_bytes(bytes))
}

fn allocate(size: u64) -> Result<Vec<u8>, DictionaryError> {
    let size = usize::try_from(size).map_err(|_| DictionaryError::SizeOverflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| DictionaryError::AllocationFailed { requested: size })?;
    bytes.resize(size, 0);
    Ok(bytes)
}

fn invalid(message: &'static str) -> DictionaryError {
    DictionaryError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn truncated() -> DictionaryError {
    DictionaryError::Io(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "dictionary entry sequence is truncated",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(entries: &[&[u8]]) -> Vec<u8> {
        let mut bytes = (entries.len() as u64).to_le_bytes().to_vec();
        if !entries.is_empty() {
            let frame: Vec<u8> = entries
                .iter()
                .flat_map(|value| {
                    (value.len() as u64)
                        .to_le_bytes()
                        .into_iter()
                        .chain(value.iter().copied())
                })
                .collect();
            bytes.extend(zstd::bulk::compress(&frame, 3).expect("compress"));
        }
        bytes
    }

    fn read(
        cache: &mut Option<VariableDictionaryPrefix>,
        data: &[u8],
        entries: u64,
    ) -> Result<VariableDictionary, DictionaryError> {
        VariableDictionaryPrefix::read(
            cache,
            0..data.len() as u64,
            DictionaryLimits::default(),
            entries,
            |offset, bytes| {
                let start = offset as usize;
                bytes.copy_from_slice(&data[start..start + bytes.len()]);
                Ok(())
            },
        )
    }

    #[test]
    fn growing_prefix_resumes_without_rereading_or_reparsing() {
        let mut random = 42_u64;
        let values: Vec<Vec<u8>> = (0..5000)
            .map(|_| {
                (0..64)
                    .map(|_| {
                        random ^= random << 13;
                        random ^= random >> 7;
                        random ^= random << 17;
                        random as u8
                    })
                    .collect()
            })
            .collect();
        let entries: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        let data = encoded(&entries);
        let mut cache = None;
        let mut next_expected = 0;
        let mut source = |offset, bytes: &mut [u8]| {
            assert_eq!(
                next_expected, offset,
                "every compressed byte is read once, sequentially"
            );
            next_expected += bytes.len() as u64;
            bytes.copy_from_slice(&data[offset as usize..offset as usize + bytes.len()]);
            Ok(())
        };
        let first = VariableDictionaryPrefix::read(
            &mut cache,
            0..data.len() as u64,
            DictionaryLimits::default(),
            1,
            &mut source,
        )
        .expect("first");
        assert_eq!(1, first.len());
        assert!(!first.is_complete());
        assert_eq!(entries[0], first.entry(0).expect("first value").value());
        let saved = cache.as_ref().expect("cache");
        assert!(
            saved.next_read < data.len() as u64,
            "unneeded compressed tail stays unread"
        );
        let buffered_bytes = saved.bytes.len();
        let previous_read = saved.next_read;
        let second = VariableDictionaryPrefix::read(
            &mut cache,
            0..data.len() as u64,
            DictionaryLimits::default(),
            2,
            &mut source,
        )
        .expect("second");
        assert_eq!(2, second.len());
        assert_eq!(previous_read, cache.as_ref().unwrap().next_read);
        assert_eq!(
            buffered_bytes,
            cache.as_ref().unwrap().bytes.len(),
            "already decoded bytes are reused"
        );
        let third = VariableDictionaryPrefix::read(
            &mut cache,
            0..data.len() as u64,
            DictionaryLimits::default(),
            1500,
            &mut source,
        )
        .expect("third");
        assert_eq!(entries[1499], third.entry(1499).unwrap().value());
        assert!(!third.is_complete());
        let full = VariableDictionaryPrefix::read(
            &mut cache,
            0..data.len() as u64,
            DictionaryLimits::default(),
            u64::MAX,
            &mut source,
        )
        .expect("all");
        assert!(full.is_complete());
        assert_eq!(5000, full.len());
        assert_eq!(
            entries,
            full.entries()
                .map(VariableDictionaryEntry::value)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            first.entry(0).unwrap().value(),
            entries[0],
            "older snapshots remain valid"
        );
        assert!(
            cache.as_ref().unwrap().bytes.is_empty(),
            "complete values moved to shared storage"
        );
        let repeated = VariableDictionaryPrefix::read(
            &mut cache,
            0..data.len() as u64,
            DictionaryLimits::default(),
            u64::MAX,
            &mut source,
        )
        .expect("repeat complete");
        assert_eq!(full, repeated);
        assert!(
            std::ptr::eq(
                full.entry(0).unwrap().value().as_ptr(),
                repeated.entry(0).unwrap().value().as_ptr()
            ),
            "complete snapshots share their value bytes"
        );
        assert_eq!(data.len() as u64, next_expected);
    }

    #[test]
    fn prefix_rejects_consumed_corruption_but_does_not_validate_unread_entries() {
        let mut frame = 2_u64.to_le_bytes().to_vec();
        frame.extend_from_slice(b"ok");
        frame.extend_from_slice(&u64::MAX.to_le_bytes());
        let mut data = 2_u64.to_le_bytes().to_vec();
        data.extend(zstd::bulk::compress(&frame, 3).unwrap());
        let mut cache = None;
        assert_eq!(
            b"ok",
            read(&mut cache, &data, 1)
                .unwrap()
                .entry(0)
                .unwrap()
                .value()
        );
        assert!(matches!(
            read(&mut cache, &data, 2),
            Err(DictionaryError::EntryTooLarge { entry_id: 1, .. })
        ));

        let mut data = encoded(&[b"ok", b"also ok"]);
        data.push(1);
        let mut cache = None;
        assert!(!read(&mut cache, &data, 1).unwrap().is_complete());
        assert!(matches!(
            read(&mut cache, &data, u64::MAX),
            Err(DictionaryError::TrailingCompressedData { remaining: 1 })
        ));

        let mut frame = 2_u64.to_le_bytes().to_vec();
        frame.extend_from_slice(b"ok");
        frame.push(0);
        let mut data = 1_u64.to_le_bytes().to_vec();
        data.extend(zstd::bulk::compress(&frame, 3).unwrap());
        assert!(matches!(
            read(&mut None, &data, 1),
            Err(DictionaryError::TrailingDecompressedData)
        ));
    }

    #[test]
    fn prefix_validates_truncation_empty_sections_and_resource_limits() {
        assert!(read(&mut None, b"", 1).is_err());
        let empty = read(&mut None, &encoded(&[]), u64::MAX).unwrap();
        assert!(empty.is_complete());
        assert!(empty.is_empty());
        let mut empty_tail = encoded(&[]);
        empty_tail.push(0);
        assert!(matches!(
            read(&mut None, &empty_tail, 0),
            Err(DictionaryError::TrailingCompressedData { .. })
        ));
        let data = encoded(&[b"small", b"larger"]);
        for truncated in 8..data.len() {
            assert!(
                read(&mut None, &data[..truncated], u64::MAX).is_err(),
                "truncation at {truncated}"
            );
        }
        let cases = [
            DictionaryLimits::new(1, 100, 10, 10, 100),
            DictionaryLimits::new(100, 100, 1, 10, 100),
            DictionaryLimits::new(100, 16, 10, 10, 100),
            DictionaryLimits::new(100, 100, 10, 1, 100),
            DictionaryLimits::new(100, 100, 10, 10, 1),
        ];
        for limits in cases {
            let mut cache = None;
            assert!(
                VariableDictionaryPrefix::read(
                    &mut cache,
                    0..data.len() as u64,
                    limits,
                    1,
                    |offset, bytes| {
                        bytes
                            .copy_from_slice(&data[offset as usize..offset as usize + bytes.len()]);
                        Ok(())
                    }
                )
                .is_err(),
                "limits {limits:?}"
            );
        }
        let mut cache = None;
        assert!(read(&mut cache, &data, 1).is_ok());
        // Changing resource limits must not silently reuse a previously validated looser prefix.
        assert!(
            VariableDictionaryPrefix::read(
                &mut cache,
                0..data.len() as u64,
                DictionaryLimits::new(1, 100, 10, 10, 100),
                1,
                |_, _| panic!("compressed limit checked before reading")
            )
            .is_err()
        );
    }

    #[test]
    fn completion_validates_zstd_checksum_after_unread_prefix_tail() {
        use std::io::Write;

        let mut random = 73_u64;
        let value: Vec<u8> = (0..256 * 1024)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                random as u8
            })
            .collect();
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(&2_u64.to_le_bytes()).unwrap();
        encoder.write_all(b"ok").unwrap();
        encoder
            .write_all(&(value.len() as u64).to_le_bytes())
            .unwrap();
        encoder.write_all(&value).unwrap();
        let mut data = 2_u64.to_le_bytes().to_vec();
        data.extend(encoder.finish().unwrap());
        assert!(read(&mut None, &data, u64::MAX).unwrap().is_complete());
        *data.last_mut().unwrap() ^= 1; // Corrupt only the checksum at the frame's end.
        let mut cache = None;
        let first = read(&mut cache, &data, 1).unwrap();
        assert_eq!(b"ok", first.entry(0).unwrap().value());
        assert!(!first.is_complete());
        assert!(cache.as_ref().unwrap().next_read < data.len() as u64);
        assert!(matches!(
            read(&mut cache, &data, u64::MAX),
            Err(DictionaryError::Io(_))
        ));
    }

    #[test]
    fn binary_uuid_prefix_keeps_id_order_binary_access_and_text() {
        let uuid = *b"34c75dc7-30ee-42e1-9f1e-58cf1667f88e";
        let other = *b"0f2b6a1e-9c4d-4e8b-a3f5-7d6c5b4a3921";
        let values: [&[u8]; 5] = [b"plain", &uuid, b"", &other, b"last"];
        let mut frame = Vec::new();
        let mut uuid_rows = Vec::new();
        for value in values {
            if let Some(binary) = canonical_uuid(value) {
                frame.extend_from_slice(&BINARY_UUID_LENGTH.to_le_bytes());
                uuid_rows.extend_from_slice(&binary);
            } else {
                frame.extend_from_slice(&(value.len() as u64).to_le_bytes());
                frame.extend_from_slice(value);
            }
        }
        let columns = uuid_columns(&uuid_rows);
        let uuid_frames: Vec<u8> = columns
            .chunks_exact(2)
            .flat_map(|column| zstd::bulk::compress(column, 3).unwrap())
            .collect();
        let mut data = BINARY_UUIDS_MARKER.to_le_bytes().to_vec();
        data.extend_from_slice(&5_u64.to_le_bytes());
        data.extend_from_slice(&2_u64.to_le_bytes());
        data.extend_from_slice(&(uuid_frames.len() as u64).to_le_bytes());
        data.extend_from_slice(&uuid_frames);
        data.extend(zstd::bulk::compress(&frame, 3).unwrap());
        let mut cache = None;
        let first = read(&mut cache, &data, 2).unwrap();
        assert_eq!(
            values[..2],
            first
                .entries()
                .map(VariableDictionaryEntry::value)
                .collect::<Vec<_>>()
        );
        assert_eq!(canonical_uuid(&uuid), first.binary_uuid(1));
        assert_eq!(None, first.binary_uuid(0));
        assert!(!first.is_complete());
        let second = read(&mut cache, &data, 4).unwrap();
        assert_eq!(
            values[..4],
            second
                .entries()
                .map(VariableDictionaryEntry::value)
                .collect::<Vec<_>>()
        );
        assert_eq!(canonical_uuid(&other), second.binary_uuid(3));
        assert_eq!(
            &uuid,
            first.entry(1).unwrap().value(),
            "old rendered snapshot remains stable"
        );
        let complete = read(&mut cache, &data, u64::MAX).unwrap();
        assert!(complete.is_complete());
        let full = decode_variable_dictionary(
            (&data[..]).take(data.len() as u64),
            DictionaryLimits::default(),
        )
        .unwrap();
        assert_eq!(full, complete);
    }
}
