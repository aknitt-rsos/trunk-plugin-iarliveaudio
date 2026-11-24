//! A safe wrapper over libopus's encoder, for mono audio, as IaR-Stream encodes it:
//! `APPLICATION_AUDIO`, at the call's own sample rate, in frames of 60 ms.

use std::os::raw::c_int;

use libopus_sys as ffi;

/// Upper bound on one encoded frame. A 60 ms mono frame at 48 kHz is far smaller.
const MAX_PACKET: usize = 4000;

/// OPUS_RESET_STATE, the encoder control that clears its history.
const RESET_STATE: c_int = 4028;

pub struct Encoder {
    raw: *mut ffi::OpusEncoder,
    rate: u32,
}

// The encoder is only ever used from one thread at a time (the plugin's main loop owns it).
unsafe impl Send for Encoder {}

impl Encoder {
    /// Opus takes these sample rates and no others.
    pub fn new(rate: u32) -> Result<Encoder, String> {
        if !matches!(rate, 8000 | 12000 | 16000 | 24000 | 48000) {
            return Err(format!("Opus can't encode audio at {rate} Hz"));
        }
        let mut error: c_int = 0;
        // SAFETY: plain FFI constructor; `error` is a valid out-pointer.
        let raw = unsafe { ffi::opus_encoder_create(rate as i32, 1, ffi::OPUS_APPLICATION_AUDIO as c_int, &mut error) };
        if raw.is_null() || error != ffi::OPUS_OK as c_int {
            return Err(format!("couldn't start the Opus encoder (error {error})"));
        }
        Ok(Encoder { raw, rate })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Encode one frame of samples. The frame must be exactly one Opus frame long (60 ms here).
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; MAX_PACKET];
        // SAFETY: `pcm` holds `pcm.len()` mono samples and `out` has MAX_PACKET bytes of room.
        let n = unsafe { ffi::opus_encode(self.raw, pcm.as_ptr(), pcm.len() as c_int, out.as_mut_ptr(), MAX_PACKET as i32) };
        if n < 0 {
            return Err(format!("Opus failed to encode a frame (error {n})"));
        }
        out.truncate(n as usize);
        Ok(out)
    }

    /// Forget the previous frames, as IaR-Stream does after each transmission.
    pub fn reset(&mut self) {
        // SAFETY: a variadic control call with no extra arguments, which is what RESET_STATE takes.
        unsafe {
            ffi::opus_encoder_ctl(self.raw, RESET_STATE);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from opus_encoder_create and is destroyed exactly once.
        unsafe { ffi::opus_encoder_destroy(self.raw) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 60 ms tone, which the encoder turns into a small packet.
    fn tone(rate: u32) -> Vec<i16> {
        let n = (rate * 60 / 1000) as usize;
        (0..n).map(|i| ((i as f64 * 0.3).sin() * 8000.0) as i16).collect()
    }

    #[test]
    fn encodes_60ms_frames_at_8khz() {
        let mut enc = Encoder::new(8000).expect("encoder");
        assert_eq!(enc.rate(), 8000);
        let packet = enc.encode(&tone(8000)).expect("frame");
        assert!(!packet.is_empty() && packet.len() < 480 * 2, "a compressed 60 ms frame: {} bytes", packet.len());
        // Encoding continues after a reset, from a clean state.
        enc.reset();
        assert!(!enc.encode(&tone(8000)).expect("frame after reset").is_empty());
    }

    #[test]
    fn refuses_rates_opus_cannot_encode() {
        assert!(Encoder::new(44100).is_err());
        assert!(Encoder::new(0).is_err());
    }
}
