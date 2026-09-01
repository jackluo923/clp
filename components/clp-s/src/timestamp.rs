//! Current-format CLP-S timestamp-pattern compilation and marshalling.
//!
//! CLP-S v0.5 stores resolved timestamp patterns beside epoch-nanosecond column values. This
//! module implements the complete resolved marshalling grammar used by the C++ extractor. The
//! ingestion-only capture-and-transform directives (`\Z`, `\?`, `\P`, `\O{...}`, and `\s`) are
//! deliberately rejected: the C++ parser resolves them before writing a pattern to an archive,
//! and the C++ marshaller cannot format them either.
//!
//! The unrelated pre-v0.5 `clp_s::TimestampPattern` format is not supported here.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::ops::Range;

const NANOSECONDS_PER_SECOND: i128 = 1_000_000_000;
const SECONDS_PER_DAY: i128 = 86_400;
const NANOSECONDS_PER_DAY: i128 = SECONDS_PER_DAY * NANOSECONDS_PER_SECOND;

/// Resource limits applied while compiling a timestamp pattern.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampPatternLimits {
    pattern_bytes: usize,
    formatted_bytes: usize,
}

impl TimestampPatternLimits {
    /// Creates explicit limits for raw and formatted pattern sizes.
    #[must_use]
    pub const fn new(max_pattern_bytes: usize, max_formatted_bytes: usize) -> Self {
        Self {
            pattern_bytes: max_pattern_bytes,
            formatted_bytes: max_formatted_bytes,
        }
    }

    /// Maximum accepted UTF-8 bytes in a raw pattern.
    #[must_use]
    pub const fn max_pattern_bytes(self) -> usize {
        self.pattern_bytes
    }

    /// Maximum possible bytes in one value formatted by the compiled pattern.
    #[must_use]
    pub const fn max_formatted_bytes(self) -> usize {
        self.formatted_bytes
    }
}

impl Default for TimestampPatternLimits {
    fn default() -> Self {
        Self::new(64 * 1024, 64 * 1024)
    }
}

/// The C++ marshalling path selected by a compiled pattern.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampRepresentation {
    /// Calendar date and time in the pattern's fixed timezone.
    DateTime,
    /// An epoch integer, fractional component, or literal-only pattern.
    Numeric,
}

/// A validated, resolved CLP-S v0.5 timestamp pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimestampPattern {
    raw: String,
    tokens: Vec<Token>,
    representation: TimestampRepresentation,
    timezone_offset_minutes: i32,
    quoted: bool,
    max_formatted_size: usize,
}

impl TimestampPattern {
    /// Compiles one resolved, current-format timestamp pattern.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampPatternError`] for invalid characters, escapes, bracket lists,
    /// timezones, duplicate or incompatible directives, unresolved ingestion-only directives,
    /// resource-limit violations, or failed bounded allocations.
    pub fn compile(
        raw: &str,
        limits: TimestampPatternLimits,
    ) -> Result<Self, TimestampPatternError> {
        if raw.len() > limits.pattern_bytes {
            return Err(TimestampPatternError::PatternTooLong {
                actual: raw.len(),
                limit: limits.pattern_bytes,
            });
        }

        let quoted = raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"');
        let content_start = usize::from(quoted);
        let content_end = raw.len() - usize::from(quoted);
        let mut compiler = PatternCompiler::new(raw, limits, content_start, content_end)?;
        compiler.compile()?;
        let compilation = compiler.finish(quoted)?;

        let mut owned = String::new();
        owned.try_reserve_exact(raw.len()).map_err(|_| {
            TimestampPatternError::AllocationFailed {
                requested: raw.len(),
            }
        })?;
        owned.push_str(raw);

        Ok(Self {
            raw: owned,
            tokens: compilation.tokens,
            representation: compilation.representation,
            timezone_offset_minutes: compilation.timezone_offset_minutes,
            quoted,
            max_formatted_size: compilation.max_formatted_size,
        })
    }

    /// Returns the raw current-format pattern exactly as compiled.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Returns whether the raw pattern includes surrounding JSON quotes.
    #[must_use]
    pub const fn is_quoted(&self) -> bool {
        self.quoted
    }

    /// Returns the date-time or numeric C++ marshalling path selected by this pattern.
    #[must_use]
    pub const fn representation(&self) -> TimestampRepresentation {
        self.representation
    }

    /// Returns the fixed timezone offset applied to calendar fields, in minutes east of UTC.
    #[must_use]
    pub const fn timezone_offset_minutes(&self) -> i32 {
        self.timezone_offset_minutes
    }

    /// Returns an upper bound on bytes appended for one timestamp.
    #[must_use]
    pub const fn max_formatted_size(&self) -> usize {
        self.max_formatted_size
    }

    /// Appends an epoch-nanosecond value using this pattern.
    ///
    /// The buffer is restored to its original length on every error. Capacity is reserved once
    /// before formatting; the formatting path itself performs no temporary heap allocation.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampFormatError`] when the output size overflows, capacity cannot be
    /// reserved, a variable fractional directive cannot represent zero, or a two-digit-year
    /// directive cannot represent the timestamp's year using C++ semantics.
    pub fn append_epoch_nanoseconds(
        &self,
        timestamp: i64,
        buffer: &mut String,
    ) -> Result<(), TimestampFormatError> {
        buffer
            .len()
            .checked_add(self.max_formatted_size)
            .ok_or(TimestampFormatError::OutputSizeOverflow)?;
        buffer.try_reserve(self.max_formatted_size).map_err(|_| {
            TimestampFormatError::AllocationFailed {
                requested_additional: self.max_formatted_size,
            }
        })?;

        let original_len = buffer.len();
        let result = self.append_tokens(timestamp, buffer);
        if result.is_err() {
            buffer.truncate(original_len);
        }
        result
    }

    /// Formats an epoch-nanosecond value into a new string.
    ///
    /// Prefer [`Self::append_epoch_nanoseconds`] in record loops to reuse caller-owned capacity.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::append_epoch_nanoseconds`].
    pub fn format_epoch_nanoseconds(&self, timestamp: i64) -> Result<String, TimestampFormatError> {
        let mut buffer = String::new();
        self.append_epoch_nanoseconds(timestamp, &mut buffer)?;
        Ok(buffer)
    }

    fn append_tokens(
        &self,
        timestamp: i64,
        buffer: &mut String,
    ) -> Result<(), TimestampFormatError> {
        let date_time = match self.representation {
            TimestampRepresentation::DateTime => Some(DateTimeParts::from_epoch_nanoseconds(
                timestamp,
                self.timezone_offset_minutes,
            )?),
            TimestampRepresentation::Numeric => None,
        };

        for token in &self.tokens {
            self.append_token(token, timestamp, date_time, buffer)?;
        }
        Ok(())
    }

