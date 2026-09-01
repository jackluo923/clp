//! Timestamp-literal resolution for the current CLP-S KQL grammar.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use super::TimestampLiteral;

const NANOSECONDS_PER_SECOND: i128 = 1_000_000_000;
const NANOSECONDS_PER_MILLISECOND: i128 = 1_000_000;
const NANOSECONDS_PER_MICROSECOND: i128 = 1_000;
const SECONDS_PER_DAY: i128 = 86_400;

const DEFAULT_PATTERNS: &[&str] = &[
    r"\Y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s\O{,.}\?\Z",
    r"\Y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s\Z",
    r"\Y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s\O{,.}\?",
    r"\Y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s",
    r"[\Y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s\O{,.}\?]",
    r"[\Y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s]",
    r"[\Y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s",
    r"<<<\Y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s:\?",
    r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \Y \H:\M:\s\O{,.}\?",
    r"[\Y\m\d-\H:\M:\s]",
    r"\y\O{-/}\m\O{-/}\d\O{T }\H:\M:\s",
    r"\y\m\d\O{T }\k:\M:\s",
    r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \l:\M:\s \p",
    r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \I:\M:\s \p",
    concat!(
        r"\B{January,February,March,April,May,June,July,August,September,October,",
        r"November,December} \d, \Y \H:\M"
    ),
    r"[\d\O{-/}\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec}\O{-/}\Y:\H:\M:\s",
    concat!(
        r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat} ",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \e \H:\M:\s \Y"
    ),
    r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\s",
    r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\s\Z",
    r"\m\O{- }\d \H:\M:\s\O{,.}\?",
    r"\P",
    r"\E.\?",
];

/// Failure to resolve a parsed `timestamp(...)` expression to epoch nanoseconds.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampQueryError {
    /// The explicit pattern is structurally invalid.
    InvalidPattern {
        /// Byte offset within the raw pattern.
        offset: usize,
    },
    /// The value cannot be consumed exactly by its explicit pattern or any current default.
    IncompatibleValue,
    /// Parsed calendar fields do not form a real date, or a supplied weekday disagrees.
    InvalidDate,
    /// Scaling or calendar conversion cannot fit the signed epoch-nanosecond domain.
    EpochNanosecondsOutOfRange,
}

impl Display for TimestampQueryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPattern { offset } => {
                write!(formatter, "invalid timestamp pattern at byte {offset}")
            }
            Self::IncompatibleValue => {
                formatter.write_str("timestamp value does not match its pattern")
            }
            Self::InvalidDate => formatter.write_str("timestamp fields do not form a valid date"),
            Self::EpochNanosecondsOutOfRange => {
                formatter.write_str("timestamp does not fit signed epoch nanoseconds")
            }
        }
    }
}

impl Error for TimestampQueryError {}

pub(super) fn resolve_timestamp_literal(
    literal: &TimestampLiteral,
) -> Result<i64, TimestampQueryError> {
    if let Some(pattern) = literal.pattern() {
        return PatternParser::new(literal.value(), pattern).parse();
    }
    for pattern in DEFAULT_PATTERNS {
        if let Ok(timestamp) = PatternParser::new(literal.value(), pattern).parse() {
            return Ok(timestamp);
        }
    }
    Err(TimestampQueryError::IncompatibleValue)
}

#[derive(Clone, Copy)]
struct Components {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    nanosecond: u32,
    weekday: Option<u32>,
    part_of_day: Option<u32>,
    timezone_minutes: i32,
    epoch_nanoseconds: i128,
}

impl Default for Components {
    fn default() -> Self {
        Self {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            nanosecond: 0,
            weekday: None,
            part_of_day: None,
            timezone_minutes: 0,
            epoch_nanoseconds: 0,
        }
    }
}

struct PatternParser<'a> {
    value: &'a str,
    pattern: &'a str,
    value_offset: usize,
    pattern_offset: usize,
    components: Components,
    seen: [bool; 256],
    flags: u8,
}

