//! Online adaptive numeric encoding for the KV-IR streaming format.
//!
//! The archive tier already runs an offline, columnar per-column bake-off
//! ([`crate::adaptive_int`] / [`crate::adaptive_float`]): it materializes every column, tries a menu
//! of codecs, measures the post-zstd size, and keeps the winner. The streaming KV-IR writer cannot
//! do that — values arrive row-interleaved and are flushed continuously, so there is never a
//! materialized column to measure. This module is the streaming sibling: it watches each numeric
//! field for a short **warmup** window, decides a single byte-aligned per-value codec once it has
//! seen enough values, emits a self-describing **control record** announcing the switch, and from
//! then on encodes that field's values with the chosen codec. The decoder mirrors the state
//! machine and restores the original `i64` (or the original `f64` for float-as-int fields), so the
//! rest of the read path (`search`, owned-event, FFI columnar) is unchanged.
//!
//! # Why this helps under zstd
//!
//! The KV-IR member is zstd-compressed as a whole. Row-interleaving scatters a single field's
//! values across the stream, so zstd's match window rarely lines them up; a wide 8-byte timestamp
//! or a large-magnitude counter compresses poorly even though the *column* is highly regular.
//! Encoding each value as a small delta/offset/dictionary code shrinks it *before* zstd, and the
//! resulting short, repetitive byte patterns are exactly what zstd does crush.
//!
//! # Schemes (all escape-free or escape-safe, all byte-aligned)
//!
//! - [`Scheme::Delta`] — `zigzag(wrapping_sub(v, last))` as a LEB128 varint. Universal and always
//!   correct (any `i64` difference is representable); ideal for monotonic / smooth signals. A
//!   constant field degenerates to a run of `0x00` deltas, which zstd removes.
//! - [`Scheme::For`] — `zigzag(wrapping_sub(v, base))` as a varint, `base` fixed from warmup.
//!   Centres a bounded oscillating field around a base so each value is a tiny varint.
//! - [`Scheme::Dict`] — a warmup-built table plus a `varint` code per value; code `0` escapes to a
//!   literal `zigzag`-varint, so post-warmup values outside the table are always representable.
//!
//! Everything here uses **wrapping** arithmetic on both sides, so round-trip is exact for every
//! `i64`, `i64::MIN`/`i64::MAX` included. Float fields only ever switch when *every* warmup value is
//! an exactly-representable integer; the integer is what gets encoded and `x as f64` restores it.

/// Per-value tag written in place of the canonical integer/float tag once a field is committed to
/// an adaptive scheme. The value bytes that follow are interpreted using the field's committed
/// [`Scheme`], which the decoder learns from the preceding [`ControlRecord`].
pub(crate) const ADAPTIVE_VALUE_TAG: u8 = 0x55;

/// Top-level unit tag for an adaptive control record (announces a field's scheme switch). Chosen
/// adjacent to the `0x3f` UTC-offset control record; `0x3e` is otherwise unused at the unit level.
pub(crate) const ADAPTIVE_CONTROL_TAG: u8 = 0x3e;

/// Wire identifier for a committed scheme, stored in the control record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Scheme {
    Delta = 1,
    For = 2,
    Dict = 3,
}

impl Scheme {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Delta),
            2 => Some(Self::For),
            3 => Some(Self::Dict),
            _ => None,
        }
    }
}

/// Errors surfaced while decoding an adaptive value or control record. Call sites map these onto
/// their own `KvIr*` error kinds (truncated payload / malformed data).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdaptiveError {
    /// Ran out of bytes mid-value or mid-record.
    Truncated,
    /// Structurally invalid (unknown scheme byte, dictionary code out of range, overlong varint).
    Malformed,
}

// ---------------------------------------------------------------------------
// varint / zigzag primitives
// ---------------------------------------------------------------------------

/// Appends `value` as an unsigned LEB128 varint.
pub(crate) fn write_uvarint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends `value` as a zigzag-then-LEB128 signed varint.
pub(crate) fn write_svarint(out: &mut Vec<u8>, value: i64) {
    write_uvarint(out, zigzag(value));
}

#[inline]
fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

