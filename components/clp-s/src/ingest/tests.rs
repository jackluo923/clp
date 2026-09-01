use std::convert::Infallible;
use std::io;
use std::io::Read;

use super::IncompleteDocumentPolicy;
use super::InvalidRecordPolicy;
use super::JsonEvent;
use super::JsonSyntaxErrorKind;
use super::NdjsonError;
use super::NdjsonInvalidRecordKind;
use super::NdjsonLimitResource;
use super::NdjsonLimits;
use super::NdjsonOptions;
use super::NdjsonReadError;
use super::NdjsonReader;
use super::NdjsonRecord;
use super::NdjsonRecordSink;
use super::NdjsonStats;
use super::ParseManyDocument;
use super::ParseManyDocumentSink;
use super::ParseManyError;
use super::ParseManyInvalidDocumentKind;
use super::ParseManyLimitResource;
use super::ParseManyLimits;
use super::ParseManyOptions;
use super::ParseManyReadError;
use super::ParseManyReader;
use super::ParseManyStats;

#[derive(Debug, Eq, PartialEq)]
enum OwnedEvent {
    ObjectStart,
    ObjectEnd,
    ArrayStart(Vec<u8>),
    ArrayEnd,
    ObjectKey { raw: Vec<u8>, decoded: String },
    String { raw: Vec<u8>, decoded: String },
    Number(Vec<u8>),
    Boolean(bool),
    Null,
}

#[derive(Debug, Eq, PartialEq)]
struct OwnedRecord {
    line: Vec<u8>,
    json: Vec<u8>,
    events: Vec<OwnedEvent>,
    line_number: u64,
    input_offset: u64,
    record_index: u64,
}

#[derive(Default)]
struct CaptureSink {
    records: Vec<OwnedRecord>,
}

impl NdjsonRecordSink for CaptureSink {
    type Error = Infallible;

    fn write_record(&mut self, record: NdjsonRecord<'_>) -> Result<(), Self::Error> {
        self.records.push(OwnedRecord {
            line: record.line_bytes().to_vec(),
            json: record.json_bytes().to_vec(),
            events: record.events().map(own_event).collect(),
            line_number: record.line_number(),
            input_offset: record.input_offset(),
            record_index: record.record_index(),
        });
        Ok(())
    }
}

#[derive(Default)]
struct ParseManyCaptureSink {
    documents: Vec<OwnedDocument>,
}

#[derive(Debug, Eq, PartialEq)]
struct OwnedDocument {
    json: Vec<u8>,
    events: Vec<OwnedEvent>,
    input_offset: u64,
    document_index: u64,
}

impl ParseManyDocumentSink for ParseManyCaptureSink {
    type Error = Infallible;

    fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error> {
        self.documents.push(OwnedDocument {
            json: document.json_bytes().to_vec(),
            events: document.events().map(own_event).collect(),
            input_offset: document.input_offset(),
            document_index: document.document_index(),
        });
        Ok(())
    }
}

fn own_event(event: JsonEvent<'_>) -> OwnedEvent {
    match event {
        JsonEvent::ObjectStart => OwnedEvent::ObjectStart,
        JsonEvent::ObjectEnd => OwnedEvent::ObjectEnd,
        JsonEvent::ArrayStart(value) => OwnedEvent::ArrayStart(value.raw_json().to_vec()),
        JsonEvent::ArrayEnd => OwnedEvent::ArrayEnd,
        JsonEvent::ObjectKey(value) => OwnedEvent::ObjectKey {
            raw: value.raw_json().to_vec(),
            decoded: value.decoded().to_owned(),
        },
        JsonEvent::String(value) => OwnedEvent::String {
            raw: value.raw_json().to_vec(),
            decoded: value.decoded().to_owned(),
        },
        JsonEvent::Number(value) => OwnedEvent::Number(value.to_vec()),
        JsonEvent::Boolean(value) => OwnedEvent::Boolean(value),
        JsonEvent::Null => OwnedEvent::Null,
    }
}

fn read_all<R: Read>(input: R, options: NdjsonOptions) -> (NdjsonStats, CaptureSink) {
    let mut reader = NdjsonReader::new(input, options);
    let mut sink = CaptureSink::default();
    let stats = reader
        .read_to_end(&mut sink)
        .expect("capture sink is infallible");
    (stats, sink)
}