const USES_DATE: u8 = 1 << 0;
const USES_NUMERIC: u8 = 1 << 1;
const USES_TWELVE_HOUR: u8 = 1 << 2;
const HAS_PART_OF_DAY: u8 = 1 << 3;

impl<'a> PatternParser<'a> {
    const fn new(value: &'a str, pattern: &'a str) -> Self {
        Self {
            value,
            pattern,
            value_offset: 0,
            pattern_offset: 0,
            components: Components {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
                nanosecond: 0,
                weekday: None,
                part_of_day: None,
                timezone_minutes: 0,
                epoch_nanoseconds: 0,
            },
            seen: [false; 256],
            flags: 0,
        }
    }

    fn parse(mut self) -> Result<i64, TimestampQueryError> {
        while self.pattern_offset < self.pattern.len() {
            let byte = self.pattern.as_bytes()[self.pattern_offset];
            if b'\\' != byte {
                self.consume_literal()?;
                continue;
            }
            let directive_offset = self
                .pattern_offset
                .checked_add(1)
                .ok_or(TimestampQueryError::EpochNanosecondsOutOfRange)?;
            let directive = *self.pattern.as_bytes().get(directive_offset).ok_or(
                TimestampQueryError::InvalidPattern {
                    offset: self.pattern_offset,
                },
            )?;
            self.pattern_offset = directive_offset + 1;
            self.mark_directive(directive, directive_offset)?;
            self.consume_directive(directive, directive_offset)?;
        }
        if self.value_offset != self.value.len() {
            return Err(TimestampQueryError::IncompatibleValue);
        }
        if self.has_flag(USES_DATE) && self.has_flag(USES_NUMERIC)
            || self.has_flag(USES_TWELVE_HOUR) != self.has_flag(HAS_PART_OF_DAY)
        {
            return Err(TimestampQueryError::InvalidPattern {
                offset: self.pattern_offset,
            });
        }
        self.finish()
    }

    const fn set_flag(&mut self, flag: u8) {
        self.flags |= flag;
    }

    const fn has_flag(&self, flag: u8) -> bool {
        0 != self.flags & flag
    }

    fn consume_literal(&mut self) -> Result<(), TimestampQueryError> {
        let character = self.pattern[self.pattern_offset..].chars().next().ok_or(
            TimestampQueryError::InvalidPattern {
                offset: self.pattern_offset,
            },
        )?;
        if character == '"' || character <= '\u{1f}' {
            return Err(TimestampQueryError::InvalidPattern {
                offset: self.pattern_offset,
            });
        }
        let size = character.len_utf8();
        let end = self
            .value_offset
            .checked_add(size)
            .ok_or(TimestampQueryError::EpochNanosecondsOutOfRange)?;
        if self.value.as_bytes().get(self.value_offset..end)
            != self
                .pattern
                .as_bytes()
                .get(self.pattern_offset..self.pattern_offset + size)
        {
            return Err(TimestampQueryError::IncompatibleValue);
        }
        self.value_offset = end;
        self.pattern_offset += size;
        Ok(())
    }

    fn mark_directive(&mut self, directive: u8, offset: usize) -> Result<(), TimestampQueryError> {
        if matches!(directive, b'"' | b'b' | b'f' | b'n' | b'r' | b't' | b'u') {
            return Err(TimestampQueryError::InvalidPattern { offset });
        }
        if !matches!(directive, b'O' | b'\\') && self.seen[usize::from(directive)] {
            return Err(TimestampQueryError::InvalidPattern { offset });
        }
        self.seen[usize::from(directive)] = true;
        Ok(())
    }