    fn append_token(
        &self,
        token: &Token,
        timestamp: i64,
        date_time: Option<DateTimeParts>,
        buffer: &mut String,
    ) -> Result<(), TimestampFormatError> {
        match token {
            Token::Literal(range) | Token::Timezone(range) => {
                append_raw_range(&self.raw, range, buffer)
            }
            Token::EscapedBackslash => {
                buffer.push_str(r"\\");
                Ok(())
            }
            Token::YearInCentury => append_year_in_century(require_date(date_time)?.year, buffer),
            Token::Year => append_positive_padded(require_date(date_time)?.year, 4, '0', buffer),
            Token::MonthName(ranges) => append_name(
                &self.raw,
                ranges,
                usize::try_from(require_date(date_time)?.month - 1)
                    .map_err(|_| TimestampFormatError::InvalidCompiledPattern)?,
                buffer,
            ),
            Token::Month => {
                append_positive_padded(i64::from(require_date(date_time)?.month), 2, '0', buffer)
            }
            Token::Day => {
                append_positive_padded(i64::from(require_date(date_time)?.day), 2, '0', buffer)
            }
            Token::SpacePaddedDay => {
                append_positive_padded(i64::from(require_date(date_time)?.day), 2, ' ', buffer)
            }
            Token::WeekdayName(ranges) => append_name(
                &self.raw,
                ranges,
                usize::try_from(require_date(date_time)?.weekday)
                    .map_err(|_| TimestampFormatError::InvalidCompiledPattern)?,
                buffer,
            ),
            Token::PartOfDay => {
                buffer.push_str(if require_date(date_time)?.hour >= 12 {
                    "PM"
                } else {
                    "AM"
                });
                Ok(())
            }
            Token::Hour24 => {
                append_positive_padded(i64::from(require_date(date_time)?.hour), 2, '0', buffer)
            }
            Token::SpacePaddedHour24 => {
                append_positive_padded(i64::from(require_date(date_time)?.hour), 2, ' ', buffer)
            }
            Token::Hour12 => append_hour12(require_date(date_time)?.hour, '0', buffer),
            Token::SpacePaddedHour12 => append_hour12(require_date(date_time)?.hour, ' ', buffer),
            Token::Minute => {
                append_positive_padded(i64::from(require_date(date_time)?.minute), 2, '0', buffer)
            }
            Token::Second => {
                append_positive_padded(i64::from(require_date(date_time)?.second), 2, '0', buffer)
            }
            Token::LeapSecond => append_positive_padded(60, 2, '0', buffer),
            Token::Milliseconds => {
                append_fraction(fractional_nanoseconds(timestamp, date_time)?, 3, buffer)
            }
            Token::Microseconds => {
                append_fraction(fractional_nanoseconds(timestamp, date_time)?, 6, buffer)
            }
            Token::Nanoseconds => {
                append_fraction(fractional_nanoseconds(timestamp, date_time)?, 9, buffer)
            }
            Token::VariableFraction => {
                append_variable_fraction(fractional_nanoseconds(timestamp, date_time)?, buffer)
            }
            Token::EpochSeconds => append_i64(timestamp / 1_000_000_000, buffer),
            Token::EpochMilliseconds => append_i64(timestamp / 1_000_000, buffer),
            Token::EpochMicroseconds => append_i64(timestamp / 1_000, buffer),
            Token::EpochNanoseconds => append_i64(timestamp, buffer),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Literal(Range<usize>),
    YearInCentury,
    Year,
    MonthName(Box<[Range<usize>]>),
    Month,
    Day,
    SpacePaddedDay,
    WeekdayName(Box<[Range<usize>]>),
    PartOfDay,
    Hour24,
    SpacePaddedHour24,
    Hour12,
    SpacePaddedHour12,
    Minute,
    Second,
    LeapSecond,
    Milliseconds,
    Microseconds,
    Nanoseconds,
    VariableFraction,
    EpochSeconds,
    EpochMilliseconds,
    EpochMicroseconds,
    EpochNanoseconds,
    Timezone(Range<usize>),
    EscapedBackslash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectiveClass {
    Date,
    TwelveHour,
    PartOfDay,
    Numeric,
    Neutral,
}

struct PatternCompiler<'a> {
    raw: &'a str,
    limits: TimestampPatternLimits,
    content_start: usize,
    content_end: usize,
    tokens: Vec<Token>,
    seen: [bool; 256],
    directive_flags: u8,
    timezone_offset_minutes: i32,
}

const USES_DATE: u8 = 1 << 0;
const USES_NUMERIC: u8 = 1 << 1;
const USES_TWELVE_HOUR: u8 = 1 << 2;
const HAS_PART_OF_DAY: u8 = 1 << 3;

struct CompiledPattern {
    tokens: Vec<Token>,
    representation: TimestampRepresentation,
    timezone_offset_minutes: i32,
    max_formatted_size: usize,
}

impl<'a> PatternCompiler<'a> {
    fn new(
        raw: &'a str,
        limits: TimestampPatternLimits,
        content_start: usize,
        content_end: usize,
    ) -> Result<Self, TimestampPatternError> {
        let mut tokens = Vec::new();
        tokens
            .try_reserve(raw.len())
            .map_err(|_| TimestampPatternError::AllocationFailed {
                requested: raw.len(),
            })?;
        if 0 != content_start {
            tokens.push(Token::Literal(0..content_start));
        }
        Ok(Self {
            raw,
            limits,
            content_start,
            content_end,
            tokens,
            seen: [false; 256],
            directive_flags: 0,
            timezone_offset_minutes: 0,
        })
    }

    fn compile(&mut self) -> Result<(), TimestampPatternError> {
        let bytes = self.raw.as_bytes();
        let mut cursor = self.content_start;
        let mut literal_start = cursor;
        while cursor < self.content_end {
            let byte = bytes[cursor];
            if b'\\' != byte {
                validate_literal_byte(cursor, byte)?;
                cursor += 1;
                continue;
            }

            self.push_literal(literal_start, cursor);
            let directive_index = cursor
                .checked_add(1)
                .ok_or(TimestampPatternError::PatternSizeOverflow)?;
            if directive_index >= self.content_end {
                return Err(TimestampPatternError::DanglingEscape { index: cursor });
            }
            let directive = bytes[directive_index];
            cursor = self.compile_directive(directive_index, directive)?;
            literal_start = cursor;
        }
        self.push_literal(literal_start, self.content_end);
        Ok(())
    }

    fn finish(mut self, quoted: bool) -> Result<CompiledPattern, TimestampPatternError> {
        if self.has_flag(USES_DATE) && self.has_flag(USES_NUMERIC) {
            return Err(TimestampPatternError::MixedRepresentations);
        }
        if self.has_flag(USES_TWELVE_HOUR) != self.has_flag(HAS_PART_OF_DAY) {
            return Err(TimestampPatternError::IncompleteTwelveHourClock);
        }
        if quoted {
            self.tokens
                .push(Token::Literal(self.content_end..self.raw.len()));
        }
        let max_formatted_size = calculate_max_formatted_size(self.raw, &self.tokens)?;
        if max_formatted_size > self.limits.formatted_bytes {
            return Err(TimestampPatternError::FormattedValueTooLong {
                minimum_limit: max_formatted_size,
                limit: self.limits.formatted_bytes,
            });
        }
        let representation = if self.has_flag(USES_DATE) {
            TimestampRepresentation::DateTime
        } else {
            TimestampRepresentation::Numeric
        };
        Ok(CompiledPattern {
            tokens: self.tokens,
            representation,
            timezone_offset_minutes: self.timezone_offset_minutes,
            max_formatted_size,
        })
    }

    fn compile_directive(
        &mut self,
        directive_index: usize,
        directive: u8,
    ) -> Result<usize, TimestampPatternError> {
        reject_unsupported_json_escape(directive_index, directive)?;
        if b'\\' != directive && self.seen[usize::from(directive)] {
            return Err(TimestampPatternError::DuplicateDirective {
                index: directive_index,
                directive,
            });
        }
        self.seen[usize::from(directive)] = true;

        if let Some((token, class)) = simple_directive(directive) {
            self.apply_class(class);
            self.tokens.push(token);
            return directive_index
                .checked_add(1)
                .ok_or(TimestampPatternError::PatternSizeOverflow);
        }
        match directive {
            b'B' => self.compile_names(directive_index, 12, true),
            b'A' => self.compile_names(directive_index, 7, false),
            b'z' => self.compile_timezone(directive_index),
            b'Z' | b'?' | b'P' | b'O' | b's' => {
                Err(TimestampPatternError::UnresolvedCatDirective {
                    index: directive_index,
                    directive,
                })
            }
            b'\\' => {
                self.tokens.push(Token::EscapedBackslash);
                directive_index
                    .checked_add(1)
                    .ok_or(TimestampPatternError::PatternSizeOverflow)
            }
            _ => Err(TimestampPatternError::InvalidEscapeSequence {
                index: directive_index,
                directive,
            }),
        }
    }

    fn compile_names(
        &mut self,
        directive_index: usize,
        expected: usize,
        months: bool,
    ) -> Result<usize, TimestampPatternError> {
        let bracket_start = directive_index
            .checked_add(1)
            .ok_or(TimestampPatternError::PatternSizeOverflow)?;
        let (content, next) = extract_bracket_content(self.raw, bracket_start, self.content_end)?;
        let ranges = parse_name_list(self.raw, content, expected, directive_index)?;
        self.tokens.push(if months {
            Token::MonthName(ranges)
        } else {
            Token::WeekdayName(ranges)
        });
        self.directive_flags |= USES_DATE;
        Ok(next)
    }

    fn compile_timezone(&mut self, directive_index: usize) -> Result<usize, TimestampPatternError> {
        let bracket_start = directive_index
            .checked_add(1)
            .ok_or(TimestampPatternError::PatternSizeOverflow)?;
        let (content, next) = extract_bracket_content(self.raw, bracket_start, self.content_end)?;
        self.timezone_offset_minutes = parse_timezone_offset(self.raw, content.clone()).ok_or(
            TimestampPatternError::InvalidTimezone {
                index: directive_index,
            },
        )?;
        self.tokens.push(Token::Timezone(content));
        self.directive_flags |= USES_DATE;
        Ok(next)
    }

    fn push_literal(&mut self, start: usize, end: usize) {
        if start < end {
            self.tokens.push(Token::Literal(start..end));
        }
    }

    const fn apply_class(&mut self, class: DirectiveClass) {
        match class {
            DirectiveClass::Date => self.directive_flags |= USES_DATE,
            DirectiveClass::TwelveHour => {
                self.directive_flags |= USES_DATE | USES_TWELVE_HOUR;
            }
            DirectiveClass::PartOfDay => {
                self.directive_flags |= USES_DATE | HAS_PART_OF_DAY;
            }
            DirectiveClass::Numeric => self.directive_flags |= USES_NUMERIC,
            DirectiveClass::Neutral => {}
        }
    }

    const fn has_flag(&self, flag: u8) -> bool {
        0 != self.directive_flags & flag
    }
}

const fn simple_directive(directive: u8) -> Option<(Token, DirectiveClass)> {
    let date = DirectiveClass::Date;
    let neutral = DirectiveClass::Neutral;
    let numeric = DirectiveClass::Numeric;
    Some(match directive {
        b'y' => (Token::YearInCentury, date),
        b'Y' => (Token::Year, date),
        b'm' => (Token::Month, date),
        b'd' => (Token::Day, date),
        b'e' => (Token::SpacePaddedDay, date),
        b'p' => (Token::PartOfDay, DirectiveClass::PartOfDay),
        b'H' => (Token::Hour24, date),
        b'k' => (Token::SpacePaddedHour24, date),
        b'I' => (Token::Hour12, DirectiveClass::TwelveHour),
        b'l' => (Token::SpacePaddedHour12, DirectiveClass::TwelveHour),
        b'M' => (Token::Minute, date),
        b'S' => (Token::Second, date),
        b'J' => (Token::LeapSecond, date),
        b'3' => (Token::Milliseconds, neutral),
        b'6' => (Token::Microseconds, neutral),
        b'9' => (Token::Nanoseconds, neutral),
        b'T' => (Token::VariableFraction, neutral),
        b'E' => (Token::EpochSeconds, numeric),
        b'L' => (Token::EpochMilliseconds, numeric),
        b'C' => (Token::EpochMicroseconds, numeric),
        b'N' => (Token::EpochNanoseconds, numeric),
        _ => return None,
    })
}

const fn validate_literal_byte(index: usize, byte: u8) -> Result<(), TimestampPatternError> {
    if b'"' == byte || byte <= 0x1f {
        Err(TimestampPatternError::InvalidCharacter { index, byte })
    } else {
        Ok(())
    }
}

const fn reject_unsupported_json_escape(
    index: usize,
    directive: u8,
) -> Result<(), TimestampPatternError> {
    if matches!(directive, b'"' | b'b' | b'f' | b'n' | b'r' | b't' | b'u') {
        Err(TimestampPatternError::UnsupportedJsonEscape { index, directive })
    } else {
        Ok(())
    }
}

fn extract_bracket_content(
    raw: &str,
    bracket_start: usize,
    content_end: usize,
) -> Result<(Range<usize>, usize), TimestampPatternError> {
    let bytes = raw.as_bytes();
    if bytes.get(bracket_start) != Some(&b'{') {
        return Err(TimestampPatternError::InvalidBracketPattern {
            index: bracket_start,
        });
    }
    let content_start = bracket_start
        .checked_add(1)
        .ok_or(TimestampPatternError::PatternSizeOverflow)?;
    for (cursor, byte) in bytes
        .iter()
        .copied()
        .enumerate()
        .take(content_end)
        .skip(content_start)
    {
        match byte {
            b'\\' => {
                return Err(TimestampPatternError::InvalidBracketPattern {
                    index: bracket_start,
                });
            }
            b'}' if cursor == content_start => {
                return Err(TimestampPatternError::InvalidBracketPattern {
                    index: bracket_start,
                });
            }
            b'}' => {
                let next = cursor
                    .checked_add(1)
                    .ok_or(TimestampPatternError::PatternSizeOverflow)?;
                return Ok((content_start..cursor, next));
            }
            _ => {}
        }
    }
    Err(TimestampPatternError::InvalidBracketPattern {
        index: bracket_start,
    })
}

fn parse_name_list(
    raw: &str,
    content: Range<usize>,
    expected: usize,
    directive_index: usize,
) -> Result<Box<[Range<usize>]>, TimestampPatternError> {
    if content.len() > usize::from(u16::MAX) {
        return Err(TimestampPatternError::NameListTooLong {
            index: directive_index,
            actual: content.len(),
            limit: usize::from(u16::MAX),
        });
    }
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(expected)
        .map_err(|_| TimestampPatternError::AllocationFailed {
            requested: expected,
        })?;
    let bytes = raw.as_bytes();
    let mut entry_start = content.start;
    for cursor in content.clone() {
        match bytes[cursor] {
            b' ' | b'\\' => {
                return Err(TimestampPatternError::InvalidNameListEntry { index: cursor });
            }
            b',' => {
                if cursor == entry_start {
                    return Err(TimestampPatternError::InvalidNameListEntry { index: cursor });
                }
                ranges.push(entry_start..cursor);
                entry_start = cursor
                    .checked_add(1)
                    .ok_or(TimestampPatternError::PatternSizeOverflow)?;
            }
            _ => {}
        }
    }
    if entry_start >= content.end {
        return Err(TimestampPatternError::InvalidNameListEntry { index: content.end });
    }
    ranges.push(entry_start..content.end);
    if ranges.len() != expected {
        return Err(TimestampPatternError::UnexpectedNameCount {
            index: directive_index,
            actual: ranges.len(),
            expected,
        });
    }
    Ok(ranges.into_boxed_slice())
}

fn parse_timezone_offset(raw: &str, content: Range<usize>) -> Option<i32> {
    let timezone = raw.get(content)?;
    let bytes = timezone.as_bytes();
    let (sign, mut cursor) = if bytes.starts_with(b"+") {
        (1_i32, 1)
    } else if bytes.starts_with(b"-") {
        (-1, 1)
    } else if bytes.starts_with("\u{2212}".as_bytes()) {
        (-1, "\u{2212}".len())
    } else {
        return None;
    };
    let hours = parse_two_digits(bytes.get(cursor..cursor.checked_add(2)?)?)?;
    if hours > 23 {
        return None;
    }
    cursor = cursor.checked_add(2)?;
    if cursor == bytes.len() {
        return Some(sign * i32::from(hours) * 60);
    }
    if bytes.get(cursor) == Some(&b':') {
        cursor = cursor.checked_add(1)?;
    }
    let minutes = parse_two_digits(bytes.get(cursor..cursor.checked_add(2)?)?)?;
    cursor = cursor.checked_add(2)?;
    if cursor != bytes.len() || minutes > 59 {
        return None;
    }
    Some(sign * (i32::from(hours) * 60 + i32::from(minutes)))
}

const fn parse_two_digits(bytes: &[u8]) -> Option<u8> {
    if 2 != bytes.len() || !bytes[0].is_ascii_digit() || !bytes[1].is_ascii_digit() {
        return None;
    }
    Some((bytes[0] - b'0') * 10 + (bytes[1] - b'0'))
}

fn calculate_max_formatted_size(
    raw: &str,
    tokens: &[Token],
) -> Result<usize, TimestampPatternError> {
    let mut size = 0_usize;
    for token in tokens {
        let token_size = match token {
            Token::Literal(range) | Token::Timezone(range) => range.len(),
            Token::YearInCentury | Token::Milliseconds => 3,
            Token::Year => 4,
            Token::MonthName(ranges) | Token::WeekdayName(ranges) => ranges
                .iter()
                .map(Range::len)
                .max()
                .ok_or(TimestampPatternError::PatternSizeOverflow)?,
            Token::Month
            | Token::Day
            | Token::SpacePaddedDay
            | Token::PartOfDay
            | Token::Hour24
            | Token::SpacePaddedHour24
            | Token::Hour12
            | Token::SpacePaddedHour12
            | Token::Minute
            | Token::Second
            | Token::LeapSecond
            | Token::EscapedBackslash => 2,
            Token::Microseconds => 6,
            Token::Nanoseconds | Token::VariableFraction => 9,
            Token::EpochSeconds
            | Token::EpochMilliseconds
            | Token::EpochMicroseconds
            | Token::EpochNanoseconds => 20,
        };
        size = size
            .checked_add(token_size)
            .ok_or(TimestampPatternError::PatternSizeOverflow)?;
    }
    // Ensure every retained range was a valid UTF-8 boundary while the source borrow is available.
    for token in tokens {
        match token {
            Token::Literal(range) | Token::Timezone(range) => {
                raw.get(range.clone())
                    .ok_or(TimestampPatternError::PatternSizeOverflow)?;
            }
            Token::MonthName(ranges) | Token::WeekdayName(ranges) => {
                for range in ranges {
                    raw.get(range.clone())
                        .ok_or(TimestampPatternError::PatternSizeOverflow)?;
                }
            }
            _ => {}
        }
    }
    Ok(size)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DateTimeParts {
    year: i64,
    month: u32,
    day: u32,
    weekday: u32,
    hour: u32,
    minute: u32,
    second: u32,
    nanosecond: u32,
}

impl DateTimeParts {
    fn from_epoch_nanoseconds(
        timestamp: i64,
        timezone_offset_minutes: i32,
    ) -> Result<Self, TimestampFormatError> {
        let adjusted = i128::from(timestamp)
            .checked_add(
                i128::from(timezone_offset_minutes)
                    .checked_mul(60 * NANOSECONDS_PER_SECOND)
                    .ok_or(TimestampFormatError::DateArithmeticOverflow)?,
            )
            .ok_or(TimestampFormatError::DateArithmeticOverflow)?;
        let days = adjusted.div_euclid(NANOSECONDS_PER_DAY);
        let day_nanoseconds = adjusted.rem_euclid(NANOSECONDS_PER_DAY);
        let (year, month, day) = civil_from_days(days)?;
        let weekday = u32::try_from((days + 4).rem_euclid(7))
            .map_err(|_| TimestampFormatError::DateArithmeticOverflow)?;

        let seconds = day_nanoseconds / NANOSECONDS_PER_SECOND;
        let nanosecond = u32::try_from(day_nanoseconds % NANOSECONDS_PER_SECOND)
            .map_err(|_| TimestampFormatError::DateArithmeticOverflow)?;
        let hour = u32::try_from(seconds / 3_600)
            .map_err(|_| TimestampFormatError::DateArithmeticOverflow)?;
        let minute = u32::try_from((seconds % 3_600) / 60)
            .map_err(|_| TimestampFormatError::DateArithmeticOverflow)?;
        let second = u32::try_from(seconds % 60)
            .map_err(|_| TimestampFormatError::DateArithmeticOverflow)?;
        Ok(Self {
            year,
            month,
            day,
            weekday,
            hour,
            minute,
            second,
            nanosecond,
        })
    }
}

fn civil_from_days(days_since_epoch: i128) -> Result<(i64, u32, u32), TimestampFormatError> {
    let shifted = days_since_epoch
        .checked_add(719_468)
        .ok_or(TimestampFormatError::DateArithmeticOverflow)?;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i128::from(month <= 2);
    Ok((
        i64::try_from(year).map_err(|_| TimestampFormatError::DateArithmeticOverflow)?,
        u32::try_from(month).map_err(|_| TimestampFormatError::DateArithmeticOverflow)?,
        u32::try_from(day).map_err(|_| TimestampFormatError::DateArithmeticOverflow)?,
    ))
}

const fn require_date(
    date_time: Option<DateTimeParts>,
) -> Result<DateTimeParts, TimestampFormatError> {
    match date_time {
        Some(parts) => Ok(parts),
        None => Err(TimestampFormatError::InvalidCompiledPattern),
    }
}

fn append_raw_range(
    raw: &str,
    range: &Range<usize>,
    buffer: &mut String,
) -> Result<(), TimestampFormatError> {
    let value = raw
        .get(range.clone())
        .ok_or(TimestampFormatError::InvalidCompiledPattern)?;
    buffer.push_str(value);
    Ok(())
}

fn append_name(
    raw: &str,
    ranges: &[Range<usize>],
    index: usize,
    buffer: &mut String,
) -> Result<(), TimestampFormatError> {
    let range = ranges
        .get(index)
        .ok_or(TimestampFormatError::InvalidCompiledPattern)?;
    append_raw_range(raw, range, buffer)
}

fn append_year_in_century(year: i64, buffer: &mut String) -> Result<(), TimestampFormatError> {
    let value = if year >= 2_000 {
        year - 2_000
    } else {
        year.checked_sub(1_900)
            .ok_or(TimestampFormatError::DateArithmeticOverflow)?
    };
    if value < 0 {
        return Err(TimestampFormatError::TwoDigitYearNotRepresentable { year });
    }
    append_positive_padded(value, 2, '0', buffer)
}

fn append_hour12(
    hour: u32,
    padding: char,
    buffer: &mut String,
) -> Result<(), TimestampFormatError> {
    let modulo = hour % 12;
    append_positive_padded(
        i64::from(if 0 == modulo { 12 } else { modulo }),
        2,
        padding,
        buffer,
    )
}

fn fractional_nanoseconds(
    timestamp: i64,
    date_time: Option<DateTimeParts>,
) -> Result<u32, TimestampFormatError> {
    date_time.map_or_else(
        || {
            u32::try_from((timestamp % 1_000_000_000).unsigned_abs())
                .map_err(|_| TimestampFormatError::DateArithmeticOverflow)
        },
        |parts| Ok(parts.nanosecond),
    )
}

fn append_fraction(
    nanoseconds: u32,
    digits: usize,
    buffer: &mut String,
) -> Result<(), TimestampFormatError> {
    let divisor = match digits {
        3 => 1_000_000,
        6 => 1_000,
        9 => 1,
        _ => return Err(TimestampFormatError::InvalidCompiledPattern),
    };
    append_positive_padded(i64::from(nanoseconds / divisor), digits, '0', buffer)
}

fn append_variable_fraction(
    nanoseconds: u32,
    buffer: &mut String,
) -> Result<(), TimestampFormatError> {
    if 0 == nanoseconds {
        return Err(TimestampFormatError::ZeroVariableFraction);
    }
    let mut digits = [b'0'; 9];
    let mut value = nanoseconds;
    for digit in digits.iter_mut().rev() {
        *digit = b'0'
            + u8::try_from(value % 10).map_err(|_| TimestampFormatError::InvalidCompiledPattern)?;
        value /= 10;
    }
    let end = digits
        .iter()
        .rposition(|digit| b'0' != *digit)
        .and_then(|index| index.checked_add(1))
        .ok_or(TimestampFormatError::ZeroVariableFraction)?;
    append_ascii_digits(&digits[..end], buffer)
}

fn append_positive_padded(
    value: i64,
    minimum_width: usize,
    padding: char,
    buffer: &mut String,
) -> Result<(), TimestampFormatError> {
    let value = u64::try_from(value)
        .map_err(|_| TimestampFormatError::InvalidPositiveDateComponent { value })?;
    let mut digits = [0_u8; 20];
    let start = write_u64_digits(value, &mut digits)?;
    for _ in digits.len() - start..minimum_width {
        buffer.push(padding);
    }
    append_ascii_digits(&digits[start..], buffer)
}

fn append_i64(value: i64, buffer: &mut String) -> Result<(), TimestampFormatError> {
    if value < 0 {
        buffer.push('-');
    }
    let mut digits = [0_u8; 20];
    let start = write_u64_digits(value.unsigned_abs(), &mut digits)?;
    append_ascii_digits(&digits[start..], buffer)
}

fn write_u64_digits(mut value: u64, digits: &mut [u8; 20]) -> Result<usize, TimestampFormatError> {
    let mut cursor = digits.len();
    loop {
        cursor -= 1;
        digits[cursor] = b'0'
            + u8::try_from(value % 10).map_err(|_| TimestampFormatError::InvalidCompiledPattern)?;
        value /= 10;
        if 0 == value {
            return Ok(cursor);
        }
    }
}

fn append_ascii_digits(digits: &[u8], buffer: &mut String) -> Result<(), TimestampFormatError> {
    let digits =
        std::str::from_utf8(digits).map_err(|_| TimestampFormatError::InvalidCompiledPattern)?;
    buffer.push_str(digits);
    Ok(())
}

/// Failure to compile a resolved current-format timestamp pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampPatternError {
    /// The raw pattern exceeds the configured byte limit.
    PatternTooLong { actual: usize, limit: usize },
    /// The pattern's maximum formatted value exceeds the configured limit.
    FormattedValueTooLong { minimum_limit: usize, limit: usize },
    /// Checked pattern-size arithmetic overflowed.
    PatternSizeOverflow,
    /// A bounded pattern allocation could not be reserved.
    AllocationFailed { requested: usize },
    /// A quote or ASCII control byte appeared outside valid surrounding quotes.
    InvalidCharacter { index: usize, byte: u8 },
    /// A trailing backslash had no directive byte.
    DanglingEscape { index: usize },
    /// A JSON escape that is not a CLP-S timestamp directive was used.
    UnsupportedJsonEscape { index: usize, directive: u8 },
    /// The byte following a backslash is not a current-format directive.
    InvalidEscapeSequence { index: usize, directive: u8 },
    /// A non-repeatable directive appeared more than once.
    DuplicateDirective { index: usize, directive: u8 },
    /// A bracket directive was missing valid nonempty `{...}` content.
    InvalidBracketPattern { index: usize },
    /// A month or weekday name list exceeds the C++ `u16` representation.
    NameListTooLong {
        index: usize,
        actual: usize,
        limit: usize,
    },
    /// A month or weekday name was empty or contained a forbidden space or backslash.
    InvalidNameListEntry { index: usize },
    /// A month or weekday directive contained the wrong number of names.
    UnexpectedNameCount {
        index: usize,
        actual: usize,
        expected: usize,
    },
    /// A fixed timezone was not `+HH`, `-HH`, `+HHMM`, `-HHMM`, or a colon variant.
    InvalidTimezone { index: usize },
    /// Date-time and epoch-numeric directives appeared in one pattern.
    MixedRepresentations,
    /// A twelve-hour directive and `\p` were not both present.
    IncompleteTwelveHourClock,
    /// An ingestion-only CAT directive was not resolved before archive marshalling.
    UnresolvedCatDirective { index: usize, directive: u8 },
}

impl Display for TimestampPatternError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PatternTooLong { actual, limit } => {
                write!(
                    formatter,
                    "timestamp pattern size {actual} exceeds limit {limit}"
                )
            }
            Self::FormattedValueTooLong {
                minimum_limit,
                limit,
            } => write!(
                formatter,
                "timestamp pattern may format {minimum_limit} bytes, exceeding limit {limit}"
            ),
            Self::PatternSizeOverflow => formatter.write_str("timestamp pattern size overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve bounded timestamp-pattern allocation of {requested} elements"
            ),
            Self::InvalidCharacter { index, byte } => write!(
                formatter,
                "invalid byte 0x{byte:02x} at timestamp-pattern offset {index}"
            ),
            Self::DanglingEscape { index } => write!(
                formatter,
                "timestamp pattern ends with a backslash at offset {index}"
            ),
            Self::UnsupportedJsonEscape { index, directive } => write!(
                formatter,
                "unsupported JSON escape \\{} at timestamp-pattern offset {index}",
                char::from(*directive)
            ),
            Self::InvalidEscapeSequence { index, directive } => write!(
                formatter,
                "invalid timestamp escape \\{} at offset {index}",
                char::from(*directive)
            ),
            Self::DuplicateDirective { index, directive } => write!(
                formatter,
                "timestamp directive \\{} is repeated at offset {index}",
                char::from(*directive)
            ),
            Self::InvalidBracketPattern { index } => write!(
                formatter,
                "invalid timestamp bracket pattern beginning at offset {index}"
            ),
            Self::NameListTooLong {
                index,
                actual,
                limit,
            } => write!(
                formatter,
                "timestamp name list at offset {index} has {actual} bytes, exceeding limit {limit}"
            ),
            Self::InvalidNameListEntry { index } => {
                write!(
                    formatter,
                    "invalid timestamp name-list entry at offset {index}"
                )
            }
            Self::UnexpectedNameCount {
                index,
                actual,
                expected,
            } => write!(
                formatter,
                "timestamp name list at offset {index} has {actual} entries; expected {expected}"
            ),
            Self::InvalidTimezone { index } => {
                write!(formatter, "invalid timestamp timezone at offset {index}")
            }
            Self::MixedRepresentations => formatter.write_str(
                "timestamp pattern mixes calendar date-time and epoch-numeric directives",
            ),
            Self::IncompleteTwelveHourClock => {
                formatter.write_str("timestamp pattern must pair a twelve-hour directive with \\p")
            }
            Self::UnresolvedCatDirective { index, directive } => write!(
                formatter,
                "ingestion-only timestamp directive \\{} at offset {index} must be resolved \
                 before archive marshalling",
                char::from(*directive)
            ),
        }
    }
}

