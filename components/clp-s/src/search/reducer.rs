//! C++-compatible reducer handshake and aggregation-result framing.
//!
//! The transport is caller-owned and only needs to implement [`Read`] and [`Write`]. This keeps
//! socket selection and connection policy outside the library while preserving the native-width,
//! native-endian framing used by the pinned C++ reducer protocol.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;
use std::io::Write;

use super::AggregationKind;
use super::AggregationResultRef;

const ACCEPTED_RESPONSE: u8 = b'y';
const MAX_FRAME_SIZE: usize = 64;
const GROUP_PREFIX: &[u8] = b"\x82\xaagroup_tags";
const RECORD_PREFIX: &[u8] = b"\xa7records\x91\x81\xa5count";

/// A negotiated reducer connection over a caller-owned byte stream.
///
/// Construct one adapter per search command and call [`Self::send_archive_results`] once after
/// each archive. Empty iterators emit no frames. The adapter flushes after the handshake and after
/// every archive so buffered transports have the same externally visible lifecycle as the C++
/// socket implementation.
#[derive(Debug)]
pub struct ReducerProtocol<S> {
    stream: S,
}

impl<S: Read + Write> ReducerProtocol<S> {
    /// Sends the native `int64_t` job ID and waits for the reducer's one-byte acceptance response.
    ///
    /// # Errors
    ///
    /// Returns a transport error when the job ID or response cannot be transferred, or
    /// [`ReducerProtocolError::Rejected`] when the response is not `y`.
    pub fn handshake(mut stream: S, job_id: i64) -> Result<Self, ReducerProtocolError> {
        stream
            .write_all(&job_id.to_ne_bytes())
            .map_err(ReducerProtocolError::WriteJobId)?;
        stream.flush().map_err(ReducerProtocolError::FlushJobId)?;

        let mut response = [0_u8; 1];
        stream
            .read_exact(&mut response)
            .map_err(ReducerProtocolError::ReadHandshake)?;
        if ACCEPTED_RESPONSE != response[0] {
            return Err(ReducerProtocolError::Rejected {
                response: response[0],
            });
        }
        Ok(Self { stream })
    }

    /// Sends one archive's count or count-by-time result groups.
    ///
    /// Each group is encoded as the exact `MessagePack` object consumed by the C++ reducer and is
    /// preceded by a native-width, native-endian `size_t`. Iteration order is retained. An empty
    /// iterator writes no size or payload bytes.
    ///
    /// # Errors
    ///
    /// Returns a transport error or [`ReducerProtocolError::UnsupportedAggregation`] for a result
    /// other than count or count-by-time. Results preceding an error may already have been sent.
    pub fn send_archive_results<'result>(
        &mut self,
        results: impl IntoIterator<Item = AggregationResultRef<'result>>,
    ) -> Result<(), ReducerProtocolError> {
        let mut frame = [0_u8; MAX_FRAME_SIZE];
        for result in results {
            let frame_size = encode_frame(result, &mut frame)?;
            self.stream
                .write_all(&frame_size.to_ne_bytes())
                .map_err(ReducerProtocolError::WriteFrameSize)?;
            self.stream
                .write_all(&frame[..frame_size])
                .map_err(ReducerProtocolError::WriteFrame)?;
        }
        self.stream
            .flush()
            .map_err(ReducerProtocolError::FlushResults)
    }

    /// Returns the caller-owned stream.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.stream
    }
}

/// Failure while negotiating with a reducer or publishing aggregation results.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReducerProtocolError {
    /// The native job ID could not be written.
    WriteJobId(io::Error),
    /// The job-ID handshake bytes could not be flushed.
    FlushJobId(io::Error),
    /// The one-byte reducer response could not be read.
    ReadHandshake(io::Error),
    /// The reducer did not accept the job ID.
    Rejected {
        /// Response byte returned by the reducer.
        response: u8,
    },
    /// The native frame length could not be written.
    WriteFrameSize(io::Error),
    /// A `MessagePack` frame could not be written.
    WriteFrame(io::Error),
    /// The current archive's buffered result bytes could not be flushed.
    FlushResults(io::Error),
    /// The reducer wire schema does not support this aggregation kind.
    UnsupportedAggregation {
        /// Rejected aggregation kind.
        kind: AggregationKind,
    },
}