    fn consume_directive(
        &mut self,
        directive: u8,
        offset: usize,
    ) -> Result<(), TimestampQueryError> {
        match directive {
            b'y' | b'Y' => self.consume_year(directive)?,
            b'B' => {
                self.components.month = self.consume_name_list(offset, 12)? + 1;
                self.set_flag(USES_DATE);
            }
            b'm' => {
                self.components.month = self.consume_bounded_padded(2, b'0', 1, 12)?;
                self.set_flag(USES_DATE);
            }
            b'd' => {
                self.components.day = self.consume_bounded_padded(2, b'0', 1, 31)?;
                self.set_flag(USES_DATE);
            }
            b'e' => {
                self.components.day = self.consume_bounded_padded(2, b' ', 1, 31)?;
                self.set_flag(USES_DATE);
            }
            b'A' => {
                self.components.weekday = Some(self.consume_name_list(offset, 7)?);
                self.set_flag(USES_DATE);
            }
            b'p' => {
                self.components.part_of_day = Some(self.consume_part_of_day()?);
                self.set_flag(USES_DATE | HAS_PART_OF_DAY);
            }
            b'H' => {
                self.components.hour = self.consume_bounded_padded(2, b'0', 0, 23)?;
                self.set_flag(USES_DATE);
            }
            b'k' => {
                self.components.hour = self.consume_bounded_padded(2, b' ', 0, 23)?;
                self.set_flag(USES_DATE);
            }
            b'I' => {
                self.components.hour = self.consume_bounded_padded(2, b'0', 1, 12)?;
                self.set_flag(USES_DATE | USES_TWELVE_HOUR);
            }
            b'l' => {
                self.components.hour = self.consume_bounded_padded(2, b' ', 1, 12)?;
                self.set_flag(USES_DATE | USES_TWELVE_HOUR);
            }
            b'M' => {
                self.components.minute = self.consume_bounded_padded(2, b'0', 0, 59)?;
                self.set_flag(USES_DATE);
            }
            b'S' => {
                self.components.second = self.consume_bounded_padded(2, b'0', 0, 59)?;
                self.set_flag(USES_DATE);
            }
            b'J' => {
                if 60 != self.consume_padded(2, b'0')? {
                    return Err(TimestampQueryError::IncompatibleValue);
                }
                self.components.second = 59;
                self.set_flag(USES_DATE);
            }
            b's' => {
                let second = self.consume_bounded_padded(2, b'0', 0, 60)?;
                self.components.second = second.min(59);
                self.set_flag(USES_DATE);
            }
            b'3' => self.components.nanosecond = self.consume_fraction(3, false)?,
            b'6' => self.components.nanosecond = self.consume_fraction(6, false)?,
            b'9' => self.components.nanosecond = self.consume_fraction(9, false)?,
            b'T' => self.components.nanosecond = self.consume_fraction(9, true)?,
            b'?' => self.components.nanosecond = self.consume_generic_fraction()?,
            b'E' => self.consume_epoch(NANOSECONDS_PER_SECOND)?,
            b'L' => self.consume_epoch(NANOSECONDS_PER_MILLISECOND)?,
            b'C' => self.consume_epoch(NANOSECONDS_PER_MICROSECOND)?,
            b'N' => self.consume_epoch(1)?,
            b'P' => self.consume_unknown_epoch()?,
            b'z' => {
                let timezone = self.take_bracket(offset)?;
                let (consumed, minutes) = parse_timezone_offset(timezone)
                    .map_err(|_| TimestampQueryError::InvalidPattern { offset })?;
                if consumed != timezone.len() {
                    return Err(TimestampQueryError::InvalidPattern { offset });
                }
                if !self.consume_exact(timezone.as_bytes()) {
                    return Err(TimestampQueryError::IncompatibleValue);
                }
                self.components.timezone_minutes = minutes;
                self.set_flag(USES_DATE);
            }
            b'Z' => {
                self.components.timezone_minutes = self.consume_generic_timezone()?;
                self.set_flag(USES_DATE);
            }
            b'O' => self.consume_one_of(offset)?,
            b'\\' => {
                if !self.consume_exact(br"\\") {
                    return Err(TimestampQueryError::IncompatibleValue);
                }
            }
            _ => return Err(TimestampQueryError::InvalidPattern { offset }),
        }
        Ok(())
    }

