//! Writes protobuf wire format for test records.

#[derive(Debug, Clone, Default)]
pub(super) struct Pb(Vec<u8>);

impl Pb {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn varint(mut self, field: u32, value: u64) -> Self {
        self.key(field, 0);
        self.raw_varint(value);
        self
    }

    pub(super) fn bytes(mut self, field: u32, value: &[u8]) -> Self {
        self.key(field, 2);
        self.raw_varint(value.len() as u64);
        self.0.extend_from_slice(value);
        self
    }

    pub(super) fn string(self, field: u32, value: &str) -> Self {
        self.bytes(field, value.as_bytes())
    }

    pub(super) fn message(self, field: u32, value: Pb) -> Self {
        self.bytes(field, &value.0)
    }

    pub(super) fn finish(self) -> Vec<u8> {
        self.0
    }

    fn key(&mut self, field: u32, wire_type: u64) {
        self.raw_varint((u64::from(field) << 3) | wire_type);
    }

    fn raw_varint(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.0.push(byte);
                return;
            }
            self.0.push(byte | 0x80);
        }
    }
}
