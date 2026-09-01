use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::{self};

/// Stable node-type discriminants stored in schema trees and unordered schema delimiters.
///
/// Existing values must never be reordered or renumbered. New values may only be appended.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
#[non_exhaustive]
pub enum NodeType {
    Integer = 0,
    Float = 1,
    ClpString = 2,
    VarString = 3,
    Boolean = 4,
    Object = 5,
    UnstructuredArray = 6,
    Null = 7,
    DeprecatedDateString = 8,
    StructuredArray = 9,
    Metadata = 10,
    DeltaInteger = 11,
    FormattedFloat = 12,
    DictionaryFloat = 13,
    Timestamp = 14,
}

impl TryFrom<u8> for NodeType {
    type Error = UnknownNodeType;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Integer),
            1 => Ok(Self::Float),
            2 => Ok(Self::ClpString),
            3 => Ok(Self::VarString),
            4 => Ok(Self::Boolean),
            5 => Ok(Self::Object),
            6 => Ok(Self::UnstructuredArray),
            7 => Ok(Self::Null),
            8 => Ok(Self::DeprecatedDateString),
            9 => Ok(Self::StructuredArray),
            10 => Ok(Self::Metadata),
            11 => Ok(Self::DeltaInteger),
            12 => Ok(Self::FormattedFloat),
            13 => Ok(Self::DictionaryFloat),
            14 => Ok(Self::Timestamp),
            _ => Err(UnknownNodeType(value)),
        }
    }
}

/// A node-type discriminant not understood by this library version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownNodeType(u8);

impl UnknownNodeType {
    /// Returns the unrecognized wire discriminant.
    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl Display for UnknownNodeType {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown structured archive node type {}", self.0)
    }
}

impl Error for UnknownNodeType {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_match_reference_order() {
        let expected = [
            NodeType::Integer,
            NodeType::Float,
            NodeType::ClpString,
            NodeType::VarString,
            NodeType::Boolean,
            NodeType::Object,
            NodeType::UnstructuredArray,
            NodeType::Null,
            NodeType::DeprecatedDateString,
            NodeType::StructuredArray,
            NodeType::Metadata,
            NodeType::DeltaInteger,
            NodeType::FormattedFloat,
            NodeType::DictionaryFloat,
            NodeType::Timestamp,
        ];

        for (wire_value, expected_type) in (0_u8..).zip(expected) {
            assert_eq!(wire_value, expected_type as u8);
            assert_eq!(Ok(expected_type), NodeType::try_from(wire_value));
        }
    }

    #[test]
    fn rejects_unknown_discriminants() {
        assert_eq!(Err(UnknownNodeType(15)), NodeType::try_from(15));
        assert_eq!(Err(UnknownNodeType(u8::MAX)), NodeType::try_from(u8::MAX));
    }
}