    fn consume_year(&mut self, directive: u8) -> Result<(), TimestampQueryError> {
        let year = self.consume_padded(if b'y' == directive { 2 } else { 4 }, b'0')?;
        self.components.year = if b'y' == directive {
            if year >= 69 { 1900 + year } else { 2000 + year }
        } else {
            year
        };
        self.set_flag(USES_DATE);
        Ok(())
    }

    fn consume_padded(&mut self, width: usize, padding: u8) -> Result<i64, TimestampQueryError> {
        let bytes = self.take_value(width)?;
        let mut start = 0;
        while start + 1 < bytes.len() && bytes[start] == padding {
            start += 1;
        }
        let digits = &bytes[start..];
        if digits.len() > 1 && digits[0] == b'0' {
            return Err(TimestampQueryError::IncompatibleValue);
        }
        let mut value = 0_i64;
        for byte in digits {
            if !byte.is_ascii_digit() {
                return Err(TimestampQueryError::IncompatibleValue);
            }
            value = value * 10 + i64::from(*byte - b'0');
        }
        Ok(value)
    }

    fn consume_bounded_padded(
        &mut self,
        width: usize,
        padding: u8,
        minimum: u32,
        maximum: u32,
    ) -> Result<u32, TimestampQueryError> {
        let value = u32::try_from(self.consume_padded(width, padding)?)
            .map_err(|_| TimestampQueryError::IncompatibleValue)?;
        if (minimum..=maximum).contains(&value) {
            Ok(value)
        } else {
            Err(TimestampQueryError::IncompatibleValue)
        }
    }

    fn consume_name_list(
        &mut self,
        offset: usize,
        expected: u32,
    ) -> Result<u32, TimestampQueryError> {
        let names = self.take_bracket(offset)?;
        if names.len() > usize::from(u16::MAX) {
            return Err(TimestampQueryError::InvalidPattern { offset });
        }
        let mut count = 0_u32;
        let mut matched = None;
        for name in names.split(',') {
            if name.is_empty()
                || name.as_bytes().contains(&b' ')
                || name.as_bytes().contains(&b'\\')
            {
                return Err(TimestampQueryError::InvalidPattern { offset });
            }
            if matched.is_none() && self.value[self.value_offset..].starts_with(name) {
                matched = Some((count, name.len()));
            }
            count += 1;
        }
        if count != expected {
            return Err(TimestampQueryError::InvalidPattern { offset });
        }
        let (index, length) = matched.ok_or(TimestampQueryError::IncompatibleValue)?;
        self.value_offset += length;
        Ok(index)
    }

    fn consume_part_of_day(&mut self) -> Result<u32, TimestampQueryError> {
        if self.consume_exact(b"AM") {
            Ok(0)
        } else if self.consume_exact(b"PM") {
            Ok(1)
        } else {
            Err(TimestampQueryError::IncompatibleValue)
        }
    }

    fn consume_fraction(
        &mut self,
        maximum_digits: usize,
        variable: bool,
    ) -> Result<u32, TimestampQueryError> {
        let remaining = &self.value.as_bytes()[self.value_offset..];
        let digits = remaining
            .iter()
            .take(maximum_digits)
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let required = if variable { digits } else { maximum_digits };
        if 0 == required || (!variable && digits != maximum_digits) {
            return Err(TimestampQueryError::IncompatibleValue);
        }
        let bytes = self.take_value(required)?;
        if variable && bytes.last() == Some(&b'0') {
            return Err(TimestampQueryError::IncompatibleValue);
        }
        let mut value = 0_u32;
        for byte in bytes {
            value = value * 10 + u32::from(*byte - b'0');
        }
        Ok(value * 10_u32.pow(u32::try_from(9 - required).unwrap_or(0)))
    }

