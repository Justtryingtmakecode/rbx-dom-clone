use std::string::FromUtf8Error;

use thiserror::Error;

use crate::VariantType;

#[derive(Debug, Error)]
pub(crate) enum AttributeError {
    #[error("missing attribute list length")]
    InvalidLength,

    #[error("missing attribute key name")]
    NoKey,

    #[error("attribute key contained invalid UTF-8")]
    KeyBadUnicode(#[source] FromUtf8Error),

    #[error("missing attribute value type")]
    NoValueType,

    #[error("invalid value type: {0}")]
    InvalidValueType(u8),

    #[error("{0:?} values are not supported in attributes")]
    UnsupportedVariantType(VariantType),

    #[error("invalid BrickColor value: {0}")]
    InvalidBrickColor(u32),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("couldn't read bytes to deserialize {0}")]
    ReadType(&'static str),
}
