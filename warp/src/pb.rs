//! Protocol Buffers' wire format, read without a schema: a message is its fields, each a
//! number and a value. Warp's task records are messages of `warp.multi_agent.v1`, whose
//! field numbers `task.rs` names; this reads only the wire.

/// One field's value as the wire carries it.
#[derive(Clone, Copy)]
pub enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    /// A 32-bit value, which no field the driver reads holds.
    Fixed32,
}

/// A message's fields, in the order the wire carries them. A message that does not parse
/// whole has the fields before the fault.
#[derive(Clone, Copy, Default)]
pub struct Msg<'a> {
    bytes: &'a [u8],
}

impl<'a> Msg<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Every field, in wire order.
    pub fn fields(self) -> Fields<'a> {
        Fields { rest: self.bytes }
    }

    /// The values of field `number`, in wire order.
    pub fn all(self, number: u32) -> impl Iterator<Item = Wire<'a>> {
        self.fields()
            .filter(move |(n, _)| *n == number)
            .map(|(_, value)| value)
    }

    /// The last value of field `number`, as Protocol Buffers reads a field that is not
    /// repeated.
    pub fn get(self, number: u32) -> Option<Wire<'a>> {
        self.all(number).last()
    }

    pub fn str(self, number: u32) -> &'a str {
        self.strs(number).last().unwrap_or_default()
    }

    /// A repeated string field.
    pub fn strs(self, number: u32) -> impl Iterator<Item = &'a str> {
        self.all(number).filter_map(|value| match value {
            Wire::Bytes(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        })
    }

    pub fn bytes(self, number: u32) -> Option<&'a [u8]> {
        match self.get(number)? {
            Wire::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub fn msg(self, number: u32) -> Option<Msg<'a>> {
        self.bytes(number).map(Msg::new)
    }

    /// A repeated message field.
    pub fn msgs(self, number: u32) -> impl Iterator<Item = Msg<'a>> {
        self.all(number).filter_map(|value| match value {
            Wire::Bytes(bytes) => Some(Msg::new(bytes)),
            _ => None,
        })
    }

    pub fn varint(self, number: u32) -> u64 {
        match self.get(number) {
            Some(Wire::Varint(n)) => n,
            _ => 0,
        }
    }

    /// An `int32`, which the wire carries sign-extended to 64 bits.
    pub fn int32(self, number: u32) -> i32 {
        self.varint(number) as i64 as i32
    }

    pub fn bool(self, number: u32) -> bool {
        self.varint(number) != 0
    }

    pub fn double(self, number: u32) -> Option<f64> {
        match self.get(number)? {
            Wire::Fixed64(bits) => Some(f64::from_bits(bits)),
            _ => None,
        }
    }

    /// The field of a `oneof` among `numbers` that the wire set last, with its value.
    pub fn oneof(self, numbers: impl Fn(u32) -> bool) -> Option<(u32, Wire<'a>)> {
        self.fields().filter(|(n, _)| numbers(*n)).last()
    }

    /// A `google.protobuf.Timestamp` field, in milliseconds since 1970.
    pub fn millis(self, number: u32) -> Option<u64> {
        let at = self.msg(number)?;
        let seconds = at.varint(1) as i64;
        let nanos = at.int32(2);
        let millis = seconds.checked_mul(1000)? + i64::from(nanos / 1_000_000);
        u64::try_from(millis).ok()
    }
}

pub struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.rest.split_first()?;
            self.rest = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest.len() < n {
            return None;
        }
        let (taken, rest) = self.rest.split_at(n);
        self.rest = rest;
        Some(taken)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u32, Wire<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let key = self.varint()?;
        let number = u32::try_from(key >> 3).ok()?;
        let value = match key & 7 {
            0 => Wire::Varint(self.varint()?),
            1 => Wire::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            2 => {
                let n = usize::try_from(self.varint()?).ok()?;
                Wire::Bytes(self.take(n)?)
            }
            5 => {
                self.take(4)?;
                Wire::Fixed32
            }
            // Groups (3, 4) are gone from proto3; a record that has one is not Warp's.
            _ => {
                self.rest = &[];
                return None;
            }
        };
        Some((number, value))
    }
}

/// A `google.protobuf.Struct` as JSON.
pub fn structure(msg: Msg) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for entry in msg.msgs(1) {
        let key = entry.str(1).to_owned();
        let value = entry.msg(2).map(value).unwrap_or(serde_json::Value::Null);
        map.insert(key, value);
    }
    serde_json::Value::Object(map)
}

/// A `google.protobuf.Value` as JSON.
fn value(msg: Msg) -> serde_json::Value {
    use serde_json::Value;
    match msg.oneof(|n| (1..=6).contains(&n)) {
        Some((2, _)) => msg
            .double(2)
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::Null, Value::Number),
        Some((3, _)) => Value::String(msg.str(3).to_owned()),
        Some((4, _)) => Value::Bool(msg.bool(4)),
        Some((5, Wire::Bytes(bytes))) => structure(Msg::new(bytes)),
        Some((6, Wire::Bytes(bytes))) => Value::Array(Msg::new(bytes).msgs(1).map(value).collect()),
        _ => Value::Null,
    }
}