    fn consume_generic_fraction(&mut self) -> Result<u32, TimestampQueryError> {
        let remaining = &self.value.as_bytes()[self.value_offset..];
        let digits = remaining
            .iter()
            .take(9)
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if 0 == digits || (!matches!(digits, 3 | 6 | 9) && remaining.get(digits - 1) == Some(&b'0'))
        {
            Err(TimestampQueryError::IncompatibleValue)
        } else {
            let bytes = self.take_value(digits)?;
            let mut value = 0_u32;
            for byte in bytes {
                value = value * 10 + u32::from(*byte - b'0');
            }
            Ok(value * 10_u32.pow(u32::try_from(9 - digits).unwrap_or(0)))
        }
    }

    fn consume_epoch(&mut self, factor: i128) -> Result<(), TimestampQueryError> {
        let value = i128::from(self.consume_signed_integer()?);
        self.components.epoch_nanoseconds = value
            .checked_mul(factor)
            .ok_or(TimestampQueryError::EpochNanosecondsOutOfRange)?;
        self.set_flag(USES_NUMERIC);
        Ok(())
    }

    fn consume_unknown_epoch(&mut self) -> Result<(), TimestampQueryError> {
        let value = self.consume_signed_integer()?;
        let magnitude = value.unsigned_abs();
        let factor = if magnitude > 31_536_000_000_000_000 {
            1_i128
        } else if magnitude > 31_536_000_000_000 {
            NANOSECONDS_PER_MICROSECOND
        } else if magnitude > 31_536_000_000 {
            NANOSECONDS_PER_MILLISECOND
        } else {
            NANOSECONDS_PER_SECOND
        };
        self.components.epoch_nanoseconds = i128::from(value)
            .checked_mul(factor)
            .ok_or(TimestampQueryError::EpochNanosecondsOutOfRange)?;
        self.set_flag(USES_NUMERIC);
        Ok(())
    }

    fn consume_signed_integer(&mut self) -> Result<i64, TimestampQueryError> {
        let start = self.value_offset;
        let negative = self.value.as_bytes().get(self.value_offset) == Some(&b'-');
        if negative {
            self.value_offset += 1;
        }
        let digit_start = self.value_offset;
        while self
            .value
            .as_bytes()
            .get(self.value_offset)
            .is_some_and(u8::is_ascii_digit)
        {
            self.value_offset += 1;
        }
        if digit_start == self.value_offset {
            return Err(TimestampQueryError::IncompatibleValue);
        }
        let digits = &self.value[digit_start..self.value_offset];
        if (negative && digits.starts_with('0')) || (digits.len() > 1 && digits.starts_with('0')) {
            return Err(TimestampQueryError::IncompatibleValue);
        }
        self.value[start..self.value_offset]
            .parse::<i64>()
            .map_err(|_| TimestampQueryError::EpochNanosecondsOutOfRange)
    }

    fn consume_generic_timezone(&mut self) -> Result<i32, TimestampQueryError> {
        let mut cursor = self.value_offset;
        if self.value.as_bytes().get(cursor) == Some(&b' ') {
            cursor += 1;
        }
        if self.value[cursor..].starts_with("UTC") {
            cursor += 3;
        }
        let mut offset =
            if let Ok((consumed, minutes)) = parse_timezone_offset(&self.value[cursor..]) {
                cursor += consumed;
                Some(minutes)
            } else {
                None
            };
        if self.value.as_bytes().get(cursor) == Some(&b'Z') {
            cursor += 1;
            offset.get_or_insert(0);
        }
        let minutes = offset.ok_or(TimestampQueryError::IncompatibleValue)?;
        self.value_offset = cursor;
        Ok(minutes)
    }

