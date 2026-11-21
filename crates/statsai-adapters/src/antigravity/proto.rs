//! A minimal reader for the protobuf wire format.
//!
//! Antigravity stores its records as protobuf blobs and ships no `.proto`
//! files, so this reads fields by number. A length-delimited field is returned
//! as raw bytes and only decoded further when the caller asks for it: the large
//! fields, such as the embedded prompt in `gen_metadata`, are stepped over by
//! their length and never parsed.

use chrono::{DateTime, TimeZone, Utc};

/// One field value as it appears on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireValue<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// A message whose top-level fields were checked to be well formed.
///
/// Checking walks the field keys and lengths of this level only, so it costs
/// little even when a field holds a large nested message. A message that does
/// not parse to its end is rejected as a whole rather than read in part.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Message<'a> {
    data: &'a [u8],
}

impl<'a> Message<'a> {
    pub(crate) fn parse(data: &'a [u8]) -> Option<Self> {
        let mut reader = FieldReader { data, position: 0 };
        while reader.position < data.len() {
            reader.next_field()?;
        }
        Some(Self { data })
    }

    pub(crate) fn fields(&self) -> impl Iterator<Item = (u32, WireValue<'a>)> + 'a {
        let mut reader = FieldReader {
            data: self.data,
            position: 0,
        };
        std::iter::from_fn(move || {
            if reader.position >= reader.data.len() {
                return None;
            }
            reader.next_field()
        })
    }

    /// The last varint of field `number`, as protobuf reads a repeated scalar.
    pub(crate) fn varint(&self, number: u32) -> Option<u64> {
        self.fields()
            .filter(|(field, _)| *field == number)
            .filter_map(|(_, value)| match value {
                WireValue::Varint(value) => Some(value),
                _ => None,
            })
            .last()
    }

    /// Every length-delimited value of field `number`, in order.
    pub(crate) fn bytes_fields(&self, number: u32) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.fields()
            .filter(move |(field, _)| *field == number)
            .filter_map(|(_, value)| match value {
                WireValue::Bytes(bytes) => Some(bytes),
                _ => None,
            })
    }

    /// The last length-delimited value of field `number`.
    pub(crate) fn bytes(&self, number: u32) -> Option<&'a [u8]> {
        self.bytes_fields(number).last()
    }

    /// Every well-formed nested message of field `number`, in order.
    pub(crate) fn messages(&self, number: u32) -> impl Iterator<Item = Message<'a>> + 'a {
        self.bytes_fields(number).filter_map(Message::parse)
    }

    /// The last nested message of field `number`, when it is well formed.
    pub(crate) fn message(&self, number: u32) -> Option<Message<'a>> {
        self.bytes(number).and_then(Message::parse)
    }

    /// The last value of field `number` as a non-empty UTF-8 string.
    pub(crate) fn string(&self, number: u32) -> Option<&'a str> {
        self.bytes(number)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    /// Field `number` read as a `google.protobuf.Timestamp`.
    pub(crate) fn timestamp(&self, number: u32) -> Option<DateTime<Utc>> {
        let timestamp = self.message(number)?;
        let seconds = i64::try_from(timestamp.varint(1).unwrap_or(0)).ok()?;
        let nanos = u32::try_from(timestamp.varint(2).unwrap_or(0)).ok()?;
        if seconds <= 0 || nanos >= 1_000_000_000 {
            return None;
        }
        Utc.timestamp_opt(seconds, nanos).single()
    }
}

struct FieldReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> FieldReader<'a> {
    fn next_field(&mut self) -> Option<(u32, WireValue<'a>)> {
        let key = self.varint()?;
        let number = u32::try_from(key >> 3).ok().filter(|number| *number > 0)?;
        let value = match key & 0x7 {
            0 => WireValue::Varint(self.varint()?),
            1 => WireValue::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            2 => {
                let length = usize::try_from(self.varint()?).ok()?;
                WireValue::Bytes(self.take(length)?)
            }
            5 => WireValue::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().ok()?)),
            // Groups (3 and 4) are obsolete and never appear in these records;
            // anything else is not protobuf.
            _ => return None,
        };
        Some((number, value))
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.data.get(self.position)?;
            self.position += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.position.checked_add(length)?;
        let bytes = self.data.get(self.position..end)?;
        self.position = end;
        Some(bytes)
    }
}
