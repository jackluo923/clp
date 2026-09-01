use std::convert::Infallible;
use std::error::Error;
use std::fs::File;
use std::time::Instant;

use clp_s::ingest::JsonArchiveOptions;
use clp_s::ingest::JsonArchiveSink;
use clp_s::ingest::ParseManyDocument;
use clp_s::ingest::ParseManyDocumentSink;
use clp_s::ingest::ParseManyOptions;
use clp_s::ingest::ParseManyReader;
use clp_s::writer::OpenDirectoryArchive;
use clp_s::writer::RecordEventAppendError;
use clp_s::writer::RecordEventAppender;
use clp_s::writer::RecordEventRef;
use clp_s::writer::WriterLimits;
use clp_s::writer::WriterOptions;

#[derive(Default)]
struct ParseSink {
    records: u64,
    events: u64,
}

impl ParseManyDocumentSink for ParseSink {
    type Error = Infallible;

    fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error> {
        self.records += 1;
        self.events += u64::try_from(document.events().len()).expect("event count fits u64");
        Ok(())
    }
}

#[derive(Default)]
struct ConversionSink {
    records: u64,
    events: u64,
}

impl RecordEventAppender for ConversionSink {
    fn try_append_record_events<'record, I, E>(
        &mut self,
        events: I,
    ) -> Result<(), RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        for (event_index, event) in events.into_iter().enumerate() {
            event.map_err(|source| RecordEventAppendError::Source {
                event_index,
                source,
            })?;
            self.events += 1;
        }
        self.records += 1;
        Ok(())
    }
}

fn options() -> WriterOptions {
    WriterOptions::default()
        .with_log_order(false)
        .with_limits(WriterLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX))
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments.next().ok_or("missing mode")?;
    let path = arguments.next().ok_or("missing input path")?;
    let input = File::open(path)?;
    let mut reader = ParseManyReader::new(input, ParseManyOptions::default());
    let begin = Instant::now();
    let (records, events) = match mode.to_str().ok_or("mode is not UTF-8")? {
        "parse" => {
            let mut sink = ParseSink::default();
            reader.read_to_end(&mut sink)?;
            (sink.records, sink.events)
        }
        "convert" => {
            let mut archive = ConversionSink::default();
            {
                let mut sink = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
                reader.read_to_end(&mut sink)?;
            }
            (archive.records, archive.events)
        }
        "ingest" => {
            let mut archive = OpenDirectoryArchive::new(options());
            {
                let mut sink = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
                reader.read_to_end(&mut sink)?;
            }
            let records = archive.record_count();
            archive.abort();
            (records, 0)
        }
        "finalize" => {
            let mut archive = OpenDirectoryArchive::new(options());
            {
                let mut sink = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
                reader.read_to_end(&mut sink)?;
            }
            let records = archive.record_count();
            let encoded = archive.finish()?;
            std::hint::black_box(encoded.total_size());
            (records, 0)
        }
        _ => return Err("unknown mode".into()),
    };
    println!(
        "mode={} elapsed={:.6} records={records} events={events}",
        mode.to_string_lossy(),
        begin.elapsed().as_secs_f64(),
    );
    Ok(())
}