    fn consume_one_of(&mut self, offset: usize) -> Result<(), TimestampQueryError> {
        let alternatives = self.take_bracket(offset)?;
        let byte = *self
            .value
            .as_bytes()
            .get(self.value_offset)
            .ok_or(TimestampQueryError::IncompatibleValue)?;
        if alternatives.as_bytes().contains(&byte) {
            self.value_offset += 1;
            Ok(())
        } else {
            Err(TimestampQueryError::IncompatibleValue)
        }
    }

    fn take_bracket(&mut self, offset: usize) -> Result<&'a str, TimestampQueryError> {
        if self.pattern.as_bytes().get(self.pattern_offset) != Some(&b'{') {
            return Err(TimestampQueryError::InvalidPattern { offset });
        }
        let content_start = self.pattern_offset + 1;
        let suffix = &self.pattern[content_start..];
        let close = suffix
            .find('}')
            .ok_or(TimestampQueryError::InvalidPattern { offset })?;
        let content_end = content_start + close;
        let content = &self.pattern[content_start..content_end];
        if content.is_empty() || content.as_bytes().contains(&b'\\') {
            return Err(TimestampQueryError::InvalidPattern { offset });
        }
        self.pattern_offset = content_end + 1;
        Ok(content)
    }

    fn take_value(&mut self, length: usize) -> Result<&'a [u8], TimestampQueryError> {
        let end = self
            .value_offset
            .checked_add(length)
            .ok_or(TimestampQueryError::EpochNanosecondsOutOfRange)?;
        let value = self
            .value
            .as_bytes()
            .get(self.value_offset..end)
            .ok_or(TimestampQueryError::IncompatibleValue)?;
        self.value_offset = end;
        Ok(value)
    }

    fn consume_exact(&mut self, expected: &[u8]) -> bool {
        let Some(end) = self.value_offset.checked_add(expected.len()) else {
            return false;
        };
        if self.value.as_bytes().get(self.value_offset..end) != Some(expected) {
            return false;
        }
        self.value_offset = end;
        true
    }

    fn finish(mut self) -> Result<i64, TimestampQueryError> {
        if !self.has_flag(USES_DATE) {
            if self.components.epoch_nanoseconds < 0 {
                self.components.epoch_nanoseconds -= i128::from(self.components.nanosecond);
            } else {
                self.components.epoch_nanoseconds += i128::from(self.components.nanosecond);
            }
            return i64::try_from(self.components.epoch_nanoseconds)
                .map_err(|_| TimestampQueryError::EpochNanosecondsOutOfRange);
        }
        if self.has_flag(USES_TWELVE_HOUR) {
            self.components.hour =
                self.components.hour % 12 + self.components.part_of_day.unwrap_or_default() * 12;
        }
        let days = days_from_civil(
            self.components.year,
            self.components.month,
            self.components.day,
        )?;
        if self
            .components
            .weekday
            .is_some_and(|weekday| weekday != u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0))
        {
            return Err(TimestampQueryError::InvalidDate);
        }
        let seconds = days
            .checked_mul(SECONDS_PER_DAY)
            .and_then(|value| value.checked_add(i128::from(self.components.hour) * 3600))
            .and_then(|value| value.checked_add(i128::from(self.components.minute) * 60))
            .and_then(|value| value.checked_add(i128::from(self.components.second)))
            .and_then(|value| value.checked_sub(i128::from(self.components.timezone_minutes) * 60))
            .ok_or(TimestampQueryError::EpochNanosecondsOutOfRange)?;
        let timestamp = seconds
            .checked_mul(NANOSECONDS_PER_SECOND)
            .and_then(|value| value.checked_add(i128::from(self.components.nanosecond)))
            .ok_or(TimestampQueryError::EpochNanosecondsOutOfRange)?;
        i64::try_from(timestamp).map_err(|_| TimestampQueryError::EpochNanosecondsOutOfRange)
    }
}