fn read_all_parse_many<R: Read>(
    input: R,
    options: ParseManyOptions,
) -> (ParseManyStats, ParseManyCaptureSink) {
    let mut reader = ParseManyReader::new(input, options);
    let mut sink = ParseManyCaptureSink::default();
    let stats = reader
        .read_to_end(&mut sink)
        .expect("capture sink is infallible");
    (stats, sink)
}

#[test]
fn physical_lines_blank_lines_crlf_and_unterminated_eof_are_explicit() {
    let input = b"\n \t\r\n {\"x\":1} \r\n[true]\n\"tail\"";
    let (stats, sink) = read_all(&input[..], NdjsonOptions::default());

    assert_eq!(u64::try_from(input.len()).unwrap(), stats.input_bytes());
    assert_eq!(5, stats.physical_lines());
    assert_eq!(2, stats.blank_lines());
    assert_eq!(0, stats.skipped_invalid_records());
    assert_eq!(3, stats.records());
    assert_eq!(3, sink.records.len());
    assert_eq!(b" {\"x\":1} \r", sink.records[0].line.as_slice());
    assert_eq!(b"{\"x\":1}", sink.records[0].json.as_slice());
    assert_eq!(3, sink.records[0].line_number);
    assert_eq!(5, sink.records[0].input_offset);
    assert_eq!(0, sink.records[0].record_index);
    assert_eq!(b"[true]", sink.records[1].json.as_slice());
    assert_eq!(4, sink.records[1].line_number);
    assert_eq!(1, sink.records[1].record_index);
    assert_eq!(b"\"tail\"", sink.records[2].json.as_slice());
    assert_eq!(5, sink.records[2].line_number);
    assert_eq!(2, sink.records[2].record_index);
}

#[test]
fn number_events_preserve_every_source_byte_without_conversion() {
    let input = b"[-0,1.2300,1E+009,123456789012345678901234567890,0.0e-0]\n";
    let (_, sink) = read_all(&input[..], NdjsonOptions::default());
    let numbers: Vec<&[u8]> = sink.records[0]
        .events
        .iter()
        .filter_map(|event| match event {
            OwnedEvent::Number(value) => Some(value.as_slice()),
            _ => None,
        })
        .collect();

    assert_eq!(
        vec![
            &b"-0"[..],
            &b"1.2300"[..],
            &b"1E+009"[..],
            &b"123456789012345678901234567890"[..],
            &b"0.0e-0"[..],
        ],
        numbers
    );
}

#[test]
fn string_and_key_events_expose_exact_tokens_and_decoded_unicode() {
    let input = r#"{"a\n":"x\u0061\uD83D\uDE00","é":"line\nquote\"slash\\"}
"#;
    let (_, sink) = read_all(input.as_bytes(), NdjsonOptions::default());

    assert_eq!(
        vec![
            OwnedEvent::ObjectStart,
            OwnedEvent::ObjectKey {
                raw: br#""a\n""#.to_vec(),
                decoded: "a\n".to_owned(),
            },
            OwnedEvent::String {
                raw: br#""x\u0061\uD83D\uDE00""#.to_vec(),
                decoded: "xa😀".to_owned(),
            },
            OwnedEvent::ObjectKey {
                raw: "\"é\"".as_bytes().to_vec(),
                decoded: "é".to_owned(),
            },
            OwnedEvent::String {
                raw: br#""line\nquote\"slash\\""#.to_vec(),
                decoded: "line\nquote\"slash\\".to_owned(),
            },
            OwnedEvent::ObjectEnd,
        ],
        sink.records[0].events
    );
}

#[test]
fn unescaped_utf8_strings_borrow_their_decoded_bytes_from_source() {
    let input = "{\"plain\":\"café\",\"empty\":\"\"}\n".as_bytes();
    let mut reader = NdjsonReader::new(input, NdjsonOptions::default());
    let mut strings = 0_usize;
    let mut sink = |record: NdjsonRecord<'_>| -> Result<(), Infallible> {
        for event in record.events() {
            let (JsonEvent::ObjectKey(value) | JsonEvent::String(value)) = event else {
                continue;
            };
            let raw_inner = &value.raw_json()[1..value.raw_json().len() - 1];
            assert_eq!(raw_inner, value.decoded_bytes());
            assert_eq!(value.decoded().as_bytes(), value.decoded_bytes());
            assert!(std::ptr::eq(
                raw_inner.as_ptr(),
                value.decoded_bytes().as_ptr()
            ));
            strings += 1;
        }
        Ok(())
    };

    assert!(reader.read_record(&mut sink).expect("valid record"));
    assert_eq!(4, strings);
    assert_eq!(0, reader.buffer_capacities().1);
}