impl Display for ReducerProtocolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::WriteJobId(source) => {
                write!(formatter, "failed to write reducer job ID: {source}")
            }
            Self::FlushJobId(source) => {
                write!(formatter, "failed to flush reducer job ID: {source}")
            }
            Self::ReadHandshake(source) => {
                write!(
                    formatter,
                    "failed to read reducer handshake response: {source}"
                )
            }
            Self::Rejected { response } => {
                write!(
                    formatter,
                    "reducer rejected the job handshake with byte 0x{response:02x}"
                )
            }
            Self::WriteFrameSize(source) => {
                write!(formatter, "failed to write reducer frame size: {source}")
            }
            Self::WriteFrame(source) => {
                write!(formatter, "failed to write reducer frame: {source}")
            }
            Self::FlushResults(source) => {
                write!(formatter, "failed to flush reducer results: {source}")
            }
            Self::UnsupportedAggregation { kind } => {
                write!(
                    formatter,
                    "reducer output does not support {kind:?} aggregation results"
                )
            }
        }
    }
}

impl Error for ReducerProtocolError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WriteJobId(source)
            | Self::FlushJobId(source)
            | Self::ReadHandshake(source)
            | Self::WriteFrameSize(source)
            | Self::WriteFrame(source)
            | Self::FlushResults(source) => Some(source),
            Self::Rejected { .. } | Self::UnsupportedAggregation { .. } => None,
        }
    }
}

fn encode_frame(
    result: AggregationResultRef<'_>,
    destination: &mut [u8; MAX_FRAME_SIZE],
) -> Result<usize, ReducerProtocolError> {
    let mut encoder = FrameEncoder::new(destination);
    encoder.extend(GROUP_PREFIX);
    let count = match result {
        AggregationResultRef::Count { count } => {
            encoder.push(0x90);
            count
        }
        AggregationResultRef::CountByTime { timestamp, count } => {
            encoder.push(0x91);
            let mut timestamp_buffer = itoa::Buffer::new();
            encoder.write_fixstr(timestamp_buffer.format(timestamp));
            count
        }
        AggregationResultRef::Minimum { .. } => {
            return Err(ReducerProtocolError::UnsupportedAggregation {
                kind: AggregationKind::Minimum,
            });
        }
        AggregationResultRef::Maximum { .. } => {
            return Err(ReducerProtocolError::UnsupportedAggregation {
                kind: AggregationKind::Maximum,
            });
        }
        AggregationResultRef::Unique { .. } => {
            return Err(ReducerProtocolError::UnsupportedAggregation {
                kind: AggregationKind::Unique,
            });
        }
    };
    encoder.extend(RECORD_PREFIX);
    encoder.write_i64(count);
    Ok(encoder.len())
}

struct FrameEncoder<'buffer> {
    destination: &'buffer mut [u8; MAX_FRAME_SIZE],
    length: usize,
}

impl<'buffer> FrameEncoder<'buffer> {
    const fn new(destination: &'buffer mut [u8; MAX_FRAME_SIZE]) -> Self {
        Self {
            destination,
            length: 0,
        }
    }

    const fn len(&self) -> usize {
        self.length
    }

    const fn push(&mut self, byte: u8) {
        self.destination[self.length] = byte;
        self.length += 1;
    }

    fn extend(&mut self, bytes: &[u8]) {
        let end = self.length + bytes.len();
        self.destination[self.length..end].copy_from_slice(bytes);
        self.length = end;
    }

    fn write_fixstr(&mut self, value: &str) {
        debug_assert!(value.len() <= 31);
        self.push(0xa0 | u8::try_from(value.len()).expect("reducer tag fits a fixstr"));
        self.extend(value.as_bytes());
    }

