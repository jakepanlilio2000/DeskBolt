use std::collections::VecDeque;

use crate::audio_codec::AUDIO_SAMPLES;

const MAX_DELAY: usize = 7_200;
const TAPS: usize = 96;
pub const REFERENCE_CAPACITY: usize = MAX_DELAY + TAPS + AUDIO_SAMPLES;

pub struct EchoCanceller {
    taps: [f32; TAPS],
    lag: usize,
    frame: usize,
}

impl Default for EchoCanceller {
    fn default() -> Self {
        Self {
            taps: [0.0; TAPS],
            lag: 0,
            frame: 0,
        }
    }
}

impl EchoCanceller {
    pub fn process(&mut self, mic: &mut [i16], rendered: &VecDeque<i16>) {
        if rendered.len() < REFERENCE_CAPACITY || mic.len() != AUDIO_SAMPLES {
            return;
        }
        let end = rendered.len() - AUDIO_SAMPLES;
        if self.frame.is_multiple_of(10) {
            // Find bulk delay before adapting the short linear filter around it.
            let mut best = (0.35, self.lag);
            let correlate = |lag: usize| {
                let mut cross = 0.0f32;
                let mut capture = 1.0;
                let mut reference = 1.0;
                for i in (0..AUDIO_SAMPLES).step_by(12) {
                    let x = rendered[end + i - TAPS - lag] as f32;
                    let y = mic[i] as f32;
                    cross += x * y;
                    capture += y * y;
                    reference += x * x;
                }
                cross.abs() / (capture * reference).sqrt()
            };
            for lag in (0..=MAX_DELAY).step_by(24) {
                let score = correlate(lag);
                if score > best.0 {
                    best = (score, lag);
                }
            }
            let coarse = best.1;
            for lag in coarse.saturating_sub(24)..=(coarse + 24).min(MAX_DELAY) {
                let score = correlate(lag);
                if score > best.0 {
                    best = (score, lag);
                }
            }
            if best.1.abs_diff(self.lag) >= TAPS / 2 {
                self.taps.fill(0.0);
                self.lag = best.1;
            }
        }
        self.frame = self.frame.wrapping_add(1);
        for (index, sample) in mic.iter_mut().enumerate() {
            let history = end + index - TAPS - self.lag;
            let mut estimate = 0.0;
            let mut energy = 1_000_000.0; // Avoid adapting against near silence.
            for (tap, weight) in self.taps.iter().enumerate() {
                let reference = rendered[history + TAPS - 1 - tap] as f32;
                estimate += weight * reference;
                energy += reference * reference;
            }
            let error = *sample as f32 - estimate;
            let step = 0.12 * error / energy;
            for (tap, weight) in self.taps.iter_mut().enumerate() {
                let reference = rendered[history + TAPS - 1 - tap] as f32;
                *weight = (*weight + step * reference).clamp(-2.0, 2.0);
            }
            *sample = error.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attenuates_a_delayed_playback_reference() {
        let mut seed = 17u32;
        let reference: VecDeque<i16> = (0..REFERENCE_CAPACITY)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 16) as i16 / 2
            })
            .collect();
        let end = reference.len() - AUDIO_SAMPLES;
        let original: Vec<i16> = (0..AUDIO_SAMPLES)
            .map(|i| reference[end + i - TAPS - 480] / 2)
            .collect();
        let mut aec = EchoCanceller::default();
        let mut mic = original.clone();
        for _ in 0..12 {
            mic.copy_from_slice(&original);
            aec.process(&mut mic, &reference);
        }
        let energy = |samples: &[i16]| {
            samples
                .iter()
                .map(|&sample| i64::from(sample).pow(2))
                .sum::<i64>()
        };
        assert!(energy(&mic) < energy(&original) / 8);
    }
}