#[test]
fn escaped_strings_still_use_the_decoded_buffer() {
    let input = br#"{"key":"line\n"}
"#;
    let mut reader = NdjsonReader::new(&input[..], NdjsonOptions::default());
    let mut saw_escaped = false;
    let mut sink = |record: NdjsonRecord<'_>| -> Result<(), Infallible> {
        for event in record.events() {
            let JsonEvent::String(value) = event else {
                continue;
            };
            assert_eq!(b"line\n", value.decoded_bytes());
            assert_eq!("line\n", value.decoded());
            let raw_inner = &value.raw_json()[1..value.raw_json().len() - 1];
            assert!(!std::ptr::eq(
                raw_inner.as_ptr(),
                value.decoded_bytes().as_ptr()
            ));
            saw_escaped = true;
        }
        Ok(())
    };

    assert!(reader.read_record(&mut sink).expect("valid record"));
    assert!(saw_escaped);
    assert_ne!(0, reader.buffer_capacities().1);
}

#[test]
fn invalid_utf8_after_valid_strings_retains_its_exact_offset() {
    let input = b"{\"ok\":\"plain\",\"bad\":\"\xff\"}\n";
    let mut reader = NdjsonReader::new(&input[..], NdjsonOptions::default());
    let mut sink = CaptureSink::default();
    let error = reader
        .read_record(&mut sink)
        .expect_err("invalid UTF-8 must be rejected");
    let NdjsonReadError::Reader(NdjsonError::InvalidRecord(invalid)) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert!(matches!(
        invalid.kind(),
        NdjsonInvalidRecordKind::Syntax(error)
            if error.kind() == JsonSyntaxErrorKind::InvalidUtf8 && error.byte_offset() == 21
    ));
}