#[inline]
fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Byte cursor over an adaptive payload. The value/record decoders consume exactly what they wrote,
/// which keeps them self-delimiting inside the surrounding KV-IR unit.
pub(crate) struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Number of bytes consumed so far. Call sites use this to advance the outer KV-IR reader by the
    /// exact width of the value they just decoded.
    pub(crate) fn consumed(&self) -> usize {
        self.pos
    }

    fn read_u8(&mut self) -> Result<u8, AdaptiveError> {
        let byte = *self.bytes.get(self.pos).ok_or(AdaptiveError::Truncated)?;
        self.pos += 1;
        Ok(byte)
    }

    fn read_uvarint(&mut self) -> Result<u64, AdaptiveError> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.read_u8()?;
            // 10 groups of 7 bits cover 70 bits; the final group may only carry the top bit.
            if shift >= 64 {
                return Err(AdaptiveError::Malformed);
            }
            result |= u64::from(byte & 0x7f)
                .checked_shl(shift)
                .ok_or(AdaptiveError::Malformed)?;
            if byte & 0x80 == 0 {
                // Reject overlong encodings whose high bits spill past bit 63.
                if shift == 63 && byte > 0x01 {
                    return Err(AdaptiveError::Malformed);
                }
                return Ok(result);
            }
            shift += 7;
        }
    }

    fn read_svarint(&mut self) -> Result<i64, AdaptiveError> {
        Ok(unzigzag(self.read_uvarint()?))
    }
}

// ---------------------------------------------------------------------------
// warmup accumulator + scheme selection
// ---------------------------------------------------------------------------

/// Cap on distinct values considered for the dictionary scheme. Beyond this a field is treated as
/// high-cardinality and `Dict` is dropped from the bake-off.
const DICT_MAX_CARDINALITY: usize = 256;

/// Buffers a field's warmup values (as `i64`; floats are pre-converted by the caller) and tracks
/// whether a float field has ever seen a non-integer value, which disqualifies it from switching.
#[derive(Debug)]
pub(crate) struct WarmupBuffer {
    values: Vec<i64>,
    is_float: bool,
    float_disqualified: bool,
}

