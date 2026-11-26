pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_SAMPLES: usize = 960;
pub const MAX_AUDIO_PACKET: usize = 4 + 1275;

pub struct AudioEncoder {
    encoder: opus::Encoder,
    sequence: u32,
}

impl AudioEncoder {
    pub fn new(channels: opus::Channels, kbps: u32) -> Result<Self, opus::Error> {
        let application = match channels {
            opus::Channels::Mono => opus::Application::Voip,
            opus::Channels::Stereo => opus::Application::Audio,
        };
        let mut encoder = opus::Encoder::new(AUDIO_RATE, channels, application)?;
        encoder.set_bitrate(opus::Bitrate::Bits((kbps * 1000) as i32))?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(10)?;
        Ok(Self {
            encoder,
            sequence: 0,
        })
    }

    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, opus::Error> {
        let mut packet = vec![0; MAX_AUDIO_PACKET];
        packet[..4].copy_from_slice(&self.sequence.to_be_bytes());
        self.sequence = self.sequence.wrapping_add(1);
        let bytes = self.encoder.encode(pcm, &mut packet[4..])?;
        packet.truncate(4 + bytes);
        Ok(packet)
    }
}

pub struct AudioDecoder {
    decoder: opus::Decoder,
    expected: Option<u32>,
    samples: usize,
}

impl AudioDecoder {
    pub fn new(channels: opus::Channels) -> Result<Self, opus::Error> {
        Ok(Self {
            decoder: opus::Decoder::new(AUDIO_RATE, channels)?,
            expected: None,
            samples: AUDIO_SAMPLES * channels as usize,
        })
    }

    pub fn decode(&mut self, packet: &[u8]) -> Option<Vec<i16>> {
        if !valid_audio_packet(packet) {
            return None;
        }
        let seq = u32::from_be_bytes(packet[..4].try_into().ok()?);
        let body = &packet[4..];
        let missing = self
            .expected
            .map_or(0, |expected| seq.wrapping_sub(expected));
        if missing >= 1 << 31 {
            return None;
        }
        self.expected = Some(seq.wrapping_add(1));
        if missing > 3 {
            let _ = self.decoder.reset_state();
        }
        let mut decoded = Vec::with_capacity(self.samples * 4);
        for index in 0..missing.min(3) {
            if missing <= 3 && index + 1 == missing {
                decoded.extend(self.decode_frame(body, true));
            } else {
                decoded.extend(self.decode_frame(&[], false));
            }
        }
        decoded.extend(self.decode_frame(body, false));
        Some(decoded)
    }

    fn decode_frame(&mut self, packet: &[u8], fec: bool) -> Vec<i16> {
        let mut frame = vec![0; self.samples];
        if !matches!(
            self.decoder.decode(packet, &mut frame, fec),
            Ok(AUDIO_SAMPLES)
        ) {
            frame.fill(0);
            let _ = self.decoder.decode(&[], &mut frame, false);
        }
        frame
    }
}

pub fn valid_audio_packet(packet: &[u8]) -> bool {
    (5..=MAX_AUDIO_PACKET).contains(&packet.len())
        && matches!(
            opus::packet::get_nb_samples(&packet[4..], AUDIO_RATE),
            Ok(AUDIO_SAMPLES)
        )
}

pub(crate) fn bounded_packet<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> Result<Vec<u8>, D::Error> {
    struct PacketVisitor;

    impl<'de> serde::de::Visitor<'de> for PacketVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "an audio packet of 5 to {MAX_AUDIO_PACKET} bytes")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            use serde::de::Error;
            if let Some(size) = seq.size_hint()
                && !(5..=MAX_AUDIO_PACKET).contains(&size)
            {
                return Err(A::Error::invalid_length(size, &self));
            }
            let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or_default());
            while let Some(byte) = seq.next_element()? {
                if bytes.len() == MAX_AUDIO_PACKET {
                    return Err(A::Error::invalid_length(bytes.len() + 1, &self));
                }
                bytes.push(byte);
            }
            if bytes.len() < 5 {
                return Err(A::Error::invalid_length(bytes.len(), &self));
            }
            Ok(bytes)
        }
    }

    decoder.deserialize_seq(PacketVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_round_trip_and_concealment_keep_twenty_ms_timing() {
        for channels in [opus::Channels::Mono, opus::Channels::Stereo] {
            let mut encoder = AudioEncoder::new(channels, 32).unwrap();
            let mut decoder = AudioDecoder::new(channels).unwrap();
            let samples = AUDIO_SAMPLES * channels as usize;
            let input: Vec<i16> = (0..samples)
                .map(|i| ((i as f32 / 16.0).sin() * 8000.0) as i16)
                .collect();
            let first = encoder.encode(&input).unwrap();
            assert_eq!(decoder.decode(&first).unwrap().len(), samples);
            let _lost = encoder.encode(&input).unwrap();
            let next = encoder.encode(&input).unwrap();
            let rendered = decoder.decode(&next).unwrap();
            assert_eq!(rendered.len(), samples * 2);
            assert!(rendered[samples..].iter().any(|&value| value != 0));
            // A valid duration header with a truncated payload exercises decoder fallback.
            let mut damaged = encoder.encode(&input).unwrap();
            damaged.truncate(5);
            if valid_audio_packet(&damaged) {
                assert_eq!(decoder.decode(&damaged).unwrap().len(), samples);
            }
            assert!(decoder.decode(&next).is_none());
            assert_eq!(decoder.decode_frame(&[], false).len(), samples);
        }
    }
}