#[test]
fn string_scanning_preserves_boundaries_escapes_and_error_offsets() {
    let syntax_error = |input: &[u8]| {
        let mut reader = NdjsonReader::new(input, NdjsonOptions::default());
        let mut sink = CaptureSink::default();
        let error = reader
            .read_record(&mut sink)
            .expect_err("fixture must contain a string error");
        let NdjsonReadError::Reader(NdjsonError::InvalidRecord(invalid)) = error else {
            panic!("unexpected error: {error:?}");
        };
        let NdjsonInvalidRecordKind::Syntax(error) = invalid.kind() else {
            panic!("unexpected invalid-record kind: {:?}", invalid.kind());
        };
        error
    };

    for prefix_bytes in 0..=24 {
        let mut valid = vec![b'"'];
        valid.extend(std::iter::repeat_n(b'a', prefix_bytes));
        valid.extend_from_slice(br#"\"tail"#);
        valid.extend_from_slice(b"\"\n");
        let (_, sink) = read_all(valid.as_slice(), NdjsonOptions::default());
        let [OwnedEvent::String { decoded, .. }] = sink.records[0].events.as_slice() else {
            panic!("expected one root string event");
        };
        assert_eq!(format!("{}\"tail", "a".repeat(prefix_bytes)), *decoded);

        let mut invalid_escape = vec![b'"'];
        invalid_escape.extend(std::iter::repeat_n(b'a', prefix_bytes));
        invalid_escape.extend_from_slice(b"\\x\"\n");
        let error = syntax_error(&invalid_escape);
        assert_eq!(JsonSyntaxErrorKind::InvalidEscape, error.kind());
        assert_eq!(prefix_bytes + 1, error.byte_offset());

        let mut control = vec![b'"'];
        control.extend(std::iter::repeat_n(b'a', prefix_bytes));
        control.extend_from_slice(&[0x1f, b'"', b'\n']);
        let error = syntax_error(&control);
        assert_eq!(JsonSyntaxErrorKind::UnescapedControlCharacter, error.kind());
        assert_eq!(prefix_bytes + 1, error.byte_offset());

        let mut trailing_escape = vec![b'"'];
        trailing_escape.extend(std::iter::repeat_n(b'a', prefix_bytes));
        trailing_escape.push(b'\\');
        let error = syntax_error(&trailing_escape);
        assert_eq!(JsonSyntaxErrorKind::UnexpectedEnd, error.kind());
        assert_eq!(trailing_escape.len(), error.byte_offset());
    }
}

#[test]
fn nested_containers_produce_balanced_depth_first_events_and_keep_duplicate_keys() {
    let input = br#"{"a":[{"b":null},true,false],"a":{}}
"#;
    let (_, sink) = read_all(&input[..], NdjsonOptions::default());
    let names: Vec<String> = sink.records[0]
        .events
        .iter()
        .map(|event| match event {
            OwnedEvent::ObjectStart => "{".to_owned(),
            OwnedEvent::ObjectEnd => "}".to_owned(),
            OwnedEvent::ArrayStart(_) => "[".to_owned(),
            OwnedEvent::ArrayEnd => "]".to_owned(),
            OwnedEvent::ObjectKey { decoded, .. } => format!("key:{decoded}"),
            OwnedEvent::String { decoded, .. } => format!("str:{decoded}"),
            OwnedEvent::Number(value) => String::from_utf8_lossy(value).into_owned(),
            OwnedEvent::Boolean(value) => value.to_string(),
            OwnedEvent::Null => "null".to_owned(),
        })
        .collect();

    assert_eq!(
        vec![
            "{", "key:a", "[", "{", "key:b", "null", "}", "true", "false", "]", "key:a", "{", "}",
            "}",
        ],
        names
    );
}

#[test]
fn array_start_preserves_exact_outer_and_nested_lexemes_in_both_framers() {
    const DOCUMENT: &[u8] = concat!(
        r#"{"outer":[ 1, ["slash\\\\marker", {"escaped":"\u0041"}],"#,
        r#" { "inner" : [ true , null ] } ],"empty":[  ]}"#,
    )
    .as_bytes();
    const ARRAYS: &[&[u8]] = &[
        concat!(
            r#"[ 1, ["slash\\\\marker", {"escaped":"\u0041"}],"#,
            r#" { "inner" : [ true , null ] } ]"#,
        )
        .as_bytes(),
        br#"["slash\\\\marker", {"escaped":"\u0041"}]"#,
        br"[ true , null ]",
        br"[  ]",
    ];

    let mut ndjson = DOCUMENT.to_vec();
    ndjson.push(b'\n');
    let (_, sink) = read_all(ChunkedReader::new(&ndjson, 1), NdjsonOptions::default());
    assert_eq!(ARRAYS, array_lexemes(&sink.records[0].events));

    let (_, sink) =
        read_all_parse_many(ChunkedReader::new(DOCUMENT, 1), ParseManyOptions::default());
    assert_eq!(ARRAYS, array_lexemes(&sink.documents[0].events));
}

fn array_lexemes(events: &[OwnedEvent]) -> Vec<&[u8]> {
    events
        .iter()
        .filter_map(|event| match event {
            OwnedEvent::ArrayStart(raw_json) => Some(raw_json.as_slice()),
            _ => None,
        })
        .collect()
}

#[test]
fn stop_reports_the_first_invalid_line_with_stream_context() {
    let first = b"{\"ok\":1}\n";
    let mut input = first.to_vec();
    input.extend_from_slice(b"{\"bad\":}\n{\"later\":2}\n");
    let mut reader = NdjsonReader::new(&input[..], NdjsonOptions::default());
    let mut sink = CaptureSink::default();

    let error = reader
        .read_to_end(&mut sink)
        .expect_err("second line is invalid");
    let NdjsonReadError::Reader(NdjsonError::InvalidRecord(invalid)) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(2, invalid.line_number());
    assert_eq!(u64::try_from(first.len()).unwrap(), invalid.input_offset());
    assert!(matches!(
        invalid.kind(),
        NdjsonInvalidRecordKind::Syntax(error)
            if error.kind() == JsonSyntaxErrorKind::ExpectedValue
    ));
    assert_eq!(1, sink.records.len());
    assert_eq!(1, reader.stats().records());
    assert_eq!(2, reader.stats().physical_lines());
}

#[test]
fn skip_resynchronizes_only_at_a_physical_lf_not_an_escaped_newline() {
    let input = b"{\"bad\":\"still\\nsame\",oops}\n{\"ok\":1}\n";
    let options = NdjsonOptions::new().with_invalid_record_policy(InvalidRecordPolicy::Skip);
    let (stats, sink) = read_all(ChunkedReader::new(input, 2), options);

    assert_eq!(2, stats.physical_lines());
    assert_eq!(1, stats.skipped_invalid_records());
    assert_eq!(1, stats.records());
    assert_eq!(1, sink.records.len());
    assert_eq!(2, sink.records[0].line_number);
    assert_eq!(b"{\"ok\":1}", sink.records[0].json.as_slice());
}

#[test]
fn oversized_line_is_drained_boundedly_before_skip_continues() {
    let oversized = b"{\"way\":\"too long\"}\n";
    let input = [oversized.as_slice(), b"{}\n"].concat();
    let options = NdjsonOptions::new()
        .with_limits(NdjsonLimits::new(8, 32, 32, 32))
        .with_invalid_record_policy(InvalidRecordPolicy::Skip);
    let (stats, sink) = read_all(ChunkedReader::new(&input, 3), options);

    assert_eq!(2, stats.physical_lines());
    assert_eq!(1, stats.skipped_invalid_records());
    assert_eq!(1, stats.records());
    assert_eq!(b"{}", sink.records[0].json.as_slice());
    assert_eq!(2, sink.records[0].line_number);
}

#[test]
fn every_per_record_limit_reports_exact_resource_actual_and_limit() {
    assert_limit(
        b"12345\r\n",
        NdjsonLimits::new(5, 8, 8, 8),
        NdjsonLimitResource::RecordBytes,
        6,
        5,
    );
    assert_limit(
        b"[[]]\n",
        NdjsonLimits::new(64, 1, 8, 8),
        NdjsonLimitResource::NestingDepth,
        2,
        1,
    );
    assert_limit(
        b"[0,1]\n",
        NdjsonLimits::new(64, 8, 2, 8),
        NdjsonLimitResource::Values,
        3,
        2,
    );
    assert_limit(
        b"\"abcd\"\n",
        NdjsonLimits::new(64, 8, 8, 5),
        NdjsonLimitResource::ScalarTokenBytes,
        6,
        5,
    );
}

fn assert_limit(
    input: &[u8],
    limits: NdjsonLimits,
    resource: NdjsonLimitResource,
    actual: u64,
    limit: u64,
) {
    let mut reader = NdjsonReader::new(input, NdjsonOptions::new().with_limits(limits));
    let mut sink = CaptureSink::default();
    let error = reader
        .read_record(&mut sink)
        .expect_err("record must exceed its limit");
    let NdjsonReadError::Reader(NdjsonError::InvalidRecord(invalid)) = error else {
        panic!("unexpected error: {error:?}");
    };
    let NdjsonInvalidRecordKind::Limit(violation) = invalid.kind() else {
        panic!("unexpected invalid-record kind: {:?}", invalid.kind());
    };
    assert_eq!(resource, violation.resource());
    assert_eq!(actual, violation.actual());
    assert_eq!(limit, violation.limit());
}

#[test]
fn invalid_utf8_surrogates_numbers_commas_and_trailing_documents_are_rejected() {
    let cases: &[(&[u8], JsonSyntaxErrorKind)] = &[
        (&[b'"', 0xff, b'"', b'\n'], JsonSyntaxErrorKind::InvalidUtf8),
        (b"\"\\uD800\"\n", JsonSyntaxErrorKind::UnpairedSurrogate),
        (b"[1,]\n", JsonSyntaxErrorKind::ExpectedValue),
        (b"01\n", JsonSyntaxErrorKind::InvalidNumber),
        (b"{}{}\n", JsonSyntaxErrorKind::TrailingCharacters),
    ];

    for (input, expected) in cases {
        let mut reader = NdjsonReader::new(*input, NdjsonOptions::default());
        let mut sink = CaptureSink::default();
        let error = reader
            .read_record(&mut sink)
            .expect_err("fixture must be invalid");
        let NdjsonReadError::Reader(NdjsonError::InvalidRecord(invalid)) = error else {
            panic!("unexpected error for {input:?}: {error:?}");
        };
        assert!(matches!(
            invalid.kind(),
            NdjsonInvalidRecordKind::Syntax(error) if error.kind() == *expected
        ));
    }
}

#[test]
fn sink_error_is_contextual_and_does_not_increment_successful_records() {
    let mut reader = NdjsonReader::new(&b"null\n"[..], NdjsonOptions::default());
    let mut sink = RejectingSink;
    let error = reader
        .read_record(&mut sink)
        .expect_err("sink rejects the record");

    assert!(matches!(
        error,
        NdjsonReadError::Sink {
            line_number: 1,
            record_index: 0,
            source: "rejected",
        }
    ));
    assert_eq!(0, reader.stats().records());
    assert_eq!(1, reader.stats().physical_lines());
}

struct RejectingSink;

impl NdjsonRecordSink for RejectingSink {
    type Error = &'static str;

    fn write_record(&mut self, _record: NdjsonRecord<'_>) -> Result<(), Self::Error> {
        Err("rejected")
    }
}

struct ChunkedReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    chunk_bytes: usize,
}

