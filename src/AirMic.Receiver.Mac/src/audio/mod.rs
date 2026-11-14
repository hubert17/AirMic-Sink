pub mod coreaudio;

use anyhow::Result;
use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const TARGET_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_FRAME_SAMPLES_PER_CH: usize = 960; // 20ms @ 48kHz

/// Exact hysteresis thresholds from `WasapiExclusiveSink.cs` & `WebRtcReceiver.cs`:
/// - Voice mode: trim down to 120 ms cushion whenever buffered audio exceeds 200 ms.
/// - Music mode: trim down to 135 ms cushion whenever buffered audio exceeds 300 ms.
pub const VOICE_MAX_BACKLOG_SECS: f64 = 0.200;
pub const VOICE_TARGET_CUSHION_SECS: f64 = 0.120;
pub const MUSIC_MAX_BACKLOG_SECS: f64 = 0.300;
pub const MUSIC_TARGET_CUSHION_SECS: f64 = 0.135;

/// Ring buffer capacity: 500 ms stereo (matching `BufferDuration = 500ms` in `WasapiExclusiveSink.cs`).
pub const RING_BUFFER_DURATION_MS: usize = 500;

/// Mirror of C# `IAudioBufferSink` adapted idiomatically for Rust + zero-lock CoreAudio HAL.
pub trait AudioSink: Send {
    fn initialize(
        &mut self,
        sample_rate: u32,
        channels: u16,
        target_device_uid: Option<&str>,
    ) -> Result<()>;

    /// Pushes interleaved 16-bit PCM frames (`source_channels` = 1 or 2) at `source_sample_rate`.
    /// Automatically upmixes mono to stereo and resamples if the virtual sink runs at != `source_sample_rate`.
    fn write_pcm_i16(&mut self, pcm_data: &[i16], source_channels: u16, source_sample_rate: u32);

    /// Dynamically updates playout jitter buffer discard thresholds (Voice vs Music mode).
    fn configure_buffer_thresholds(&self, max_backlog_secs: f64, target_cushion_secs: f64);

    fn start(&mut self) -> Result<()>;
    fn stop(&mut self);
    fn device_name(&self) -> &str;
    fn device_sample_rate(&self) -> u32;
}

/// Counterpart capture trait for reading PCM from a virtual audio bus (Meeting App Speaker -> Mobile).
pub trait AudioSource: Send {
    fn initialize(&mut self, target_device_uid: &str) -> Result<()>;
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self);
    /// Reads interleaved stereo `f32` samples from the wait-free `rtrb` capture ring buffer.
    fn read_stereo_f32(&mut self, out: &mut [f32]) -> usize;
    fn device_sample_rate(&self) -> u32;
    fn device_name(&self) -> &str;
}

/// Lock-free shared runtime controls & live telemetry updated via relaxed atomics.
/// Safe to read/write from CoreAudio Mach real-time IOProcs without locks, allocations, or syscalls.
pub struct AudioRuntimeControls {
    mute_incoming: AtomicBool,
    output_gain_bits: AtomicU32,
    incoming_vu_bits: AtomicU32,
    outgoing_vu_bits: AtomicU32,
    buffer_fill_ms_bits: AtomicU32,
    device_sample_rate: AtomicU32,
    max_backlog_frames: AtomicUsize,
    target_cushion_frames: AtomicUsize,
}

impl Default for AudioRuntimeControls {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRuntimeControls {
    pub fn new() -> Self {
        let rate = TARGET_SAMPLE_RATE;
        Self {
            mute_incoming: AtomicBool::new(false),
            output_gain_bits: AtomicU32::new(1.0_f32.to_bits()),
            incoming_vu_bits: AtomicU32::new(0.0_f32.to_bits()),
            outgoing_vu_bits: AtomicU32::new(0.0_f32.to_bits()),
            buffer_fill_ms_bits: AtomicU32::new(0.0_f32.to_bits()),
            device_sample_rate: AtomicU32::new(rate),
            max_backlog_frames: AtomicUsize::new((rate as f64 * VOICE_MAX_BACKLOG_SECS) as usize),
            target_cushion_frames: AtomicUsize::new(
                (rate as f64 * VOICE_TARGET_CUSHION_SECS) as usize,
            ),
        }
    }

