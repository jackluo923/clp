//! Streams the KV-IR benchmark pair format through the Rust serializer.

use std::env;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;

use clp_s::ingest::KvIrSerializer;

const PAIR_STREAM_MAGIC: &[u8] = b"CLP-KV-IR-BENCH-PAIRS-V1\n";
const USER_DEFINED_METADATA: &[u8] = br#"{
  "benchmark": "clp-ffi-py-kv-ir",
  "format": 1,
  "nested": {"enabled": true, "ratio": 0.5},
  "tags": ["cpp-baseline", "streaming"]
}"#;
const OUTPUT_BUFFER_LIMIT: usize = 65_536;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let input_path = PathBuf::from(arguments.next().ok_or("missing pair-stream path")?);
    let output_path = PathBuf::from(arguments.next().ok_or("missing output path")?);
    if arguments.next().is_some() {
        return Err("usage: profile_kv_ir_serializer PAIRS OUTPUT".into());
    }

    let mut input = BufReader::new(File::open(input_path)?);
    let mut line = Vec::new();
    input.read_until(b'\n', &mut line)?;
    if line != PAIR_STREAM_MAGIC {
        return Err("invalid benchmark pair-stream magic".into());
    }
    line.clear();
    input.read_until(b'\n', &mut line)?;
    let event_count = parse_event_count(&line)?;

    let mut output = BufWriter::new(File::create(output_path)?);
    let mut serializer = KvIrSerializer::new_four_byte(Some(USER_DEFINED_METADATA))?;
    let mut auto_generated = Vec::new();
    let mut user_generated = Vec::new();
    for _ in 0..event_count {
        let mut lengths = [0_u8; 8];
        input.read_exact(&mut lengths)?;
        let auto_length = usize::try_from(u32::from_be_bytes(lengths[..4].try_into()?))?;
        let user_length = usize::try_from(u32::from_be_bytes(lengths[4..].try_into()?))?;
        read_frame(&mut input, &mut auto_generated, auto_length)?;
        read_frame(&mut input, &mut user_generated, user_length)?;
        serializer.serialize_log_event_from_msgpack_maps(&auto_generated, &user_generated)?;
        if serializer.pending_output().len() > OUTPUT_BUFFER_LIMIT {
            serializer.write_all_pending(&mut output)?;
        }
    }
    let mut trailing = [0_u8; 1];
    if input.read(&mut trailing)? != 0 {
        return Err("trailing benchmark pair-stream bytes".into());
    }
    serializer.finish()?;
    serializer.write_all_pending(&mut output)?;
    output.flush()?;
    eprintln!(
        "events={} bytes={} schema_nodes={}",
        serializer.stats().log_events(),
        serializer.stats().serialized_bytes(),
        serializer.stats().schema_nodes()
    );
    Ok(())
}

fn read_frame<R: Read>(input: &mut R, buffer: &mut Vec<u8>, length: usize) -> io::Result<()> {
    buffer.resize(length, 0);
    input.read_exact(buffer)
}

fn parse_event_count(metadata: &[u8]) -> Result<u64, Box<dyn std::error::Error>> {
    let marker = b"\"events\":";
    let start = metadata
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or("pair-stream metadata has no event count")?
        + marker.len();
    let digit_count = metadata[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_count == 0 {
        return Err("pair-stream event count is empty".into());
    }
    Ok(std::str::from_utf8(&metadata[start..start + digit_count])?.parse()?)
}