impl<'a> ChunkedReader<'a> {
    const fn new(bytes: &'a [u8], chunk_bytes: usize) -> Self {
        Self {
            bytes,
            offset: 0,
            chunk_bytes,
        }
    }
}

impl Read for ChunkedReader<'_> {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        let remaining = &self.bytes[self.offset..];
        let count = remaining.len().min(destination.len()).min(self.chunk_bytes);
        destination[..count].copy_from_slice(&remaining[..count]);
        self.offset += count;
        Ok(count)
    }
}

#[test]
fn parse_many_matches_oracle_multiline_whitespace_and_direct_adjacency() {
    // The pinned C++ binary accepts these three boundaries through simdjson::iterate_many,
    // including the direct `}{` transition between the first two objects.
    let first = &b"{\"id\":1}"[..];
    let second = &b"{\n  \"id\": 2,\n  \"nested\": {\"n\": 1E+009}\n}"[..];
    let third = &b"{\"id\":3}"[..];
    let mut input = b" \n".to_vec();
    let first_offset = input.len();
    input.extend_from_slice(first);
    let second_offset = input.len();
    input.extend_from_slice(second);
    input.extend_from_slice(b"\t\r\n");
    let third_offset = input.len();
    input.extend_from_slice(third);
    input.extend_from_slice(b" \n");

    let (stats, sink) =
        read_all_parse_many(ChunkedReader::new(&input, 3), ParseManyOptions::default());

    assert_eq!(u64::try_from(input.len()).unwrap(), stats.input_bytes());
    assert_eq!(7, stats.separator_bytes());
    assert_eq!(3, stats.documents());
    assert_eq!(
        vec![first, second, third],
        sink.documents
            .iter()
            .map(|d| d.json.as_slice())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        u64::try_from(first_offset).unwrap(),
        sink.documents[0].input_offset
    );
    assert_eq!(
        u64::try_from(second_offset).unwrap(),
        sink.documents[1].input_offset
    );
    assert_eq!(
        u64::try_from(third_offset).unwrap(),
        sink.documents[2].input_offset
    );
    assert_eq!(
        vec![0, 1, 2],
        sink.documents
            .iter()
            .map(|document| document.document_index)
            .collect::<Vec<_>>()
    );
    assert!(
        sink.documents[1]
            .events
            .iter()
            .any(|event| { matches!(event, OwnedEvent::Number(number) if number == b"1E+009") })
    );
}

