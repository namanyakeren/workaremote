//! Dirty-tile detection and JPEG encoding.
//!
//! The screen is cut into TILE-sized squares. Only squares whose pixels changed
//! since they were last *sent* are encoded. That keeps a mostly-static desktop
//! at a few KB/s.

use crate::platform::Frame;
use proto::{encode_tiles, TILE};

const NEVER: u64 = u64::MAX;
/// Per-frame byte ceiling. Bigger frames are finished on the next tick, which
/// keeps latency bounded on slow links instead of queueing megabytes.
const BUDGET: usize = 480_000;

pub struct Tiler {
    w: u16,
    h: u16,
    cols: usize,
    prev: Vec<u64>,
}

impl Tiler {
    pub fn new() -> Self {
        Tiler {
            w: 0,
            h: 0,
            cols: 0,
            prev: Vec::new(),
        }
    }

    /// Forget what the viewer has; the next frame is a full refresh.
    pub fn reset(&mut self) {
        self.w = 0;
        self.h = 0;
        self.prev.clear();
    }

    pub fn encode(&mut self, frame: &Frame, quality: u8) -> Vec<u8> {
        if frame.w != self.w || frame.h != self.h {
            self.w = frame.w;
            self.h = frame.h;
            self.cols = (frame.w as usize + TILE as usize - 1) / TILE as usize;
            let rows = (frame.h as usize + TILE as usize - 1) / TILE as usize;
            self.prev = vec![NEVER; self.cols * rows];
        }

        let stride = frame.w as usize * 4;
        let mut tiles: Vec<(u16, u16, u16, u16, Vec<u8>)> = Vec::new();
        let mut scratch: Vec<u8> = Vec::new();
        let mut bytes = 0usize;

        'outer: for ty in 0..self.prev.len() / self.cols {
            for tx in 0..self.cols {
                let x = tx * TILE as usize;
                let y = ty * TILE as usize;
                let tw = (frame.w as usize - x).min(TILE as usize);
                let th = (frame.h as usize - y).min(TILE as usize);

                let mut hash = 0u64;
                for row in 0..th {
                    let start = (y + row) * stride + x * 4;
                    let row_hash = hash_bytes(&frame.bgra[start..start + tw * 4]);
                    hash = (hash ^ row_hash)
                        .wrapping_mul(0x100_0000_01b3)
                        .rotate_left(7);
                }

                let idx = ty * self.cols + tx;
                if self.prev[idx] == hash {
                    continue;
                }
                if bytes > BUDGET && !tiles.is_empty() {
                    break 'outer;
                }

                scratch.clear();
                for row in 0..th {
                    let start = (y + row) * stride + x * 4;
                    scratch.extend_from_slice(&frame.bgra[start..start + tw * 4]);
                }
                let mut jpeg = Vec::new();
                let encoded = jpeg_encoder::Encoder::new(&mut jpeg, quality).encode(
                    &scratch,
                    tw as u16,
                    th as u16,
                    jpeg_encoder::ColorType::Rgba,
                );
                if encoded.is_err() {
                    continue;
                }
                bytes += jpeg.len();
                // Only mark as delivered once it is really in the batch.
                self.prev[idx] = hash;
                tiles.push((x as u16, y as u16, tw as u16, th as u16, jpeg));
            }
        }

        if tiles.is_empty() {
            return Vec::new();
        }
        let refs: Vec<(u16, u16, u16, u16, &[u8])> = tiles
            .iter()
            .map(|t| (t.0, t.1, t.2, t.3, t.4.as_slice()))
            .collect();
        encode_tiles(&refs)
    }
}

fn hash_bytes(data: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let v = u64::from_le_bytes(c.try_into().unwrap());
        h = (h ^ v).wrapping_mul(0x100_0000_01b3).rotate_left(5);
    }
    for &b in chunks.remainder() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(w: u16, h: u16) -> Frame {
        let mut bgra = vec![0u8; w as usize * h as usize * 4];
        for (i, p) in bgra.chunks_exact_mut(4).enumerate() {
            p[0] = (i % 251) as u8;
            p[3] = 255;
        }
        Frame { w, h, bgra }
    }

    fn count(batch: &[u8]) -> u16 {
        u16::from_le_bytes([batch[1], batch[2]])
    }

    #[test]
    fn sends_only_changed_tiles() {
        let mut tiler = Tiler::new();
        let mut frame = test_frame(512, 512);

        let first = tiler.encode(&frame, 70);
        assert_eq!(first[0], proto::MSG_TILES);
        assert_eq!(count(&first), 4, "512x512 must be four 256px tiles");

        assert!(
            tiler.encode(&frame, 70).is_empty(),
            "unchanged screen sends nothing"
        );

        frame.bgra[0] ^= 0xff;
        let second = tiler.encode(&frame, 70);
        assert_eq!(count(&second), 1, "one changed pixel touches one tile");
    }

    #[test]
    fn reset_forces_full_refresh() {
        let mut tiler = Tiler::new();
        let frame = test_frame(256, 256);
        assert_eq!(count(&tiler.encode(&frame, 70)), 1);
        tiler.reset();
        assert_eq!(count(&tiler.encode(&frame, 70)), 1);
    }

    /// Manual check against the real X display: cargo test -p worka-agent -- --ignored
    #[test]
    #[ignore = "butuh display X11"]
    fn captures_real_screen() {
        let mut platform = crate::platform::Platform::new().unwrap();
        let monitors = platform.monitors();
        let frame = platform.capture(monitors[0].i).unwrap();
        assert!(frame.w > 0 && frame.h > 0);
        assert_eq!(frame.bgra.len(), frame.w as usize * frame.h as usize * 4);
        let mut tiler = Tiler::new();
        let batch = tiler.encode(&frame, 80);
        assert!(!batch.is_empty());
        std::fs::write("/tmp/worka-frame.bin", &batch).unwrap();
    }
}
