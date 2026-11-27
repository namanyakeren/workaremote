use crate::platform::Frame;

pub struct Encoder {
    #[cfg(feature = "h264")]
    inner: Option<openh264::encoder::Encoder>,
    #[cfg(feature = "h264")]
    rgb: Vec<u8>,
    #[cfg(feature = "h264")]
    dims: (usize, usize),
    #[cfg(feature = "h264")]
    first: bool,
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "h264")]
            inner: None,
            #[cfg(feature = "h264")]
            rgb: Vec::new(),
            #[cfg(feature = "h264")]
            dims: (0, 0),
            #[cfg(feature = "h264")]
            first: true,
        }
    }

    pub fn reset(&mut self) {
        #[cfg(feature = "h264")]
        {
            self.inner = None;
            self.rgb.clear();
            self.dims = (0, 0);
            self.first = true;
        }
    }

    pub fn encode(&mut self, frame: &Frame) -> Option<Vec<u8>> {
        #[cfg(feature = "h264")]
        {
            let w = frame.w as usize & !1;
            let h = frame.h as usize & !1;
            if w < 2 || h < 2 {
                return None;
            }
            self.rgb.resize(w * h * 3, 0);
            for y in 0..h {
                for x in 0..w {
                    let si = (y * frame.w as usize + x) * 4;
                    let di = (y * w + x) * 3;
                    self.rgb[di..di + 3].copy_from_slice(&[
                        frame.bgra[si + 2],
                        frame.bgra[si + 1],
                        frame.bgra[si],
                    ]);
                }
            }
            if self.dims != (w, h) {
                let api = openh264::OpenH264API::from_source();
                let config = openh264::encoder::EncoderConfig::new(w as u32, h as u32)
                    .set_bitrate_bps(2_000_000)
                    .max_frame_rate(25.0);
                self.inner = openh264::encoder::Encoder::with_config(api, config).ok();
                self.dims = (w, h);
                self.first = true;
            }
            let yuv = openh264::formats::YUVBuffer::with_rgb(w, h, &self.rgb);
            let encoded = self.inner.as_mut()?.encode(&yuv).ok()?.to_vec();
            let key = self.first;
            self.first = false;
            return Some(proto::encode_video(key, 0, &encoded));
        }
        #[cfg(not(feature = "h264"))]
        {
            let _ = frame;
            None
        }
    }
}

pub fn available() -> bool {
    cfg!(feature = "h264")
}