#[test]
fn parse_many_single_chunk_lines_commit_only_the_object_bytes() {
    let input = b" \t{\"id\":1} \t\r\n{\"id\":2}\n";
    let mut reader = ParseManyReader::new(&input[..], ParseManyOptions::default());
    let mut sink = ParseManyCaptureSink::default();

    assert!(reader.read_document(&mut sink).unwrap());
    assert_eq!(10, reader.stats().input_bytes());
    assert_eq!(2, reader.stats().separator_bytes());

    assert!(reader.read_document(&mut sink).unwrap());
    assert_eq!(22, reader.stats().input_bytes());
    assert_eq!(6, reader.stats().separator_bytes());
    assert!(!reader.read_document(&mut sink).unwrap());

    assert_eq!(
        u64::try_from(input.len()).unwrap(),
        reader.stats().input_bytes()
    );
    assert_eq!(7, reader.stats().separator_bytes());
    assert_eq!(2, reader.stats().documents());
    assert_eq!(b"{\"id\":1}".as_slice(), sink.documents[0].json);
    assert_eq!(b"{\"id\":2}".as_slice(), sink.documents[1].json);
    assert_eq!(2, sink.documents[0].input_offset);
    assert_eq!(14, sink.documents[1].input_offset);
}

#[test]
fn parse_many_single_chunk_speculation_falls_back_for_direct_adjacency() {
    let input = b"{}{}\n";
    let (stats, sink) = read_all_parse_many(&input[..], ParseManyOptions::default());

    assert_eq!(u64::try_from(input.len()).unwrap(), stats.input_bytes());
    assert_eq!(1, stats.separator_bytes());
    assert_eq!(2, stats.documents());
    assert_eq!(b"{}".as_slice(), sink.documents[0].json);
    assert_eq!(b"{}".as_slice(), sink.documents[1].json);
    assert_eq!(0, sink.documents[0].input_offset);
    assert_eq!(2, sink.documents[1].input_offset);
}