    #[inline]
    pub fn set_mute_incoming(&self, mute: bool) {
        self.mute_incoming.store(mute, Ordering::Relaxed);
    }

    #[inline]
    pub fn is_mute_incoming(&self) -> bool {
        self.mute_incoming.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_output_gain(&self, gain: f32) {
        let clamped = gain.clamp(0.0, 1.5);
        self.output_gain_bits
            .store(clamped.to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn output_gain(&self) -> f32 {
        f32::from_bits(self.output_gain_bits.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_device_sample_rate(&self, sample_rate: u32) {
        self.device_sample_rate
            .store(sample_rate.max(8_000), Ordering::Relaxed);
    }

    #[inline]
    pub fn configure_thresholds_for_rate(
        &self,
        sample_rate: u32,
        max_backlog_secs: f64,
        target_cushion_secs: f64,
    ) {
        let rate = sample_rate.max(8_000) as f64;
        let max_frames = (rate * max_backlog_secs).round() as usize;
        let cushion_frames = (rate * target_cushion_secs).round() as usize;
        self.max_backlog_frames.store(max_frames, Ordering::Relaxed);
        self.target_cushion_frames
            .store(cushion_frames.min(max_frames), Ordering::Relaxed);
    }

    #[inline]
    pub fn configure_thresholds(&self, max_backlog_secs: f64, target_cushion_secs: f64) {
        let rate = self.device_sample_rate.load(Ordering::Relaxed);
        self.configure_thresholds_for_rate(rate, max_backlog_secs, target_cushion_secs);
    }

    #[inline]
    pub fn thresholds_frames(&self) -> (usize, usize) {
        (
            self.max_backlog_frames.load(Ordering::Relaxed),
            self.target_cushion_frames.load(Ordering::Relaxed),
        )
    }

    #[inline]
    pub fn store_incoming_vu(&self, rms: f32) {
        let prev = self.incoming_vu();
        // Fast attack, smooth release for clean UI metering
        let smoothed = if rms >= prev {
            rms.min(1.0)
        } else {
            (prev * 0.85 + rms * 0.15).max(0.0)
        };
        self.incoming_vu_bits
            .store(smoothed.to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn incoming_vu(&self) -> f32 {
        f32::from_bits(self.incoming_vu_bits.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn store_outgoing_vu(&self, rms: f32) {
        let prev = self.outgoing_vu();
        let smoothed = if rms >= prev {
            rms.min(1.0)
        } else {
            (prev * 0.85 + rms * 0.15).max(0.0)
        };
        self.outgoing_vu_bits
            .store(smoothed.to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn outgoing_vu(&self) -> f32 {
        f32::from_bits(self.outgoing_vu_bits.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn store_buffer_fill_ms(&self, ms: f32) {
        self.buffer_fill_ms_bits
            .store(ms.max(0.0).to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn buffer_fill_ms(&self) -> f32 {
        f32::from_bits(self.buffer_fill_ms_bits.load(Ordering::Relaxed))
    }

    pub fn reset_meters(&self) {
        self.incoming_vu_bits
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
        self.outgoing_vu_bits
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
        self.buffer_fill_ms_bits
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
    }
}

/// Pure calculation matching `WasapiExclusiveSink.Write` backlog trimming:
/// If `buffered_frames > max_backlog_frames`, returns exact frame count to discard
/// so remaining frames equal `target_cushion_frames`.
#[inline]
pub fn compute_hysteresis_discard_frames(
    buffered_frames: usize,
    max_backlog_frames: usize,
    target_cushion_frames: usize,
) -> usize {
    if buffered_frames > max_backlog_frames && buffered_frames > target_cushion_frames {
        buffered_frames - target_cushion_frames
    } else {
        0
    }
}

/// Wait-free O(1) hysteresis trim on an `rtrb::Consumer<f32>` holding interleaved stereo frames.
/// Zero allocations, zero copies, zero locks: advances the atomic read index directly.
#[inline]
pub fn trim_stereo_ring_buffer_hysteresis(
    consumer: &mut Consumer<f32>,
    max_backlog_frames: usize,
    target_cushion_frames: usize,
) -> usize {
    let available_frames = consumer.slots() / 2;
    let discard_frames = compute_hysteresis_discard_frames(
        available_frames,
        max_backlog_frames,
        target_cushion_frames,
    );
    if discard_frames > 0 {
        let discard_samples = discard_frames * 2;
        if let Ok(chunk) = consumer.read_chunk(discard_samples) {
            chunk.commit_all();
            return discard_frames;
        }
    }
    0
}

/// Upmixes mono `i16` PCM samples into interleaved stereo `i16` samples (`[L, R, L, R, ...]`).
pub fn upmix_mono_to_stereo_i16(mono: &[i16], out_stereo: &mut Vec<i16>) {
    out_stereo.clear();
    out_stereo.reserve(mono.len() * 2);
    for &s in mono {
        out_stereo.push(s);
        out_stereo.push(s);
    }
}

/// Converts interleaved `i16` PCM (`1` or `2` channels) at `src_rate` into interleaved stereo `f32`
/// (`[-1.0, 1.0]`) at `dst_rate` using linear interpolation when sample rates differ.
pub fn convert_pcm_i16_to_stereo_f32(
    pcm: &[i16],
    source_channels: u16,
    src_rate: u32,
    dst_rate: u32,
    scratch_stereo: &mut Vec<f32>,
    out_resampled: &mut Vec<f32>,
) {
    scratch_stereo.clear();
    if source_channels == 1 {
        scratch_stereo.reserve(pcm.len() * 2);
        for &sample in pcm {
            let f = (sample as f32) / 32768.0;
            scratch_stereo.push(f);
            scratch_stereo.push(f);
        }
    } else {
        let ch = (source_channels as usize).max(2);
        let frames = pcm.len() / ch;
        scratch_stereo.reserve(frames * 2);
        for frame_idx in 0..frames {
            let l = (pcm[frame_idx * ch] as f32) / 32768.0;
            let r = (pcm[frame_idx * ch + 1] as f32) / 32768.0;
            scratch_stereo.push(l);
            scratch_stereo.push(r);
        }
    }

    out_resampled.clear();
    if src_rate == dst_rate || scratch_stereo.is_empty() {
        out_resampled.extend_from_slice(scratch_stereo);
        return;
    }

    let src_frames = scratch_stereo.len() / 2;
    let ratio = (dst_rate as f64) / (src_rate as f64);
    let dst_frames = ((src_frames as f64) * ratio) as usize;
    if dst_frames == 0 {
        return;
    }

    out_resampled.reserve(dst_frames * 2);
    for i in 0..dst_frames {
        let src_pos = (i as f64) / ratio;
        let idx0 = (src_pos.floor() as usize).min(src_frames - 1);
        let idx1 = (idx0 + 1).min(src_frames - 1);
        let frac = (src_pos - (idx0 as f64)) as f32;

        let l0 = scratch_stereo[idx0 * 2];
        let r0 = scratch_stereo[idx0 * 2 + 1];
        let l1 = scratch_stereo[idx1 * 2];
        let r1 = scratch_stereo[idx1 * 2 + 1];

        out_resampled.push(l0 * (1.0 - frac) + l1 * frac);
        out_resampled.push(r0 * (1.0 - frac) + r1 * frac);
    }
}

/// Converts captured stereo `f32` samples at `src_rate` into `target_channels` (`1` or `2`)
/// `i16` PCM samples at `dst_rate` (`48000 Hz`), matching `ProcessCaptureQueueLoop` in `WebRtcReceiver.cs`.
pub fn convert_captured_stereo_f32_to_i16(
    stereo_f32: &[f32],
    src_rate: u32,
    dst_rate: u32,
    target_channels: u16,
    out_i16: &mut Vec<i16>,
) {
    out_i16.clear();
    let src_frames = stereo_f32.len() / 2;
    if src_frames == 0 {
        return;
    }

    let target_ch = (target_channels as usize).clamp(1, 2);

    if src_rate == dst_rate {
        out_i16.reserve(src_frames * target_ch);
        for i in 0..src_frames {
            let l = stereo_f32[i * 2];
            let r = stereo_f32[i * 2 + 1];
            if target_ch == 1 {
                let mono = ((l + r) * 0.5).clamp(-1.0, 1.0);
                out_i16.push((mono * i16::MAX as f32) as i16);
            } else {
                out_i16.push((l.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                out_i16.push((r.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            }
        }
        return;
    }

    let ratio = (dst_rate as f64) / (src_rate as f64);
    let dst_frames = ((src_frames as f64) * ratio) as usize;
    if dst_frames == 0 {
        return;
    }

    out_i16.reserve(dst_frames * target_ch);
    for i in 0..dst_frames {
        let src_pos = (i as f64) / ratio;
        let f0 = (src_pos.floor() as usize).min(src_frames - 1);
        let f1 = (f0 + 1).min(src_frames - 1);
        let w = (src_pos - (f0 as f64)) as f32;

        let l = stereo_f32[f0 * 2] * (1.0 - w) + stereo_f32[f1 * 2] * w;
        let r = stereo_f32[f0 * 2 + 1] * (1.0 - w) + stereo_f32[f1 * 2 + 1] * w;

        if target_ch == 1 {
            let mono = ((l + r) * 0.5).clamp(-1.0, 1.0);
            out_i16.push((mono * i16::MAX as f32) as i16);
        } else {
            out_i16.push((l.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            out_i16.push((r.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
    }
}

/// Computes normalized `[0.0, 1.0]` RMS amplitude of 16-bit PCM samples, matching `WebRtcReceiver.cs`.
#[inline]
pub fn compute_rms_i16(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0_f64;
    for &s in samples {
        let v = (s as f64) / 32768.0;
        sum += v * v;
    }
    (sum / (samples.len() as f64)).sqrt()
}

/// 1-click 440 Hz virtual sink test generator (desktop UI replacement for `--mode test-sink`).
pub struct SineWaveTestRunner {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    device_name: String,
}

impl SineWaveTestRunner {
    pub fn start(
        target_device_uid: &str,
        controls: Arc<AudioRuntimeControls>,
    ) -> Result<Self> {
        let mut sink = coreaudio::CoreAudioVirtualSink::new(controls.clone());
        sink.initialize(TARGET_SAMPLE_RATE, 2, Some(target_device_uid))?;
        sink.configure_buffer_thresholds(VOICE_MAX_BACKLOG_SECS, VOICE_TARGET_CUSHION_SECS);
        sink.start()?;

        let device_name = sink.device_name().to_string();
        let running = Arc::new(AtomicBool::new(true));
        let running_flag = running.clone();

        let worker = thread::Builder::new()
            .name("airmic-440hz-test".to_string())
            .spawn(move || {
                tracing::info!(
                    "[+] 440 Hz Sink Test active -> feeding continuous tone to '{}'",
                    sink.device_name()
                );

                let sample_rate = TARGET_SAMPLE_RATE;
                let samples_per_frame = (sample_rate / 100) as usize; // 10ms = 480 samples
                let frequency = 440.0_f64;
                let amplitude = 0.15_f64; // Exact comfort level from Program.cs
                let phase_step = std::f64::consts::TAU * frequency / (sample_rate as f64);
                let mut phase = 0.0_f64;
                let mut frame_buf = vec![0_i16; samples_per_frame];

                let start_instant = Instant::now();
                let mut frames_emitted: u64 = 0;

                while running_flag.load(Ordering::Relaxed) {
                    for sample_out in &mut frame_buf {
                        let s = amplitude * phase.sin();
                        phase += phase_step;
                        if phase >= std::f64::consts::TAU {
                            phase -= std::f64::consts::TAU;
                        }
                        *sample_out = (s * (i16::MAX as f64)) as i16;
                    }

                    sink.write_pcm_i16(&frame_buf, 1, sample_rate);
                    frames_emitted += 1;

                    let target_elapsed = Duration::from_millis(frames_emitted * 10);
                    let now_elapsed = start_instant.elapsed();
                    if target_elapsed > now_elapsed {
                        thread::sleep(target_elapsed - now_elapsed);
                    }
                }

                sink.stop();
                controls.reset_meters();
                tracing::info!("[*] 440 Hz Sink Test stopped and virtual sink released.");
            })?;

        Ok(Self {
            running,
            worker: Some(worker),
            device_name,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SineWaveTestRunner {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    #[test]
    fn voice_hysteresis_trims_above_200ms_down_to_120ms() {
        let rate = 48_000_usize;
        let max_backlog = (rate as f64 * VOICE_MAX_BACKLOG_SECS) as usize; // 9,600 frames
        let cushion = (rate as f64 * VOICE_TARGET_CUSHION_SECS) as usize; // 5,760 frames

        // At exactly 200ms (9,600 frames), no trim should happen
        assert_eq!(
            compute_hysteresis_discard_frames(9_600, max_backlog, cushion),
            0
        );

        // At 220ms (10,560 frames), trim down to exactly 120ms (5,760 frames)
        let (mut prod, mut cons) = RingBuffer::<f32>::new(rate * 2);
        for i in 0..(10_560 * 2) {
            prod.push(i as f32).unwrap();
        }

        let discarded = trim_stereo_ring_buffer_hysteresis(&mut cons, max_backlog, cushion);
        assert_eq!(discarded, 10_560 - 5_760);
        assert_eq!(cons.slots(), 5_760 * 2);
        // Verify stereo alignment (first remaining sample corresponds to frame 4800 L)
        let next_sample = cons.pop().unwrap();
        assert_eq!(next_sample, (4_800 * 2) as f32);
    }

    #[test]
    fn music_hysteresis_trims_above_300ms_down_to_135ms() {
        let rate = 48_000_usize;
        let max_backlog = (rate as f64 * MUSIC_MAX_BACKLOG_SECS) as usize; // 14,400 frames
        let cushion = (rate as f64 * MUSIC_TARGET_CUSHION_SECS) as usize; // 6,480 frames

        assert_eq!(
            compute_hysteresis_discard_frames(14_400, max_backlog, cushion),
            0
        );

        let (mut prod, mut cons) = RingBuffer::<f32>::new(rate * 2);
        for _ in 0..(16_000 * 2) {
            prod.push(0.25).unwrap();
        }

        let discarded = trim_stereo_ring_buffer_hysteresis(&mut cons, max_backlog, cushion);
        assert_eq!(discarded, 16_000 - 6_480);
        assert_eq!(cons.slots(), 6_480 * 2);
    }

    #[test]
    fn mono_upmix_and_capture_resample_roundtrip() {
        let mono_in = vec![1000_i16, -1000_i16, 16384_i16, -16384_i16];
        let mut stereo = Vec::new();
        upmix_mono_to_stereo_i16(&mono_in, &mut stereo);
        assert_eq!(
            stereo,
            vec![1000, 1000, -1000, -1000, 16384, 16384, -16384, -16384]
        );

        // Resample 44.1kHz captured stereo to 48kHz mono
        let src_frames = 441;
        let src_stereo = vec![0.5_f32; src_frames * 2];
        let mut out_mono = Vec::new();
        convert_captured_stereo_f32_to_i16(&src_stereo, 44_100, 48_000, 1, &mut out_mono);
        assert_eq!(out_mono.len(), 480);
        assert!((out_mono[0] - (i16::MAX / 2)).abs() <= 2);
    }

    #[test]
    fn live_440hz_sine_tone_reaches_coreaudio_virtual_sink_cleanly() {
        let virtual_devs = coreaudio::enumerate_virtual_devices();
        let Some(out_dev) = virtual_devs.iter().find(|d| d.has_output()) else {
            return;
        };

        // Ensure physical devices never leaked into virtual enumeration
        for d in &virtual_devs {
            let lower = d.name.to_lowercase();
            assert!(!lower.contains("macbook") && !lower.contains("speakers"));
        }

        let controls = Arc::new(AudioRuntimeControls::new());
        let mut runner = SineWaveTestRunner::start(&out_dev.uid, controls.clone())
            .expect("Failed starting 440 Hz SineWaveTestRunner on virtual sink");

        thread::sleep(Duration::from_millis(300));

        // Amplitude is 0.15 -> theoretical sine RMS is 0.15 / sqrt(2) ~= 0.106
        let measured_vu = controls.incoming_vu();
        assert!(
            measured_vu > 0.08 && measured_vu < 0.15,
            "Expected 440 Hz sink RMS around 0.106, got {}",
            measured_vu
        );

        runner.stop();
        assert_eq!(controls.incoming_vu(), 0.0);
    }
}