impl WarmupBuffer {
    pub(crate) fn new(is_float: bool) -> Self {
        Self {
            values: Vec::new(),
            is_float,
            float_disqualified: false,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    /// Records one integer value observed during warmup.
    pub(crate) fn push_int(&mut self, value: i64) {
        self.values.push(value);
    }

    /// Records one float value observed during warmup. A value that is not an exactly-representable
    /// integer permanently disqualifies the field from adaptive encoding (it stays canonical).
    pub(crate) fn push_float(&mut self, value: f64) {
        match float_as_int(value) {
            Some(int_value) => self.values.push(int_value),
            None => self.float_disqualified = true,
        }
    }

    /// Runs the bake-off over the buffered warmup sample and returns the committed scheme, or `None`
    /// to stay on the canonical (PLAIN) encoding. On a `Some`, the caller emits the returned
    /// [`ControlRecord`] and switches the field.
    pub(crate) fn decide(&self) -> Option<CommittedScheme> {
        if self.float_disqualified || self.values.is_empty() {
            return None;
        }
        let values = &self.values;

        // Baseline: the canonical KV-IR integer encoding picks width by magnitude (1/2/4/8 bytes).
        // Adaptive and canonical both carry a 1-byte value tag, so compare payload bytes only.
        let plain: usize = values.iter().map(|&v| canonical_int_width(v)).sum();

        let mut best: Option<(usize, CommittedScheme)> = None;
        let mut consider = |bytes: usize, scheme: CommittedScheme| {
            if best.as_ref().map_or(true, |(b, _)| bytes < *b) {
                best = Some((bytes, scheme));
            }
        };

        // Delta.
        let mut delta_bytes = 0usize;
        let mut last = 0i64;
        for (i, &v) in values.iter().enumerate() {
            let d = if i == 0 { v } else { v.wrapping_sub(last) };
            delta_bytes += uvarint_len(zigzag(d));
            last = v;
        }
        // The delta scheme seeds `last` from the field's most recent warmup value (supplied by the
        // caller when the control record is built), so the estimate above intentionally treats the
        // first sampled value as a delta-from-zero placeholder; it is a size estimate, not the seed.
        consider(delta_bytes, CommittedScheme::delta(self.is_float, *values.last().unwrap()));

        // Frame-of-reference around the midpoint of the observed range.
        let base = midpoint(values);
        let for_bytes: usize = values
            .iter()
            .map(|&v| uvarint_len(zigzag(v.wrapping_sub(base))))
            .sum();
        consider(for_bytes, CommittedScheme::frame_of_reference(self.is_float, base));

        // Dictionary, only when cardinality is low.
        if let Some(table) = build_dict_table(values) {
            let dict_bytes: usize = values
                .iter()
                .map(|&v| {
                    // code 0 is the escape; real entries are 1-based.
                    let code = table.iter().position(|&t| t == v).unwrap() as u64 + 1;
                    uvarint_len(code)
                })
                .sum();
            consider(dict_bytes, CommittedScheme::dict(self.is_float, table));
        }

        // The KV-IR member is zstd-compressed, but the writer cannot measure the post-zstd size, so
        // it estimates raw bytes. Raw savings do not always survive zstd: canonical fixed-width
        // integers keep their high-order bytes constant, which zstd already crushes, whereas compact
        // varints are less regular and can compress worse. To stay on the right side of that, only
        // switch a field that is genuinely wide (mostly 4- or 8-byte canonical values) AND whose
        // adaptive form is a large raw reduction (at most half) — the regime where the win is big
        // enough to survive zstd (e.g. an 8-byte near-constant-delta counter collapsing to a
        // one-byte delta). Marginal switches on small, oscillating fields are left canonical.
        let count = values.len();
        let plain_avg_width = plain as f64 / count as f64;
        match best {
            Some((bytes, scheme)) if plain_avg_width >= 4.0 && bytes.saturating_mul(2) <= plain => {
                Some(scheme)
            }
            _ => None,
        }
    }
}

/// Returns the integer a float exactly represents, or `None` for non-integers, NaN, infinities, and
/// integers outside the exactly-representable `f64` range.
fn float_as_int(value: f64) -> Option<i64> {
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    if value < i64::MIN as f64 || value >= 9_223_372_036_854_775_808.0 {
        return None;
    }
    let as_int = value as i64;
    // Guard against magnitudes above 2^53 where the cast is lossy.
    if as_int as f64 == value {
        Some(as_int)
    } else {
        None
    }
}

/// Canonical KV-IR integer payload width (bytes, excluding the tag) for `value`.
fn canonical_int_width(value: i64) -> usize {
    if i8::try_from(value).is_ok() {
        1
    } else if i16::try_from(value).is_ok() {
        2
    } else if i32::try_from(value).is_ok() {
        4
    } else {
        8
    }
}

/// Length in bytes of the unsigned LEB128 encoding of `value`.
fn uvarint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

/// Midpoint of the min/max of `values`, computed in `i128` to avoid overflow.
fn midpoint(values: &[i64]) -> i64 {
    let mut min = values[0] as i128;
    let mut max = min;
    for &v in &values[1..] {
        let v = v as i128;
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    (min + (max - min) / 2) as i64
}

/// Builds a dictionary table (first-seen order) if the distinct cardinality is at most
/// [`DICT_MAX_CARDINALITY`], else `None`.
fn build_dict_table(values: &[i64]) -> Option<Vec<i64>> {
    let mut table: Vec<i64> = Vec::new();
    for &v in values {
        if !table.contains(&v) {
            if table.len() == DICT_MAX_CARDINALITY {
                return None;
            }
            table.push(v);
        }
    }
    Some(table)
}

// ---------------------------------------------------------------------------
// committed scheme: the per-field encode/decode state after a switch
// ---------------------------------------------------------------------------

/// The byte-plan for one adaptive value payload, produced read-only by
/// [`CommittedScheme::plan_payload`] and written into the event stage by [`write_plan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PayloadPlan {
    /// A single zigzag varint (delta / frame-of-reference offset).
    Svarint(i64),
    /// A single unsigned varint (a 1-based dictionary code).
    Uvarint(u64),
    /// Dictionary escape: `uvarint(0)` then the literal value as a zigzag varint.
    DictEscape(i64),
}

/// Writes a [`PayloadPlan`] (the bytes after [`ADAPTIVE_VALUE_TAG`]) into a growable `out`. The
/// production writer uses the alloc-free [`write_tagged_value`]; this `Vec` form is the reference
/// used by the codec round-trip tests.
#[cfg(test)]
pub(crate) fn write_plan(out: &mut Vec<u8>, plan: PayloadPlan) {
    match plan {
        PayloadPlan::Svarint(value) => write_svarint(out, value),
        PayloadPlan::Uvarint(value) => write_uvarint(out, value),
        PayloadPlan::DictEscape(value) => {
            write_uvarint(out, 0);
            write_svarint(out, value);
        }
    }
}

/// Maximum bytes an adaptive value occupies on the wire: the [`ADAPTIVE_VALUE_TAG`] plus the widest
/// payload (a dictionary escape: `uvarint(0)` = 1 byte, then a 10-byte zigzag varint).
pub(crate) const MAX_ADAPTIVE_VALUE_LEN: usize = 1 + 1 + 10;

/// Writes an unsigned LEB128 varint into `out` starting at `pos`, returning the new position.
fn put_uvarint(out: &mut [u8], mut pos: usize, mut value: u64) -> usize {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out[pos] = byte;
            return pos + 1;
        }
        out[pos] = byte | 0x80;
        pos += 1;
    }
}