#[test]
fn parse_many_structural_framing_ignores_escaped_quotes_and_braces_in_strings() {
    let input = b"{\"text\":\"} { \\\" still one\",\"array\":[{\"x\":true}]}{\"next\":null}";
    let (_, sink) = read_all_parse_many(ChunkedReader::new(input, 1), ParseManyOptions::default());

    assert_eq!(2, sink.documents.len());
    assert_eq!(
        b"{\"text\":\"} { \\\" still one\",\"array\":[{\"x\":true}]}".as_slice(),
        sink.documents[0].json
    );
    assert_eq!(b"{\"next\":null}".as_slice(), sink.documents[1].json);
}

#[test]
fn parse_many_rejects_non_object_separators_and_latches_after_failure() {
    for invalid_suffix in [&b"[]"[..], b"true", b",{}"] {
        let mut input = b"{} ".to_vec();
        input.extend_from_slice(invalid_suffix);
        let mut reader = ParseManyReader::new(&input[..], ParseManyOptions::default());
        let mut sink = ParseManyCaptureSink::default();
        assert!(reader.read_document(&mut sink).unwrap());

        let error = reader
            .read_document(&mut sink)
            .expect_err("next token is not a root object");
        let ParseManyReadError::Reader(ParseManyError::InvalidDocument(invalid)) = error else {
            panic!("unexpected error: {error:?}");
        };
        assert_eq!(1, invalid.document_index());
        assert_eq!(3, invalid.input_offset());
        assert!(matches!(
            invalid.kind(),
            ParseManyInvalidDocumentKind::Syntax(error)
                if error.kind() == JsonSyntaxErrorKind::ExpectedObject
        ));
        assert!(matches!(
            reader.read_document(&mut sink),
            Err(ParseManyReadError::Reader(ParseManyError::Stopped))
        ));
        assert_eq!(1, sink.documents.len());
    }
}

#[test]
fn parse_many_stops_on_malformed_middle_object_before_later_objects() {
    let input = b"{}{\"bad\" 1}{}";
    let mut reader =
        ParseManyReader::new(ChunkedReader::new(input, 2), ParseManyOptions::default());
    let mut sink = ParseManyCaptureSink::default();
    assert!(reader.read_document(&mut sink).unwrap());

    let error = reader
        .read_document(&mut sink)
        .expect_err("middle object is malformed");
    let ParseManyReadError::Reader(ParseManyError::InvalidDocument(invalid)) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(1, invalid.document_index());
    assert_eq!(2, invalid.input_offset());
    assert!(matches!(
        invalid.kind(),
        ParseManyInvalidDocumentKind::Syntax(error)
            if error.kind() == JsonSyntaxErrorKind::ExpectedColon
    ));
    assert_eq!(1, sink.documents.len());
}

#[test]
fn parse_many_reports_incomplete_eof_suffix_instead_of_silently_dropping_it() {
    // The pinned C++ iterator warns and drops this seven-byte suffix. The strict library adapter
    // intentionally makes it an error so callers cannot mistake partial ingestion for success.
    let input = b"{\"id\":1} {\"id\":";
    let mut reader = ParseManyReader::new(&input[..], ParseManyOptions::default());
    let mut sink = ParseManyCaptureSink::default();
    assert!(reader.read_document(&mut sink).unwrap());

    let error = reader
        .read_document(&mut sink)
        .expect_err("truncated object must be reported");
    let ParseManyReadError::Reader(ParseManyError::InvalidDocument(invalid)) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(9, invalid.input_offset());
    assert!(matches!(
        invalid.kind(),
        ParseManyInvalidDocumentKind::Syntax(error)
            if error.kind() == JsonSyntaxErrorKind::UnexpectedEnd
    ));
    assert_eq!(
        u64::try_from(input.len()).unwrap(),
        reader.stats().input_bytes()
    );
}

