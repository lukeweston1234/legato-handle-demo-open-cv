use std::fmt;

pub const LANDMARKS: usize = 21;
const HEADER_LEN: usize = 8 + 1;
const HAND_LEN: usize = 1 + 4 + LANDMARKS * 3 * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handedness {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hand {
    pub handedness: Handedness,
    pub score: f32,
    /// Normalized image coords: x/y in [0, 1], z is depth relative to the wrist.
    pub landmarks: [[f32; 3]; LANDMARKS],
}

impl Hand {
    pub const WRIST: usize = 0;
    pub const THUMB_TIP: usize = 4;
    pub const INDEX_TIP: usize = 8;
    pub const MIDDLE_TIP: usize = 12;
    pub const RING_TIP: usize = 16;
    pub const PINKY_TIP: usize = 20;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub timestamp_us: u64,
    pub hands: Vec<Hand>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Length { expected: usize, actual: usize },
    Handedness(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { expected, actual } => {
                write!(f, "expected {expected} bytes, got {actual}")
            }
            Self::Handedness(b) => write!(f, "invalid handedness byte {b}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Little-endian cursor over a buffer whose length has already been checked.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (head, rest) = self.0.split_first_chunk().expect("length pre-checked");
        self.0 = rest;
        *head
    }
    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
    fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.take())
    }
}

impl Frame {
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        if buf.len() < HEADER_LEN {
            return Err(DecodeError::Length {
                expected: HEADER_LEN,
                actual: buf.len(),
            });
        }
        let mut r = Reader(buf);
        let timestamp_us = r.u64();
        let count = r.u8() as usize;

        let expected = HEADER_LEN + count * HAND_LEN;
        if buf.len() != expected {
            return Err(DecodeError::Length {
                expected,
                actual: buf.len(),
            });
        }

        let hands = (0..count)
            .map(|_| {
                let handedness = match r.u8() {
                    0 => Handedness::Left,
                    1 => Handedness::Right,
                    b => return Err(DecodeError::Handedness(b)),
                };
                let score = r.f32();
                let landmarks = std::array::from_fn(|_| [r.f32(), r.f32(), r.f32()]);
                Ok(Hand {
                    handedness,
                    score,
                    landmarks,
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(Self {
            timestamp_us,
            hands,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(frame: &Frame) -> Vec<u8> {
        let mut out = frame.timestamp_us.to_le_bytes().to_vec();
        out.push(frame.hands.len() as u8);
        for hand in &frame.hands {
            out.push(hand.handedness as u8);
            out.extend(hand.score.to_le_bytes());
            out.extend(
                hand.landmarks
                    .iter()
                    .flatten()
                    .flat_map(|c| c.to_le_bytes()),
            );
        }
        out
    }

    #[test]
    fn round_trip() {
        let frame = Frame {
            timestamp_us: 123_456,
            hands: vec![
                Hand {
                    handedness: Handedness::Right,
                    score: 0.9,
                    landmarks: std::array::from_fn(|i| [i as f32, 0.5, -0.1]),
                },
                Hand {
                    handedness: Handedness::Left,
                    score: 0.7,
                    landmarks: [[0.25; 3]; LANDMARKS],
                },
            ],
        };
        let bytes = encode(&frame);
        assert_eq!(bytes.len(), HEADER_LEN + 2 * HAND_LEN);
        assert_eq!(Frame::decode(&bytes), Ok(frame));
    }

    #[test]
    fn rejects_truncated() {
        let mut bytes = encode(&Frame {
            timestamp_us: 0,
            hands: vec![Hand {
                handedness: Handedness::Left,
                score: 1.0,
                landmarks: [[0.0; 3]; LANDMARKS],
            }],
        });
        bytes.pop();
        assert!(matches!(
            Frame::decode(&bytes),
            Err(DecodeError::Length { .. })
        ));
        assert!(matches!(
            Frame::decode(&[0; 3]),
            Err(DecodeError::Length { .. })
        ));
    }

    #[test]
    fn rejects_bad_handedness() {
        let mut bytes = encode(&Frame {
            timestamp_us: 0,
            hands: vec![Hand {
                handedness: Handedness::Left,
                score: 1.0,
                landmarks: [[0.0; 3]; LANDMARKS],
            }],
        });
        bytes[HEADER_LEN] = 7;
        assert_eq!(Frame::decode(&bytes), Err(DecodeError::Handedness(7)));
    }
}