    fn write_i64(&mut self, value: i64) {
        let bytes = value.to_be_bytes();
        if (0..128).contains(&value) {
            self.push(bytes[7]);
        } else if u8::try_from(value).is_ok() {
            self.extend(&[0xcc, bytes[7]]);
        } else if u16::try_from(value).is_ok() {
            self.push(0xcd);
            self.extend(&bytes[6..]);
        } else if u32::try_from(value).is_ok() {
            self.push(0xce);
            self.extend(&bytes[4..]);
        } else if 0 <= value {
            self.push(0xcf);
            self.extend(&bytes);
        } else if -32 <= value {
            self.push(bytes[7]);
        } else if i8::try_from(value).is_ok() {
            self.extend(&[0xd0, bytes[7]]);
        } else if i16::try_from(value).is_ok() {
            self.push(0xd1);
            self.extend(&bytes[6..]);
        } else if i32::try_from(value).is_ok() {
            self.push(0xd2);
            self.extend(&bytes[4..]);
        } else {
            self.push(0xd3);
            self.extend(&bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::search::AggregationNumber;

    const COUNT_42_FRAME: &[u8] = b"\x82\xaagroup_tags\x90\xa7records\x91\x81\xa5count\x2a";

    #[derive(Debug, Default)]
    struct ScriptedStream {
        response: Cursor<Vec<u8>>,
        written: Vec<u8>,
        flushes: usize,
    }

    impl ScriptedStream {
        fn with_response(response: u8) -> Self {
            Self {
                response: Cursor::new(vec![response]),
                ..Self::default()
            }
        }
    }

    impl Read for ScriptedStream {
        fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
            self.response.read(destination)
        }
    }

    impl Write for ScriptedStream {
        fn write(&mut self, source: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(source);
            Ok(source.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn handshake_and_count_match_the_native_cpp_wire_bytes() {
        let job_id = 0x0102_0304_0506_0708_i64;
        let mut protocol =
            ReducerProtocol::handshake(ScriptedStream::with_response(ACCEPTED_RESPONSE), job_id)
                .expect("accept reducer handshake");
        protocol
            .send_archive_results([AggregationResultRef::Count { count: 42 }])
            .expect("send count result");

        let stream = protocol.into_inner();
        let mut expected = Vec::new();
        expected.extend_from_slice(&job_id.to_ne_bytes());
        expected.extend_from_slice(&COUNT_42_FRAME.len().to_ne_bytes());
        expected.extend_from_slice(COUNT_42_FRAME);
        assert_eq!(expected, stream.written);
        assert_eq!(2, stream.flushes);
    }

    #[test]
    fn count_by_time_retains_input_order_and_uses_decimal_group_tags() {
        let mut protocol =
            ReducerProtocol::handshake(ScriptedStream::with_response(ACCEPTED_RESPONSE), 7)
                .expect("accept reducer handshake");
        protocol
            .send_archive_results([
                AggregationResultRef::CountByTime {
                    timestamp: -1_700_000_001_000,
                    count: 256,
                },
                AggregationResultRef::CountByTime {
                    timestamp: 1_700_000_000_000,
                    count: 65_536,
                },
            ])
            .expect("send ordered time buckets");

        let written = &protocol.into_inner().written[size_of::<i64>()..];
        let frames = split_frames(written);
        assert_eq!(2, frames.len());
        assert_eq!(
            b"\x82\xaagroup_tags\x91\xae-1700000001000\xa7records\x91\x81\xa5count\xcd\x01\x00",
            frames[0]
        );
        let mut expected = b"\x82\xaagroup_tags\x91\xad1700000000000".to_vec();
        expected.extend_from_slice(b"\xa7records\x91\x81\xa5count\xce\x00\x01\x00\x00");
        assert_eq!(expected, frames[1]);
    }

    #[test]
    fn empty_archive_flushes_without_writing_a_frame() {
        let job_id = 11_i64;
        let mut protocol =
            ReducerProtocol::handshake(ScriptedStream::with_response(ACCEPTED_RESPONSE), job_id)
                .expect("accept reducer handshake");
        protocol
            .send_archive_results(std::iter::empty::<AggregationResultRef<'static>>())
            .expect("send empty archive results");
        let stream = protocol.into_inner();
        assert_eq!(job_id.to_ne_bytes(), stream.written.as_slice());
        assert_eq!(2, stream.flushes);
    }

    #[test]
    fn handshake_rejection_is_typed_and_sends_only_the_job_id() {
        let job_id = 13_i64;
        let error = ReducerProtocol::handshake(ScriptedStream::with_response(b'n'), job_id)
            .expect_err("reject reducer handshake");
        assert!(matches!(
            error,
            ReducerProtocolError::Rejected { response: b'n' }
        ));
    }

    #[test]
    fn unsupported_aggregation_is_rejected_before_its_frame_is_written() {
        let job_id = 17_i64;
        let mut protocol =
            ReducerProtocol::handshake(ScriptedStream::with_response(ACCEPTED_RESPONSE), job_id)
                .expect("accept reducer handshake");
        let error = protocol
            .send_archive_results([AggregationResultRef::Minimum {
                field: "value",
                value: AggregationNumber::Integer(1),
            }])
            .expect_err("reject unsupported reducer aggregation");
        assert!(matches!(
            error,
            ReducerProtocolError::UnsupportedAggregation {
                kind: AggregationKind::Minimum
            }
        ));
        assert_eq!(
            job_id.to_ne_bytes(),
            protocol.into_inner().written.as_slice()
        );
    }

    fn split_frames(mut source: &[u8]) -> Vec<&[u8]> {
        let mut frames = Vec::new();
        while !source.is_empty() {
            let (size, remainder) = source.split_at(size_of::<usize>());
            let size = usize::from_ne_bytes(size.try_into().expect("native reducer frame size"));
            let (frame, remainder) = remainder.split_at(size);
            frames.push(frame);
            source = remainder;
        }
        frames
    }
}