#[test]
fn parse_many_cpp_policy_accounts_and_reports_an_ignored_incomplete_suffix() {
    let input = b"{\"id\":1} {\"id\":";
    let options =
        ParseManyOptions::new().with_incomplete_document_policy(IncompleteDocumentPolicy::Ignore);
    let mut reader = ParseManyReader::new(&input[..], options);
    let mut sink = ParseManyCaptureSink::default();
    let stats = reader.read_to_end(&mut sink).expect("ignore final suffix");

    assert_eq!(1, sink.documents.len());
    assert_eq!(u64::try_from(input.len()).unwrap(), stats.input_bytes());
    assert_eq!(6, stats.truncated_bytes());
    assert_eq!(1, stats.documents());
}

#[test]
fn parse_many_enforces_document_and_shared_parser_limits() {
    assert_parse_many_limit(
        b"{\"a\":1}",
        ParseManyLimits::new(6, 8, 8, 8),
        ParseManyLimitResource::DocumentBytes,
        7,
        6,
    );
    assert_parse_many_limit(
        b"{\"a\":[[]]}",
        ParseManyLimits::new(64, 2, 8, 8),
        ParseManyLimitResource::NestingDepth,
        3,
        2,
    );
    assert_parse_many_limit(
        b"{\"a\":0,\"b\":1}",
        ParseManyLimits::new(64, 8, 2, 8),
        ParseManyLimitResource::Values,
        3,
        2,
    );
    assert_parse_many_limit(
        b"{\"long-key\":0}",
        ParseManyLimits::new(64, 8, 8, 9),
        ParseManyLimitResource::ScalarTokenBytes,
        10,
        9,
    );
}

fn assert_parse_many_limit(
    input: &[u8],
    limits: ParseManyLimits,
    resource: ParseManyLimitResource,
    actual: u64,
    limit: u64,
) {
    let mut reader = ParseManyReader::new(
        ChunkedReader::new(input, 2),
        ParseManyOptions::new().with_limits(limits),
    );
    let mut sink = ParseManyCaptureSink::default();
    let error = reader
        .read_document(&mut sink)
        .expect_err("document must exceed its limit");
    let ParseManyReadError::Reader(ParseManyError::InvalidDocument(invalid)) = error else {
        panic!("unexpected error: {error:?}");
    };
    let ParseManyInvalidDocumentKind::Limit(violation) = invalid.kind() else {
        panic!("unexpected invalid-document kind: {:?}", invalid.kind());
    };
    assert_eq!(resource, violation.resource());
    assert_eq!(actual, violation.actual());
    assert_eq!(limit, violation.limit());
}

#[test]
fn large_documents_reuse_amortized_reader_and_parser_buffers() {
    let mut document = String::from("{\"values\":[");
    for index in 0..4_096 {
        if 0 != index {
            document.push(',');
        }
        document.push_str("null");
    }
    document.push_str("],\"text\":\"");
    document.push_str(&"x".repeat(64 * 1024));
    document.push_str("\"}");

    let mut parse_input = document.as_bytes().to_vec();
    parse_input.extend_from_slice(document.as_bytes());
    let limits = ParseManyLimits::new(128 * 1024, 32, 8_192, 96 * 1024);
    let mut parse_reader = ParseManyReader::new(
        ChunkedReader::new(&parse_input, 127),
        ParseManyOptions::new().with_limits(limits),
    );
    let mut parse_sink = ParseManyCaptureSink::default();
    assert!(parse_reader.read_document(&mut parse_sink).unwrap());
    let first_capacities = parse_reader.buffer_capacities();
    assert!(parse_reader.read_document(&mut parse_sink).unwrap());
    assert_eq!(first_capacities, parse_reader.buffer_capacities());

    let mut ndjson_input = document.as_bytes().to_vec();
    ndjson_input.push(b'\n');
    ndjson_input.extend_from_slice(document.as_bytes());
    ndjson_input.push(b'\n');
    let ndjson_limits = NdjsonLimits::new(128 * 1024, 32, 8_192, 96 * 1024);
    let mut ndjson_reader = NdjsonReader::new(
        ChunkedReader::new(&ndjson_input, 127),
        NdjsonOptions::new().with_limits(ndjson_limits),
    );
    let mut ndjson_sink = CaptureSink::default();
    assert!(ndjson_reader.read_record(&mut ndjson_sink).unwrap());
    let first_capacities = ndjson_reader.buffer_capacities();
    assert!(ndjson_reader.read_record(&mut ndjson_sink).unwrap());
    assert_eq!(first_capacities, ndjson_reader.buffer_capacities());
}