impl Error for TimestampPatternError {}

/// Failure to append a compiled timestamp value.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampFormatError {
    /// Existing-buffer and pattern sizes overflowed `usize`.
    OutputSizeOverflow,
    /// Output capacity could not be reserved before formatting.
    AllocationFailed { requested_additional: usize },
    /// Checked calendar arithmetic overflowed.
    DateArithmeticOverflow,
    /// A `\y` directive cannot represent this pre-1900 year using C++ semantics.
    TwoDigitYearNotRepresentable { year: i64 },
    /// A date component expected to be nonnegative was negative.
    InvalidPositiveDateComponent { value: i64 },
    /// `\T` cannot represent a zero fractional component.
    ZeroVariableFraction,
    /// A private compiled-pattern invariant was violated.
    InvalidCompiledPattern,
}

impl Display for TimestampFormatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputSizeOverflow => formatter.write_str("formatted timestamp size overflow"),
            Self::AllocationFailed {
                requested_additional,
            } => write!(
                formatter,
                "could not reserve {requested_additional} bytes for a formatted timestamp"
            ),
            Self::DateArithmeticOverflow => {
                formatter.write_str("timestamp calendar arithmetic overflow")
            }
            Self::TwoDigitYearNotRepresentable { year } => {
                write!(formatter, "year {year} cannot be formatted by \\y")
            }
            Self::InvalidPositiveDateComponent { value } => write!(
                formatter,
                "negative date component {value} cannot be formatted as an unsigned field"
            ),
            Self::ZeroVariableFraction => {
                formatter.write_str("\\T cannot format a zero fractional component")
            }
            Self::InvalidCompiledPattern => {
                formatter.write_str("compiled timestamp pattern invariant violated")
            }
        }
    }
}