/// Writes [`ADAPTIVE_VALUE_TAG`] followed by `plan` into the front of `out` and returns the number
/// of bytes written. `out` must be at least [`MAX_ADAPTIVE_VALUE_LEN`] long. This lets the KV-IR
/// writer stage an adaptive value through a fixed stack buffer, with no per-value heap allocation
/// and no borrow of the serializer's own buffers.
pub(crate) fn write_tagged_value(out: &mut [u8], plan: PayloadPlan) -> usize {
    out[0] = ADAPTIVE_VALUE_TAG;
    match plan {
        PayloadPlan::Svarint(value) => put_uvarint(out, 1, zigzag(value)),
        PayloadPlan::Uvarint(value) => put_uvarint(out, 1, value),
        PayloadPlan::DictEscape(value) => {
            let pos = put_uvarint(out, 1, 0);
            put_uvarint(out, pos, zigzag(value))
        }
    }
}

/// A field's committed adaptive encoding, held on both the writer (to encode) and the reader (to
/// decode). Constructed by [`WarmupBuffer::decide`] on the write side, or from a decoded
/// [`ControlRecord`] on the read side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommittedScheme {
    scheme: Scheme,
    is_float: bool,
    /// Delta: the running previous value (seeded from warmup, advanced on every encode/decode).
    /// Unused by other schemes.
    last: i64,
    /// Frame-of-reference base. Unused by other schemes.
    base: i64,
    /// Dictionary table (1-based codes; code 0 is the literal escape). Empty for other schemes.
    table: Vec<i64>,
}

impl CommittedScheme {
    fn delta(is_float: bool, seed_last: i64) -> Self {
        Self {
            scheme: Scheme::Delta,
            is_float,
            last: seed_last,
            base: 0,
            table: Vec::new(),
        }
    }

    fn frame_of_reference(is_float: bool, base: i64) -> Self {
        Self {
            scheme: Scheme::For,
            is_float,
            last: 0,
            base,
            table: Vec::new(),
        }
    }

    fn dict(is_float: bool, table: Vec<i64>) -> Self {
        Self {
            scheme: Scheme::Dict,
            is_float,
            last: 0,
            base: 0,
            table,
        }
    }

    pub(crate) fn is_float(&self) -> bool {
        self.is_float
    }

    /// The committed scheme identifier. `Copy`, so a caller can read it and immediately drop the
    /// borrow before pulling the value's payload from a byte stream.
    pub(crate) fn kind(&self) -> Scheme {
        self.scheme
    }

    /// The delta scheme's running previous value.
    pub(crate) fn last(&self) -> i64 {
        self.last
    }

    /// Advances the delta scheme's running previous value (used by a stream decoder that reads the
    /// payload itself rather than through [`Self::decode_int`]).
    pub(crate) fn set_last(&mut self, value: i64) {
        self.last = value;
    }

    /// The frame-of-reference base.
    pub(crate) fn base(&self) -> i64 {
        self.base
    }

