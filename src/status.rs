//! The UDP packet sent back to `tracking.py`, so its preview can show what
//! the instrument is doing. Little-endian:
//!
//! ```text
//!     u8   version (2)
//!     u8   curled fingers, bit per finger (index..pinky)
//!     u8   flags: bits 0..4 fingers sounding, bit 4 hold, bit 5 fist
//!     f32  height, pinch, roll, pos, depth   (the conducting hand, 0..1)
//!     4 x  u8 len, utf-8 chord name          (one per finger)
//! ```

use crate::{
    gesture::{Gesture, PADS},
    nodes::harmony::Shared,
};

pub const VERSION: u8 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub curled: [bool; PADS],
    pub sounding: [bool; PADS],
    pub hold: bool,
    pub fist: bool,
    pub height: f32,
    pub pinch: f32,
    pub roll: f32,
    pub pos: f32,
    pub depth: f32,
    pub names: [String; PADS],
}

impl Status {
    pub fn new(g: &Gesture, harmony: &Shared) -> Self {
        Self {
            curled: g.gates,
            sounding: harmony.fingers(),
            hold: harmony.holding(),
            fist: g.fist,
            height: g.height,
            pinch: g.pinch,
            roll: g.roll,
            pos: g.pos,
            depth: g.depth,
            names: harmony.names().clone(),
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        let bits = |b: &[bool]| {
            b.iter()
                .enumerate()
                .fold(0u8, |acc, (i, &on)| acc | (on as u8) << i)
        };
        out.clear();
        out.push(VERSION);
        out.push(bits(&self.curled));
        out.push(bits(&self.sounding) | (self.hold as u8) << 4 | (self.fist as u8) << 5);
        for x in [self.height, self.pinch, self.roll, self.pos, self.depth] {
            out.extend(x.to_le_bytes());
        }
        for name in &self.names {
            let name = &name.as_bytes()[..name.len().min(u8::MAX as usize)];
            out.push(name.len() as u8);
            out.extend(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_doc() {
        let status = Status {
            curled: [true, false, false, true],
            sounding: [true, false, false, false],
            hold: true,
            fist: false,
            height: 0.25,
            pinch: 0.5,
            roll: 0.75,
            pos: 0.125,
            depth: 1.0,
            names: ["Am9", "Fmaj9", "Cmaj9", "G9"].map(String::from),
        };
        let mut out = Vec::new();
        status.encode(&mut out);

        assert_eq!(&out[..3], &[VERSION, 0b1001, 0b1_0001]);
        assert_eq!(f32::from_le_bytes(out[3..7].try_into().unwrap()), 0.25);
        assert_eq!(f32::from_le_bytes(out[15..19].try_into().unwrap()), 0.125);
        assert_eq!(&out[23..27], &[3, b'A', b'm', b'9']);
        assert_eq!(out.len(), 3 + 20 + (4 + 6 + 6 + 3));
    }
}
