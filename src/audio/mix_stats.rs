//! What the guest mixed, for the `ounce.run.v1` report: how many stereo frames, and an FNV-1a 64 digest of the
//! little-endian bytes of every mixed 16-bit sample, in order. Counted where audio is produced, so it does not
//! depend on a host sound device or on the host ring buffer's capacity.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Debug, Clone, Copy)]
pub struct MixStats {
    samples: u64,
    digest: u64,
}

impl Default for MixStats {
    fn default() -> Self {
        Self { samples: 0, digest: FNV_OFFSET }
    }
}

fn fnv(mut state: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        state ^= u64::from(*b);
        state = state.wrapping_mul(FNV_PRIME);
    }
    state
}

impl MixStats {
    /// Records `samples` as mixed, in order.
    pub fn feed(&mut self, samples: &[i16]) {
        for s in samples {
            self.digest = fnv(self.digest, &s.to_le_bytes());
        }
        self.samples += samples.len() as u64;
    }

    /// Stereo frames mixed (a left and a right sample each).
    pub fn frames(&self) -> u64 {
        self.samples / 2
    }
}

/// `(audio_frames_mixed, audio_hash)` for the report from the renderer (`audren`) and output (`audout`) streams.
///
/// One stream used: its digest. Both used: FNV-1a 64 over the two digests as 8 little-endian bytes each, audren
/// first. Nothing mixed: `0` and `None`.
pub fn report_fields(audren: &MixStats, audout: &MixStats) -> (u64, Option<String>) {
    let frames = audren.frames() + audout.frames();
    let digest = match (audren.samples > 0, audout.samples > 0) {
        (false, false) => return (0, None),
        (true, false) => audren.digest,
        (false, true) => audout.digest,
        (true, true) => {
            let both = fnv(FNV_OFFSET, &audren.digest.to_le_bytes());
            fnv(both, &audout.digest.to_le_bytes())
        }
    };
    (frames, Some(format!("fnv1a64:{digest:016x}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_mixed_reports_zero_and_none() {
        assert_eq!(report_fields(&MixStats::default(), &MixStats::default()), (0, None));
    }

    #[test]
    fn digest_is_fnv1a_64_of_little_endian_samples() {
        // FNV-1a 64 of the bytes 01 00 02 00 (samples 1 and 2), computed independently.
        let mut s = MixStats::default();
        s.feed(&[1, 2]);
        assert_eq!(s.frames(), 1);
        let expected = [1u8, 0, 2, 0].iter().fold(FNV_OFFSET, |h, b| (h ^ u64::from(*b)).wrapping_mul(FNV_PRIME));
        assert_eq!(report_fields(&s, &MixStats::default()), (1, Some(format!("fnv1a64:{expected:016x}"))));
        // Feeding in two calls is the same stream.
        let mut t = MixStats::default();
        t.feed(&[1]);
        t.feed(&[2]);
        assert_eq!(report_fields(&t, &MixStats::default()), report_fields(&s, &MixStats::default()));
    }

    #[test]
    fn both_streams_combine_audren_first() {
        let (mut ren, mut out) = (MixStats::default(), MixStats::default());
        ren.feed(&[1, 2]);
        out.feed(&[3, 4, 5, 6]);
        let (frames, hash) = report_fields(&ren, &out);
        assert_eq!(frames, 3);
        let mut bytes = ren.digest.to_le_bytes().to_vec();
        bytes.extend_from_slice(&out.digest.to_le_bytes());
        let expected = fnv(FNV_OFFSET, &bytes);
        assert_eq!(hash, Some(format!("fnv1a64:{expected:016x}")));
        assert_ne!(report_fields(&out, &ren).1, hash, "order matters");
    }
}