impl Error for TimestampFormatError {}

#[cfg(test)]
mod tests {
    use super::*;

    type FormattingVector = (&'static str, &'static str, i64);

    const CPP_DATE_TIME_VECTORS: &[FormattingVector] = &[
        (
            "2015-02-01T01:02:03.004",
            r"\Y-\m-\dT\H:\M:\S.\3",
            1_422_752_523_004_000_000,
        ),
        (
            "2015-02-01T01:02:03.004005",
            r"\Y-\m-\dT\H:\M:\S.\6",
            1_422_752_523_004_005_000,
        ),
        (
            "2015-02-01T01:02:03.004005006",
            r"\Y-\m-\dT\H:\M:\S.\9",
            1_422_752_523_004_005_006,
        ),
        (
            "2015-02-01T01:02:03,004",
            r"\Y-\m-\dT\H:\M:\S,\3",
            1_422_752_523_004_000_000,
        ),
        (
            "[2015-02-01T01:02:03",
            r"[\Y-\m-\dT\H:\M:\S",
            1_422_752_523_000_000_000,
        ),
        (
            "[20150201-01:02:03]",
            r"[\Y\m\d-\H:\M:\S]",
            1_422_752_523_000_000_000,
        ),
        (
            "2015-02-01 01:02:03,004",
            r"\Y-\m-\d \H:\M:\S,\3",
            1_422_752_523_004_000_000,
        ),
        (
            "[2015-02-01 01:02:03,004]",
            r"[\Y-\m-\d \H:\M:\S,\3]",
            1_422_752_523_004_000_000,
        ),
        (
            "2015/02/01 01:02:03",
            r"\Y/\m/\d \H:\M:\S",
            1_422_752_523_000_000_000,
        ),
        (
            "15/02/01 01:02:03",
            r"\y/\m/\d \H:\M:\S",
            1_422_752_523_000_000_000,
        ),
        (
            "150201  1:02:03",
            r"\y\m\d \k:\M:\S",
            1_422_752_523_000_000_000,
        ),
        (
            "01 Feb 2015 01:02:03,004",
            r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \Y \H:\M:\S,\3",
            1_422_752_523_004_000_000,
        ),
        (
            "Feb 01, 2015  1:02:03 AM",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \l:\M:\S \p",
            1_422_752_523_000_000_000,
        ),
        (
            "Feb 01, 2015 01:02:03 AM",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \I:\M:\S \p",
            1_422_752_523_000_000_000,
        ),
        (
            "Feb 01, 2015 12:02:03 AM",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \l:\M:\S \p",
            1_422_748_923_000_000_000,
        ),
        (
            "Feb 01, 2015 12:02:03 PM",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \l:\M:\S \p",
            1_422_792_123_000_000_000,
        ),
        (
            "February 01, 2015 01:02",
            concat!(
                r"\B{January,February,March,April,May,June,July,August,September,October,",
                r"November,December} \d, \Y \H:\M"
            ),
            1_422_752_520_000_000_000,
        ),
        (
            "[01/Feb/2015:01:02:03",
            r"[\d/\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec}/\Y:\H:\M:\S",
            1_422_752_523_000_000_000,
        ),
        (
            "Sun Feb  1 01:02:03 2015",
            concat!(
                r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat} ",
                r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \e \H:\M:\S \Y"
            ),
            1_422_752_523_000_000_000,
        ),
        (
            "<<<2015-02-01 01:02:03:004",
            r"<<<\Y-\m-\d \H:\M:\S:\3",
            1_422_752_523_004_000_000,
        ),
        (
            "Jan 21 11:56:42",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S",
            1_771_002_000_000_000,
        ),
        (
            "01-21 11:56:42.392",
            r"\m-\d \H:\M:\S.\3",
            1_771_002_392_000_000,
        ),
        (
            "2015/01/31T15:50:45.123",
            r"\Y/\m/\dT\H:\M:\S.\3",
            1_422_719_445_123_000_000,
        ),
        (
            "2015-01-31T15:50:45",
            r"\Y-\m-\dT\H:\M:\S",
            1_422_719_445_000_000_000,
        ),
        (
            "1895-11-20T21:55:46,010",
            r"\Y-\m-\dT\H:\M:\S,\3",
            -2_338_769_053_990_000_000,
        ),
        (
            "2016-12-31T23:59:59,999Z",
            r"\Y-\m-\dT\H:\M:\S,\3Z",
            1_483_228_799_999_000_000,
        ),
        (
            "2016-12-31T23:59:60,999Z",
            r"\Y-\m-\dT\H:\M:\J,\3Z",
            1_483_228_799_999_000_000,
        ),
        (
            "2017-01-01T00:00:00,999Z",
            r"\Y-\m-\dT\H:\M:\S,\3Z",
            1_483_228_800_999_000_000,
        ),
    ];

    const CPP_NUMERIC_VECTORS: &[FormattingVector] = &[
        ("1762445893", r"\E", 1_762_445_893_000_000_000),
        ("1762445893001", r"\L", 1_762_445_893_001_000_000),
        ("1762445893001002", r"\C", 1_762_445_893_001_002_000),
        ("1762445893001002003", r"\N", 1_762_445_893_001_002_003),
        ("1762445893.001", r"\E.\3", 1_762_445_893_001_000_000),
        ("1762445893.001002", r"\E.\6", 1_762_445_893_001_002_000),
        ("1762445893.001002003", r"\E.\9", 1_762_445_893_001_002_003),
        ("1762445893.001002000", r"\E.\9", 1_762_445_893_001_002_000),
        ("1762445893.00100201", r"\E.\T", 1_762_445_893_001_002_010),
        ("1762445893.1", r"\E.\T", 1_762_445_893_100_000_000),
        ("-1762445893", r"\E", -1_762_445_893_000_000_000),
        ("-1762445893001", r"\L", -1_762_445_893_001_000_000),
        ("-1762445893001002", r"\C", -1_762_445_893_001_002_000),
        ("-1762445893001002003", r"\N", -1_762_445_893_001_002_003),
        ("-1762445893.001", r"\E.\3", -1_762_445_893_001_000_000),
        ("-1762445893.001002", r"\E.\6", -1_762_445_893_001_002_000),
        (
            "-1762445893.001002003",
            r"\E.\9",
            -1_762_445_893_001_002_003,
        ),
        (
            "-1762445893.001002000",
            r"\E.\9",
            -1_762_445_893_001_002_000,
        ),
        ("-1762445893.00100201", r"\E.\T", -1_762_445_893_001_002_010),
        ("-1762445893.1", r"\E.\T", -1_762_445_893_100_000_000),
    ];

    const CPP_TIMEZONE_VECTORS: &[FormattingVector] = &[
        (
            "Jan 21 11:56:42Z",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\SZ",
            1_771_002_000_000_000,
        ),
        (
            "Jan 21 11:56:42 UTC-01",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{-01}",
            1_774_602_000_000_000,
        ),
        (
            "Jan 21 11:56:42 UTC-01:30",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{-01:30}",
            1_776_402_000_000_000,
        ),
        (
            "Jan 21 11:56:42 UTC-0130",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{-0130}",
            1_776_402_000_000_000,
        ),
        (
            "Jan 21 11:56:42 UTC+01",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{+01}",
            1_767_402_000_000_000,
        ),
        (
            "Jan 21 11:56:42 UTC+01:30",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{+01:30}",
            1_765_602_000_000_000,
        ),
        (
            "Jan 21 11:56:42 UTC+0130",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{+0130}",
            1_765_602_000_000_000,
        ),
    ];

    fn assert_vectors(vectors: &[FormattingVector]) {
        for &(expected, raw_pattern, timestamp) in vectors {
            let pattern = TimestampPattern::compile(raw_pattern, TimestampPatternLimits::default())
                .expect("C++ resolved pattern compiles");
            assert_eq!(
                expected,
                pattern
                    .format_epoch_nanoseconds(timestamp)
                    .expect("C++ timestamp vector formats"),
                "pattern {raw_pattern}"
            );

            let quoted_raw = format!("\"{raw_pattern}\"");
            let quoted =
                TimestampPattern::compile(quoted_raw.as_str(), TimestampPatternLimits::default())
                    .expect("quoted C++ pattern compiles");
            assert!(quoted.is_quoted());
            assert_eq!(
                format!("\"{expected}\""),
                quoted
                    .format_epoch_nanoseconds(timestamp)
                    .expect("quoted C++ vector formats"),
                "quoted pattern {quoted_raw}"
            );
        }
    }

    #[test]
    fn formats_cpp_date_time_vectors() {
        assert_vectors(CPP_DATE_TIME_VECTORS);
    }

    #[test]
    fn formats_cpp_numeric_vectors_with_truncating_negative_division() {
        assert_vectors(CPP_NUMERIC_VECTORS);
        let near_zero = TimestampPattern::compile(r"\E.\9", TimestampPatternLimits::default())
            .expect("numeric pattern compiles");
        assert_eq!(
            "0.000000001",
            near_zero
                .format_epoch_nanoseconds(-1)
                .expect("C++ truncates the epoch portion toward zero")
        );
    }

    #[test]
    fn formats_cpp_timezone_vectors() {
        assert_vectors(CPP_TIMEZONE_VECTORS);
        let unicode_minus = TimestampPattern::compile(
            r"\Y-\m-\dT\H:\M:\S \z{−04:30}",
            TimestampPatternLimits::default(),
        )
        .expect("Unicode-minus timezone compiles");
        assert_eq!(-270, unicode_minus.timezone_offset_minutes());
        assert_eq!(
            "1969-12-31T19:30:00 −04:30",
            unicode_minus
                .format_epoch_nanoseconds(0)
                .expect("timezone-adjusted epoch formats")
        );
    }

    #[test]
    fn formats_leap_days_pre_epoch_and_i64_endpoints() {
        let nanosecond_pattern =
            TimestampPattern::compile(r"\Y-\m-\dT\H:\M:\S.\9", TimestampPatternLimits::default())
                .expect("endpoint pattern compiles");
        assert_eq!(
            "2000-02-29T12:34:56.000000000",
            nanosecond_pattern
                .format_epoch_nanoseconds(951_827_696_000_000_000)
                .expect("Gregorian leap day formats")
        );
        assert_eq!(
            "2262-04-11T23:47:16.854775807",
            nanosecond_pattern
                .format_epoch_nanoseconds(i64::MAX)
                .expect("maximum epoch nanoseconds format")
        );
        assert_eq!(
            "1677-09-21T00:12:43.145224192",
            nanosecond_pattern
                .format_epoch_nanoseconds(i64::MIN)
                .expect("minimum epoch nanoseconds format")
        );

        let numeric = TimestampPattern::compile(r"\N", TimestampPatternLimits::default())
            .expect("nanosecond epoch pattern compiles");
        assert_eq!(
            i64::MAX.to_string(),
            numeric.format_epoch_nanoseconds(i64::MAX).unwrap()
        );
        assert_eq!(
            i64::MIN.to_string(),
            numeric.format_epoch_nanoseconds(i64::MIN).unwrap()
        );
    }

    #[test]
    fn appends_without_replacing_existing_content_and_escapes_backslashes() {
        let pattern =
            TimestampPattern::compile(r"\Y\\\m\\\dT\H:\M:\S.\3", TimestampPatternLimits::default())
                .expect("escaped-backslash C++ pattern compiles");
        let mut buffer = String::from("prefix:");
        pattern
            .append_epoch_nanoseconds(1_422_752_523_004_000_000, &mut buffer)
            .expect("timestamp appends");
        assert_eq!(r"prefix:2015\\02\\01T01:02:03.004", buffer);
    }

    #[test]
    fn rejects_invalid_patterns_with_structured_errors() {
        let limits = TimestampPatternLimits::default();
        for raw in ["\"", "abc\"", "\"abc", "\0", "\x01", "\x1f"] {
            assert!(matches!(
                TimestampPattern::compile(raw, limits),
                Err(TimestampPatternError::InvalidCharacter { .. })
            ));
        }
        for raw in [r"\u0000", r"\b", r"\f", r"\n", r"\r", r"\t", r"\u"] {
            assert!(matches!(
                TimestampPattern::compile(raw, limits),
                Err(TimestampPatternError::UnsupportedJsonEscape { .. })
            ));
        }
        assert!(matches!(
            TimestampPattern::compile("abc\\", limits),
            Err(TimestampPatternError::DanglingEscape { .. })
        ));
        assert!(matches!(
            TimestampPattern::compile(r"\Y\Y", limits),
            Err(TimestampPatternError::DuplicateDirective {
                directive: b'Y',
                ..
            })
        ));
        assert!(matches!(
            TimestampPattern::compile(r"\Y\E", limits),
            Err(TimestampPatternError::MixedRepresentations)
        ));
        for raw in [r"\I", r"\p"] {
            assert!(matches!(
                TimestampPattern::compile(raw, limits),
                Err(TimestampPatternError::IncompleteTwelveHourClock)
            ));
        }
    }

    #[test]
    fn validates_name_lists_and_fixed_timezones() {
        let limits = TimestampPatternLimits::default();
        for raw in [
            r"\B{}",
            r"\B{Jan,Feb}",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,}",
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat,Extra}",
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat Sat}",
        ] {
            assert!(TimestampPattern::compile(raw, limits).is_err(), "{raw}");
        }
        for raw in [
            r"\z{+24}",
            r"\z{+01:60}",
            r"\z{+1}",
            r"\z{UTC}",
            r"\z{+010}",
        ] {
            assert!(matches!(
                TimestampPattern::compile(raw, limits),
                Err(TimestampPatternError::InvalidTimezone { .. })
            ));
        }
        for raw in [r"\z{+00}", r"\z{+23:59}", r"\z{-0130}", r"\z{−04}"] {
            assert!(TimestampPattern::compile(raw, limits).is_ok(), "{raw}");
        }
    }

    #[test]
    fn rejects_all_ingestion_only_cat_directives() {
        let limits = TimestampPatternLimits::default();
        for (raw, directive) in [
            (r"\Z", b'Z'),
            (r"\?", b'?'),
            (r"\P", b'P'),
            (r"\O{-/}", b'O'),
            (r"\s", b's'),
        ] {
            assert!(matches!(
                TimestampPattern::compile(raw, limits),
                Err(TimestampPatternError::UnresolvedCatDirective {
                    directive: actual,
                    ..
                }) if actual == directive
            ));
        }
    }

    #[test]
    fn enforces_pattern_and_output_limits() {
        assert!(matches!(
            TimestampPattern::compile("abc", TimestampPatternLimits::new(2, 10)),
            Err(TimestampPatternError::PatternTooLong {
                actual: 3,
                limit: 2
            })
        ));
        assert!(matches!(
            TimestampPattern::compile(r"\N", TimestampPatternLimits::new(10, 19)),
            Err(TimestampPatternError::FormattedValueTooLong {
                minimum_limit: 20,
                limit: 19
            })
        ));
    }

    #[test]
    fn formatting_errors_restore_the_existing_buffer() {
        let variable =
            TimestampPattern::compile(r"before\Tafter", TimestampPatternLimits::default())
                .expect("variable fraction pattern compiles");
        let mut buffer = String::from("retained");
        assert!(matches!(
            variable.append_epoch_nanoseconds(1_000_000_000, &mut buffer),
            Err(TimestampFormatError::ZeroVariableFraction)
        ));
        assert_eq!("retained", buffer);

        let two_digit_year =
            TimestampPattern::compile(r"before\y", TimestampPatternLimits::default())
                .expect("two-digit year pattern compiles");
        assert!(matches!(
            two_digit_year.append_epoch_nanoseconds(-2_338_769_053_990_000_000, &mut buffer),
            Err(TimestampFormatError::TwoDigitYearNotRepresentable { year: 1895 })
        ));
        assert_eq!("retained", buffer);
    }

    #[test]
    fn literal_and_empty_patterns_follow_the_cpp_numeric_path() {
        let literal = TimestampPattern::compile("literal", TimestampPatternLimits::default())
            .expect("literal pattern compiles");
        assert_eq!(TimestampRepresentation::Numeric, literal.representation());
        assert_eq!("literal", literal.format_epoch_nanoseconds(123).unwrap());

        let quoted_empty = TimestampPattern::compile(r#""""#, TimestampPatternLimits::default())
            .expect("quoted empty pattern compiles");
        assert!(quoted_empty.is_quoted());
        assert_eq!(r#""""#, quoted_empty.format_epoch_nanoseconds(123).unwrap());
    }
}