    /// Resolves a 1-based dictionary `code` to its table entry, or `None` if out of range. `code`
    /// must be non-zero (zero is the literal escape, handled by the caller).
    pub(crate) fn dict_lookup(&self, code: u64) -> Option<i64> {
        code.checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.table.get(index).copied())
    }

    /// Encodes one integer value's payload (everything after the [`ADAPTIVE_VALUE_TAG`]).
    ///
    /// Reference `Vec`-based single-value encoder, used by the codec round-trip tests. The
    /// production writer instead calls [`Self::plan_payload`] and stages the plan through the
    /// alloc-free [`write_tagged_value`].
    #[cfg(test)]
    pub(crate) fn encode_int(&self, out: &mut Vec<u8>, value: i64) {
        write_plan(out, self.plan_payload(value));
    }

    /// Computes the byte-plan for encoding `value` under this scheme, borrowing only `&self`. The
    /// KV-IR writer calls this while a codec borrow is live, then drops the borrow before writing
    /// the returned [`PayloadPlan`] into the event stage — which keeps the disjoint-field borrows
    /// on the serializer unambiguous.
    pub(crate) fn plan_payload(&self, value: i64) -> PayloadPlan {
        match self.scheme {
            Scheme::Delta => PayloadPlan::Svarint(value.wrapping_sub(self.last)),
            Scheme::For => PayloadPlan::Svarint(value.wrapping_sub(self.base)),
            Scheme::Dict => match self.table.iter().position(|&t| t == value) {
                Some(index) => PayloadPlan::Uvarint(index as u64 + 1),
                None => PayloadPlan::DictEscape(value),
            },
        }
    }

    /// Advances the delta scheme's running `last` after an adaptively-encoded value is committed to
    /// the stream. No-op for schemes without per-value state. Only call this for values actually
    /// emitted with [`ADAPTIVE_VALUE_TAG`]; canonically-escaped values (e.g. a post-warmup
    /// non-integral float) must not advance `last`, and the decoder mirrors that by only advancing
    /// on `0x55` values.
    pub(crate) fn advance(&mut self, value: i64) {
        if self.scheme == Scheme::Delta {
            self.last = value;
        }
    }

    /// Encodes one float value's payload. Floats only reach here after warmup proved every value
    /// integral, but a *post-warmup* value could be non-integral; such a value cannot be
    /// represented adaptively, so the caller must fall back to a canonical float tag instead of
    /// calling this. [`Self::try_float_as_int`] performs that check.
    pub(crate) fn try_float_as_int(value: f64) -> Option<i64> {
        float_as_int(value)
    }

    /// Reference slice-based single-value decoder, used by the codec round-trip tests. The
    /// production reader decodes an adaptive value directly from its input stream (it cannot know
    /// the value's length up front to slice it), mirroring this logic with its own varint readers.
    #[cfg(test)]
    pub(crate) fn decode_int(&mut self, cursor: &mut Cursor<'_>) -> Result<i64, AdaptiveError> {
        match self.scheme {
            Scheme::Delta => {
                let delta = cursor.read_svarint()?;
                let value = self.last.wrapping_add(delta);
                self.last = value;
                Ok(value)
            }
            Scheme::For => Ok(self.base.wrapping_add(cursor.read_svarint()?)),
            Scheme::Dict => {
                let code = cursor.read_uvarint()?;
                if code == 0 {
                    cursor.read_svarint()
                } else {
                    let index = (code - 1) as usize;
                    self.table
                        .get(index)
                        .copied()
                        .ok_or(AdaptiveError::Malformed)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// control record wire format
// ---------------------------------------------------------------------------

/// A decoded adaptive control record: which field switches, and to what scheme.
pub(crate) struct ControlRecord {
    pub(crate) auto_generated: bool,
    pub(crate) node_id: u32,
    pub(crate) scheme: CommittedScheme,
}

/// Flag bits in the control-record header byte.
const FLAG_AUTO: u8 = 0b0000_0001;
const FLAG_FLOAT: u8 = 0b0000_0010;

/// Serializes a control record's body (everything after [`ADAPTIVE_CONTROL_TAG`]) into `out`.
///
/// Layout: `flags:u8`, `node_id:uvarint`, `scheme:u8`, then scheme params:
/// - Delta: `seed_last:svarint`
/// - For:   `base:svarint`
/// - Dict:  `len:uvarint`, `len × entry:svarint`
pub(crate) fn write_control_record(
    out: &mut Vec<u8>,
    auto_generated: bool,
    node_id: u32,
    scheme: &CommittedScheme,
) {
    let mut flags = 0u8;
    if auto_generated {
        flags |= FLAG_AUTO;
    }
    if scheme.is_float {
        flags |= FLAG_FLOAT;
    }
    out.push(flags);
    write_uvarint(out, u64::from(node_id));
    out.push(scheme.scheme as u8);
    match scheme.scheme {
        Scheme::Delta => write_svarint(out, scheme.last),
        Scheme::For => write_svarint(out, scheme.base),
        Scheme::Dict => {
            write_uvarint(out, scheme.table.len() as u64);
            for &entry in &scheme.table {
                write_svarint(out, entry);
            }
        }
    }
}

/// Deserializes a control-record body from `cursor` (positioned just after the unit tag).
pub(crate) fn read_control_record(cursor: &mut Cursor<'_>) -> Result<ControlRecord, AdaptiveError> {
    let flags = cursor.read_u8()?;
    let auto_generated = flags & FLAG_AUTO != 0;
    let is_float = flags & FLAG_FLOAT != 0;
    let node_id = u32::try_from(cursor.read_uvarint()?).map_err(|_| AdaptiveError::Malformed)?;
    let scheme_byte = cursor.read_u8()?;
    let scheme = Scheme::from_u8(scheme_byte).ok_or(AdaptiveError::Malformed)?;
    let committed = match scheme {
        Scheme::Delta => CommittedScheme::delta(is_float, cursor.read_svarint()?),
        Scheme::For => CommittedScheme::frame_of_reference(is_float, cursor.read_svarint()?),
        Scheme::Dict => {
            let len = usize::try_from(cursor.read_uvarint()?).map_err(|_| AdaptiveError::Malformed)?;
            if len > DICT_MAX_CARDINALITY {
                return Err(AdaptiveError::Malformed);
            }
            let mut table = Vec::with_capacity(len);
            for _ in 0..len {
                table.push(cursor.read_svarint()?);
            }
            CommittedScheme::dict(is_float, table)
        }
    };
    Ok(ControlRecord {
        auto_generated,
        node_id,
        scheme: committed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_uvarint(value: u64) {
        let mut buf = Vec::new();
        write_uvarint(&mut buf, value);
        let mut cursor = Cursor::new(&buf);
        assert_eq!(cursor.read_uvarint().unwrap(), value);
        assert_eq!(cursor.consumed(), buf.len());
    }

    #[test]
    fn uvarint_roundtrips_edges() {
        for value in [0, 1, 127, 128, 16383, 16384, u64::MAX, u64::MAX - 1] {
            roundtrip_uvarint(value);
        }
    }

    #[test]
    fn svarint_roundtrips_edges() {
        for value in [0i64, 1, -1, i64::MIN, i64::MAX, -128, 127, 300, -300] {
            let mut buf = Vec::new();
            write_svarint(&mut buf, value);
            let mut cursor = Cursor::new(&buf);
            assert_eq!(cursor.read_svarint().unwrap(), value);
        }
    }

    #[test]
    fn uvarint_rejects_overlong() {
        // 10 continuation bytes then a spill bit past bit 63.
        let bytes = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        let mut cursor = Cursor::new(&bytes);
        assert_eq!(cursor.read_uvarint(), Err(AdaptiveError::Malformed));
    }

    #[test]
    fn uvarint_truncated() {
        let bytes = [0x80u8, 0x80];
        let mut cursor = Cursor::new(&bytes);
        assert_eq!(cursor.read_uvarint(), Err(AdaptiveError::Truncated));
    }

    /// Encode a full value stream through a committed scheme, then decode it back, asserting exact
    /// recovery. Mirrors how the serializer and reader drive the codec per field.
    fn assert_stream_roundtrips(mut enc: CommittedScheme, values: &[i64]) {
        let mut dec = enc.clone();
        for &v in values {
            let mut buf = Vec::new();
            enc.encode_int(&mut buf, v); // read-only
            enc.advance(v); // writer advances after commit
            let mut cursor = Cursor::new(&buf);
            let got = dec.decode_int(&mut cursor).unwrap();
            assert_eq!(got, v, "value {v} did not round-trip under {:?}", enc.scheme);
            assert_eq!(cursor.consumed(), buf.len(), "codec left trailing bytes");
        }
    }

    #[test]
    fn delta_roundtrips_monotonic_and_extremes() {
        let scheme = CommittedScheme::delta(false, 1000);
        assert_stream_roundtrips(scheme, &[1001, 1002, 1005, 1005, 900, 1_000_000, -50]);
        // Extremes: wrapping keeps this exact even across the whole i64 range.
        let scheme = CommittedScheme::delta(false, i64::MIN);
        assert_stream_roundtrips(scheme, &[i64::MAX, i64::MIN, 0, i64::MAX]);
    }

    #[test]
    fn for_roundtrips_around_base() {
        let scheme = CommittedScheme::frame_of_reference(false, 500);
        assert_stream_roundtrips(scheme, &[500, 501, 499, 480, 520, 500, -100000, 100000]);
    }

    #[test]
    fn dict_roundtrips_hits_and_escapes() {
        let scheme = CommittedScheme::dict(false, vec![10, 20, 30]);
        // 10/20/30 are table hits; 99 and 40 escape to literals.
        assert_stream_roundtrips(scheme, &[10, 20, 30, 99, 10, 40, 30]);
    }

    #[test]
    fn decide_picks_delta_for_monotonic() {
        let mut buf = WarmupBuffer::new(false);
        for i in 0..200 {
            buf.push_int(1_000_000_000 + i * 7);
        }
        let scheme = buf.decide().expect("monotonic field should switch");
        assert_eq!(scheme.scheme, Scheme::Delta);
    }

    #[test]
    fn decide_switches_low_cardinality_large_magnitude() {
        let palette = [4_000_000_001_i64, 4_000_000_050, 4_000_000_099];
        let values: Vec<i64> = (0..300).map(|i| palette[i % palette.len()]).collect();
        let mut buf = WarmupBuffer::new(false);
        for &v in &values {
            buf.push_int(v);
        }
        // A dictionary, frame-of-reference, or delta scheme can all beat the 5-byte canonical i64
        // here; whichever the bake-off picks, it must switch (not stay PLAIN) and round-trip.
        let scheme = buf.decide().expect("low-cardinality field should switch");
        assert_stream_roundtrips(scheme, &values);
    }

    #[test]
    fn decide_stays_plain_for_tiny_values() {
        let mut buf = WarmupBuffer::new(false);
        for i in 0..200 {
            buf.push_int((i % 7) as i64); // already 1-byte canonical; nothing to gain.
        }
        assert!(buf.decide().is_none());
    }

    #[test]
    fn decide_stays_plain_for_non_integral_float() {
        let mut buf = WarmupBuffer::new(true);
        for i in 0..200 {
            buf.push_float(i as f64 + 0.5);
        }
        assert!(buf.decide().is_none());
    }

    #[test]
    fn decide_switches_integral_float_field() {
        let mut buf = WarmupBuffer::new(true);
        for i in 0..200 {
            buf.push_float((1_000_000 + i * 3) as f64);
        }
        let scheme = buf.decide().expect("integral float field should switch");
        assert!(scheme.is_float());
    }

    #[test]
    fn float_as_int_guards() {
        assert_eq!(float_as_int(57400.0), Some(57400));
        assert_eq!(float_as_int(-1.0), Some(-1));
        assert_eq!(float_as_int(3.14), None);
        assert_eq!(float_as_int(f64::NAN), None);
        assert_eq!(float_as_int(f64::INFINITY), None);
        // 2^53 is the largest run of consecutive exactly-representable integers; it is itself an
        // exact integer and is accepted.
        assert_eq!(float_as_int(9_007_199_254_740_992.0), Some(9_007_199_254_740_992));
        // 2^63 and beyond do not fit an i64; reject.
        assert_eq!(float_as_int(9_223_372_036_854_775_808.0), None);
        assert_eq!(float_as_int(1.0e30), None);
    }

    #[test]
    fn control_record_roundtrips_every_scheme() {
        let cases = [
            (true, 5u32, CommittedScheme::delta(true, -42)),
            (false, 300, CommittedScheme::frame_of_reference(false, 123456)),
            (false, 1, CommittedScheme::dict(false, vec![10, 20, 30, -5])),
        ];
        for (auto, node_id, scheme) in cases {
            let mut buf = Vec::new();
            write_control_record(&mut buf, auto, node_id, &scheme);
            let mut cursor = Cursor::new(&buf);
            let record = read_control_record(&mut cursor).unwrap();
            assert_eq!(record.auto_generated, auto);
            assert_eq!(record.node_id, node_id);
            assert_eq!(record.scheme, scheme);
            assert_eq!(cursor.consumed(), buf.len());
        }
    }

    #[test]
    fn control_record_rejects_unknown_scheme() {
        let bytes = [0u8, 5, 0x7f]; // flags=0, node_id=5, scheme=0x7f (unknown)
        let mut cursor = Cursor::new(&bytes);
        assert!(matches!(
            read_control_record(&mut cursor),
            Err(AdaptiveError::Malformed)
        ));
    }
}