fn parse_timezone_offset(value: &str) -> Result<(usize, i32), TimestampQueryError> {
    let bytes = value.as_bytes();
    let (sign, mut cursor) = if bytes.starts_with(b"+") {
        (1_i32, 1)
    } else if bytes.starts_with(b"-") {
        (-1, 1)
    } else if bytes.starts_with("\u{2212}".as_bytes()) {
        (-1, "\u{2212}".len())
    } else {
        return Err(TimestampQueryError::IncompatibleValue);
    };
    let hours = parse_two_digits(bytes.get(cursor..cursor + 2))?;
    if hours > 23 {
        return Err(TimestampQueryError::IncompatibleValue);
    }
    cursor += 2;
    let hours_only = (cursor, sign * i32::from(hours) * 60);
    if bytes.get(cursor) == Some(&b':') {
        cursor += 1;
    }
    let Some(minutes_bytes) = bytes.get(cursor..cursor + 2) else {
        return Ok(hours_only);
    };
    let Ok(minutes) = parse_two_digits(Some(minutes_bytes)) else {
        return Ok(hours_only);
    };
    if minutes > 59 {
        return Ok(hours_only);
    }
    cursor += 2;
    Ok((cursor, sign * (i32::from(hours) * 60 + i32::from(minutes))))
}

fn parse_two_digits(bytes: Option<&[u8]>) -> Result<u8, TimestampQueryError> {
    let bytes = bytes.ok_or(TimestampQueryError::IncompatibleValue)?;
    if 2 != bytes.len() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(TimestampQueryError::IncompatibleValue);
    }
    Ok((bytes[0] - b'0') * 10 + bytes[1] - b'0')
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Result<i128, TimestampQueryError> {
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return Err(TimestampQueryError::InvalidDate);
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Ok(i128::from(era * 146_097 + day_of_era - 719_468))
}

const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

const fn is_leap_year(year: i64) -> bool {
    0 == year % 4 && (0 != year % 100 || 0 == year % 400)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(value: &str, pattern: Option<&str>) -> TimestampLiteral {
        TimestampLiteral {
            value: value.to_owned(),
            pattern: pattern.map(str::to_owned),
        }
    }

    #[test]
    fn resolves_cpp_kql_and_search_vectors() {
        let vectors = [
            ("1970-01-01 00:00:00.000000001", None, 1),
            ("1", Some(r"\N"), 1),
            ("1759417024.4", None, 1_759_417_024_400_000_000),
            ("1759417024400", None, 1_759_417_024_400_000_000),
            ("1759417024.299", None, 1_759_417_024_299_000_000),
        ];
        for (value, pattern, expected) in vectors {
            assert_eq!(
                expected,
                resolve_timestamp_literal(&literal(value, pattern)).unwrap(),
                "{value}"
            );
        }
    }

    #[test]
    fn resolves_every_default_pattern_family() {
        let vectors = [
            "2024/02/29T12:34:56.123456789Z",
            "[20240229-12:34:56]",
            "29 Feb 2024 12:34:56.123",
            "Feb 29, 2024 12:34:56 PM",
            "Thu Feb 29 12:34:56 2024",
        ];
        for value in vectors {
            assert!(
                resolve_timestamp_literal(&literal(value, None)).is_ok(),
                "{value}"
            );
        }
    }

    #[test]
    fn rejects_invalid_dates_patterns_and_noncanonical_numeric_values() {
        for (value, pattern) in [
            ("2023-02-29 00:00:00", None),
            ("01", Some(r"\N")),
            ("-0", Some(r"\N")),
            ("1", Some(r"\Y")),
        ] {
            assert!(resolve_timestamp_literal(&literal(value, pattern)).is_err());
        }
        assert!(matches!(
            resolve_timestamp_literal(&literal("1", Some(r"\N\N"))),
            Err(TimestampQueryError::InvalidPattern { .. })
        ));
    }
}
