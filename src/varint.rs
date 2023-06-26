//! Variable length integer encoding.
//!
//! This module implements the variable length encoding of 32 bit integers, as described in the
//! juliet RFC.

use std::num::{NonZeroU32, NonZeroU8};

use bytemuck::{Pod, Zeroable};

use crate::Outcome::{self, Err, Incomplete, Success};

/// The bitmask to separate the data-follows bit from actual value bits.
const VARINT_MASK: u8 = 0b0111_1111;

/// The only possible error for a varint32 parsing, value overflow.
#[derive(Clone, Copy, Debug)]
pub struct Overflow;

/// A successful parse of a varint32.
///
/// Contains both the decoded value and the bytes consumed.
pub struct ParsedU32 {
    /// The number of bytes consumed by the varint32.
    // Note: The `NonZeroU8` allows for niche optimization of compound types containing this type.
    pub offset: NonZeroU8,
    /// The actual parsed value.
    pub value: u32,
}

/// Decodes a varint32 from the given input.
pub fn decode_varint32(input: &[u8]) -> Outcome<ParsedU32, Overflow> {
    let mut value = 0u32;

    for (idx, &c) in input.iter().enumerate() {
        if idx >= 4 && c & 0b1111_0000 != 0 {
            return Err(Overflow);
        }

        value |= ((c & 0b0111_1111) as u32) << (idx * 7);

        if c & 0b1000_0000 == 0 {
            return Success(ParsedU32 {
                value,
                offset: NonZeroU8::new((idx + 1) as u8).unwrap(),
            });
        }
    }

    // We found no stop bit, so our integer is incomplete.
    Incomplete(NonZeroU32::new(1).unwrap())
}

/// An encoded varint32.
///
/// Internally these are stored as six byte arrays to make passing around convenient. Since the
/// maximum length a 32 bit varint can posses is 5 bytes, the 6th bytes is used to record the
/// length.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Varint32([u8; 6]);

impl Varint32 {
    /// Encode a 32-bit integer to variable length.
    pub const fn encode(mut value: u32) -> Self {
        let mut output = [0u8; 6];
        let mut count = 0;

        while value > 0 {
            output[count] = value as u8 & VARINT_MASK;
            value = value >> 7;
            if value > 0 {
                output[count] |= !VARINT_MASK;
                count += 1;
            }
        }

        output[5] = count as u8;
        Varint32(output)
    }

    /// Returns the number of bytes in the encoded varint.
    pub const fn len(self) -> usize {
        self.0[5] as usize + 1
    }
}

impl AsRef<[u8]> for Varint32 {
    fn as_ref(&self) -> &[u8] {
        &self.0[0..self.len()]
    }
}

#[cfg(test)]
mod tests {
    use bytemuck::Zeroable;
    use proptest::prelude::{any, prop::collection};
    use proptest_attr_macro::proptest;

    use crate::{
        varint::{decode_varint32, Overflow},
        Outcome,
    };

    use super::{ParsedU32, Varint32};

    #[test]
    fn encode_known_values() {
        assert_eq!(Varint32::encode(0x00000000).as_ref(), &[0x00]);
        assert_eq!(Varint32::encode(0x00000040).as_ref(), &[0x40]);
        assert_eq!(Varint32::encode(0x0000007f).as_ref(), &[0x7f]);
        assert_eq!(Varint32::encode(0x00000080).as_ref(), &[0x80, 0x01]);
        assert_eq!(Varint32::encode(0x000000ff).as_ref(), &[0xff, 0x01]);
        assert_eq!(Varint32::encode(0x0000ffff).as_ref(), &[0xff, 0xff, 0x03]);
        assert_eq!(
            Varint32::encode(u32::MAX).as_ref(),
            &[0xff, 0xff, 0xff, 0xff, 0x0f]
        );

        // 0x12345678 = 0b0001   0010001   1010001   0101100   1111000
        //                0001  10010001  11010001  10101100  11111000
        //              0x  01        91        d1        ac        f8

        assert_eq!(
            Varint32::encode(0x12345678).as_ref(),
            &[0xf8, 0xac, 0xd1, 0x91, 0x01]
        );
    }

    #[track_caller]
    fn check_decode(expected: u32, input: &[u8]) {
        let ParsedU32 { offset, value } =
            decode_varint32(input).expect("expected decoding to succeed");
        assert_eq!(expected, value);
        assert_eq!(offset.get() as usize, input.len());

        // Also ensure that all partial outputs yield `Incomplete`.
        let mut l = input.len();

        while l > 1 {
            l -= 1;

            let partial = &input.as_ref()[0..l];
            assert!(matches!(decode_varint32(partial), Outcome::Incomplete(n) if n.get() == 1));
        }
    }

    #[test]
    fn decode_known_values() {
        check_decode(0x00000000, &[0x00]);
        check_decode(0x00000040, &[0x40]);
        check_decode(0x0000007f, &[0x7f]);
        check_decode(0x00000080, &[0x80, 0x01]);
        check_decode(0x000000ff, &[0xff, 0x01]);
        check_decode(0x0000ffff, &[0xff, 0xff, 0x03]);
        check_decode(u32::MAX, &[0xff, 0xff, 0xff, 0xff, 0x0f]);
        check_decode(0xf0000000, &[0x80, 0x80, 0x80, 0x80, 0x0f]);
        check_decode(0x12345678, &[0xf8, 0xac, 0xd1, 0x91, 0x01]);
    }

    #[proptest]
    fn roundtrip_value(value: u32) {
        let encoded = Varint32::encode(value);
        assert_eq!(encoded.len(), encoded.as_ref().len());
        check_decode(value, encoded.as_ref());
    }

    #[test]
    fn check_error_conditions() {
        // Value is too long (no more than 5 bytes allowed).
        assert!(matches!(
            decode_varint32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]),
            Outcome::Err(Overflow)
        ));

        // This behavior should already trigger on the fifth byte.
        assert!(matches!(
            decode_varint32(&[0x80, 0x80, 0x80, 0x80, 0x80]),
            Outcome::Err(Overflow)
        ));

        // Value is too big to be held by a `u32`.
        assert!(matches!(
            decode_varint32(&[0x80, 0x80, 0x80, 0x80, 0x10]),
            Outcome::Err(Overflow)
        ));
    }

    proptest::proptest! {
    #[test]
    fn fuzz_varint(data in collection::vec(any::<u8>(), 0..256)) {
        if let Outcome::Success(ParsedU32{ offset, value }) = decode_varint32(&data) {
            let valid_substring = &data[0..(offset.get() as usize)];
            check_decode(value, valid_substring);
        }
    }}

    #[test]
    fn ensure_is_zeroable() {
        assert_eq!(Varint32::zeroed().as_ref(), Varint32::encode(0).as_ref());
    }
}
