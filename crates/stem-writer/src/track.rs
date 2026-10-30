//! One stem's file: resampling to 16 kHz and placing each buffer at its host time.

use std::collections::VecDeque;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use stem_timing::{BufferStamp, Stem, StemLosses};

use crate::manifest::{AheadRecord, DeviceRecord, GapCause, GapRecord, RateRecord, StemRecord};
use crate::wav::StemWav;
use crate::{OUTPUT_RATE_HZ, SEGMENT_SAMPLE_LIMIT, StemWriterError};

const NS_PER_S: u128 = 1_000_000_000;

/// Drift past this either way, with no stall step to explain it, is no clock skew. Behind host
/// time it is filled with silence: the first buffer after a device switch, or a stall the step
/// test missed. Ahead of host time it opens a run ahead. Cap and OBS smooth timestamps within the
/// same 70 ms.
const DRIFT_LIMIT_NS: i128 = 70_000_000;

/// A host-time step between two stamps beyond what their sample counts explain is a stall when it
/// passes this floor. Core Audio stamps come from the device clock and jitter well under 1 ms.
const STALL_FLOOR_NS: i128 = 4_000_000;

/// Noisy stamps raise the stall limit to this many times their mean step size, so jitter never
/// reads as a stall.
const STALL_JITTER_FACTOR: f64 = 4.0;

/// Steps measured before the mean step size is trusted. Until then only a 70 ms step is a stall.
const STALL_WARMUP_STEPS: u32 = 16;

/// The mean step size averages about the last 64 stamps.
const STEP_SIZE_WEIGHT: f64 = 1.0 / 64.0;

/// Time constant of the low-pass on the drift before steering. Ten times the 20 s steering time
/// constant would slow the loop; 2 s averages about 80 stamps, which cuts ±10 ms stamp jitter to
/// a swing of a few tens of ppm.
const DRIFT_FILTER_S: f64 = 2.0;

/// A run ahead ends once the file is back within this of host time.
const AHEAD_CLOSED_NS: i128 = 1_000_000;

/// The most steering may stretch or squeeze the audio. Device clocks sit within about 100 ppm
/// of nominal; 1,000 ppm is an inaudible pitch change of 0.06 semitones.
const STEERING_LIMIT: f64 = 0.001;

/// Steering gains on the drift in seconds. Proportional 0.05 per second gives a 20 s time
/// constant, slow enough that 1 ms of stamp jitter moves the ratio by only 50 ppm. The integral
/// gain is critically damped for it (kp squared over 4) and holds a steady skew at zero drift.
const STEERING_PROPORTIONAL_PER_S: f64 = 0.05;
const STEERING_INTEGRAL_PER_S2: f64 =
    STEERING_PROPORTIONAL_PER_S * STEERING_PROPORTIONAL_PER_S / 4.0;

/// Input frames per resampler call: 21 ms at 48 kHz.
const RESAMPLER_CHUNK_FRAMES: usize = 1_024;

/// 128 taps reject aliasing well for speech at 16 kHz and cost about half of the 256-tap default.
const SINC_LEN_TAPS: usize = 128;

/// Samples without a stamp wait this long for one, then go into the file unplaced. Only an
/// untimed device or a lost stamp makes them wait this long.
const HOLD_LIMIT_MS: u64 = 500;

/// Silence at least this long skips the resampler: zeros at 16 kHz cost nothing to write, and
/// minutes of input zeros through a sinc filter would cost seconds of CPU.
const RESAMPLER_BYPASS_NS: u128 = NS_PER_S;

/// What a stem's file holds so far.
#[derive(Debug, Clone)]
pub struct StemProgress {
    pub stem: Stem,
    pub file: String,
    /// 16 kHz samples in the file, silence included.
    pub samples_written: u64,
    /// Input samples taken from the device so far.
    pub samples_in: u64,
    pub rate_in_hz: u32,
    pub first_host_time_ns: Option<u64>,
    pub gap_count: usize,
    pub gap_total_ms: f64,
    pub gap_largest_ms: f64,
    /// Host time minus file position at the newest stamp. Positive: the file trails host time.
    pub drift_ms: Option<f64>,
    /// Steering's stretch of the audio, in parts per million.
    pub steering_ppm: f64,
    pub ahead_count: usize,
    /// Largest input sample magnitude since the last `take_progress`.
    pub peak_recent: f32,
    /// Largest input sample magnitude in the whole stem.
    pub peak: f32,
    pub stopped_by: Option<String>,
}

/// The resampler for one stretch of constant input rate.
struct RateEpoch {
    rate_hz: u32,
    resampler: Async<f32>,
    /// File position, in output samples, where this stretch begins.
    start_sample: u64,
    /// Output samples the queued input will make, at the steering ratio it was queued under.
    out_position: f64,
    /// Output samples this stretch has put in the file.
    samples_out: u64,
    /// Output samples still to discard: the resampler's start-up delay.
    delay_left: usize,
    queued: Vec<f32>,
}

impl RateEpoch {
    fn new(rate_hz: u32, start_sample: u64, relative_ratio: f64) -> Result<Self, StemWriterError> {
        let parameters =
            SincInterpolationParameters::new(SINC_LEN_TAPS, WindowFunction::BlackmanHarris2)
                .interpolation(SincInterpolationType::Cubic);
        let mut resampler = Async::<f32>::new_sinc(
            base_ratio(rate_hz),
            1.0 + 2.0 * STEERING_LIMIT,
            &parameters,
            RESAMPLER_CHUNK_FRAMES,
            1,
            FixedAsync::Input,
        )
        .map_err(|source| StemWriterError::ResamplerSetup { rate_hz, source })?;
        resampler
            .set_resample_ratio_relative(relative_ratio, false)
            .map_err(|source| StemWriterError::Resample { source })?;
        let delay_left = resampler.output_delay();
        Ok(Self {
            rate_hz,
            resampler,
            start_sample,
            out_position: 0.0,
            samples_out: 0,
            delay_left,
            queued: Vec::with_capacity(RESAMPLER_CHUNK_FRAMES * 2),
        })
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative sample count far below 2^52"
    )]
    fn samples_out_due(&self) -> u64 {
        self.out_position.round().max(0.0) as u64
    }
}

/// A PI loop on the drift that stretches or squeezes the audio, so device clock skew never
/// builds up to a gap or a run ahead. The drift goes through a low-pass first, so stamp jitter
/// barely moves the ratio.
#[derive(Default)]
struct ClockSteering {
    integral_s2: f64,
    /// Steering's ratio minus 1.
    stretch: f64,
    last_host_ns: Option<u64>,
    /// The low-passed drift. `None` restarts the filter at the next drift.
    filtered_drift_s: Option<f64>,
}

impl ClockSteering {
    #[expect(
        clippy::cast_precision_loss,
        reason = "drift and stamp spacing stay far below 2^52 ns"
    )]
    fn update(&mut self, drift_ns: i128, host_ns: u64) -> f64 {
        let measured_s = drift_ns as f64 / 1e9;
        let step_s = self
            .last_host_ns
            .map_or(0.0, |last_ns| host_ns.saturating_sub(last_ns) as f64 / 1e9)
            .min(1.0);
        self.last_host_ns = Some(host_ns);
        let error_s = self.filtered_drift_s.map_or(measured_s, |filtered_s| {
            filtered_s + (measured_s - filtered_s) * step_s / (DRIFT_FILTER_S + step_s)
        });
        self.filtered_drift_s = Some(error_s);
        let integral_s2 = self.integral_s2 + error_s * step_s;
        let wanted = STEERING_PROPORTIONAL_PER_S * error_s + STEERING_INTEGRAL_PER_S2 * integral_s2;
        // Integrate only while the output is in range, so a long run ahead does not wind up.
        if wanted.abs() <= STEERING_LIMIT {
            self.integral_s2 = integral_s2;
        }
        self.stretch = wanted.clamp(-STEERING_LIMIT, STEERING_LIMIT);
        1.0 + self.stretch
    }

    /// After a jump in the drift, the filter starts again from the next drift.
    fn restart_filter(&mut self) {
        self.filtered_drift_s = None;
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "the drift stays far below 2^63 ns"
    )]
    fn filtered_drift_ns(&self) -> Option<i128> {
        self.filtered_drift_s
            .map(|drift_s| (drift_s * 1e9).round() as i128)
    }
}

/// Tells a stall from stamp jitter by the host-time step between consecutive stamps.
#[derive(Default)]
struct StallDetector {
    /// Mean size of the steps that were not stalls.
    mean_step_ns: f64,
    steps: u32,
}

impl StallDetector {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the scaled mean is capped at 70 ms"
    )]
    fn limit_ns(&self) -> i128 {
        if self.steps < STALL_WARMUP_STEPS {
            return DRIFT_LIMIT_NS;
        }
        let scaled_ns = (self.mean_step_ns * STALL_JITTER_FACTOR).min(1e9) as i128;
        scaled_ns.clamp(STALL_FLOOR_NS, DRIFT_LIMIT_NS)
    }

    /// Whether the step is a stall. A step that is not one updates the mean step size.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a step under the limit is below 2^52 ns"
    )]
    fn is_stall(&mut self, step_ns: i128) -> bool {
        let limit_ns = self.limit_ns();
        if step_ns > limit_ns {
            return true;
        }
        self.steps = self.steps.saturating_add(1);
        let weight = (1.0 / f64::from(self.steps)).max(STEP_SIZE_WEIGHT);
        let size_ns = step_ns.abs().min(limit_ns) as f64;
        self.mean_step_ns += (size_ns - self.mean_step_ns) * weight;
        false
    }
}

/// Silence of a gap that did not fit in the file before rotation. It opens the next file.
#[derive(Clone, Copy)]
struct CarriedGap {
    gap: GapRecord,
    samples: u64,
}

pub(crate) struct StemTrack {
    stem: Stem,
    file: String,
    wav: StemWav,
    epoch: RateEpoch,
    steering: ClockSteering,
    stalls: StallDetector,
    samples_in: u64,
    /// Host time of the file's first sample.
    anchor_host_ns: Option<u64>,
    /// Input samples received and not yet written, waiting for the stamp that places them.
    held: Vec<f32>,
    /// Stream index of `held[0]`.
    held_start_index: u64,
    pending: VecDeque<BufferStamp>,
    newest_stamp: Option<BufferStamp>,
    /// Stream index where the newest buffer's accepted samples end, and how many it lost there.
    pending_loss: Option<(u64, u64)>,
    /// Frames the newest buffer lost that get filled at their own spot, so the next stall step
    /// does not count them again.
    newest_loss_frames: u64,
    /// Set by a device switch: the next gap is the time between the devices.
    is_after_switch: bool,
    devices: Vec<DeviceRecord>,
    rates_in: Vec<RateRecord>,
    gaps: Vec<GapRecord>,
    new_gaps: Vec<GapRecord>,
    runs_ahead: Vec<AheadRecord>,
    /// Index into `runs_ahead` of the span still running.
    open_ahead: Option<usize>,
    /// The lead a rotation took from the old file's run ahead, for the new file's first span.
    carried_lead_ns: Option<i128>,
    carried_gap: Option<CarriedGap>,
    /// Host time minus file position at the newest stamp, unfiltered.
    drift_ns: Option<i128>,
    ring_losses: StemLosses,
    output_scratch: Vec<f32>,
    peak_recent: f32,
    peak: f32,
    stopped_by: Option<String>,
}

impl StemTrack {
    pub(crate) fn create(
        stem: Stem,
        file: String,
        path: &std::path::Path,
        device: String,
        opened_rate_hz: u32,
    ) -> Result<Self, StemWriterError> {
        if opened_rate_hz == 0 {
            return Err(StemWriterError::ZeroRate { stem });
        }
        let wav = StemWav::create(path)?;
        let epoch = RateEpoch::new(opened_rate_hz, 0, 1.0)?;
        let output_scratch = vec![0.0; epoch.resampler.output_frames_max()];
        Ok(Self {
            stem,
            file,
            wav,
            epoch,
            steering: ClockSteering::default(),
            stalls: StallDetector::default(),
            samples_in: 0,
            anchor_host_ns: None,
            held: Vec::new(),
            held_start_index: 0,
            pending: VecDeque::new(),
            newest_stamp: None,
            pending_loss: None,
            newest_loss_frames: 0,
            is_after_switch: false,
            devices: vec![DeviceRecord {
                start_ms: 0.0,
                name: device,
            }],
            rates_in: vec![RateRecord {
                start_ms: 0.0,
                rate_hz: opened_rate_hz,
            }],
            gaps: Vec::new(),
            new_gaps: Vec::new(),
            runs_ahead: Vec::new(),
            open_ahead: None,
            carried_lead_ns: None,
            carried_gap: None,
            drift_ns: None,
            ring_losses: StemLosses::default(),
            output_scratch,
            peak_recent: 0.0,
            peak: 0.0,
            stopped_by: None,
        })
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped_by.is_some()
    }

    pub(crate) fn stop(&mut self, reason: String) {
        self.stopped_by = Some(reason);
        self.held.clear();
        self.pending.clear();
    }

    /// Takes one drain's stamps and samples. Each stamp refers to samples given in this call or
    /// an earlier one, as `StemReader::drain` guarantees.
    pub(crate) fn write(
        &mut self,
        stamps: &[BufferStamp],
        samples: &[f32],
    ) -> Result<(), StemWriterError> {
        self.pending.extend(stamps.iter().copied());
        self.held.extend_from_slice(samples);
        self.place_held()
    }

    pub(crate) fn note_ring_losses(&mut self, losses: StemLosses) {
        self.ring_losses = losses;
    }

    pub(crate) fn take_new_gaps(&mut self) -> Vec<GapRecord> {
        std::mem::take(&mut self.new_gaps)
    }

    /// Writes all held audio of the old device, then starts a new stream: its sample index
    /// starts again at 0 and its clock gets fresh steering. The anchor stays.
    pub(crate) fn switch_device(
        &mut self,
        device: String,
        rate_hz: u32,
    ) -> Result<(), StemWriterError> {
        if rate_hz == 0 {
            return Err(StemWriterError::ZeroRate { stem: self.stem });
        }
        self.write_everything_held()?;
        self.pending.clear();
        self.held_start_index = 0;
        self.newest_stamp = None;
        self.pending_loss = None;
        self.newest_loss_frames = 0;
        self.steering = ClockSteering::default();
        self.stalls = StallDetector::default();
        self.epoch
            .resampler
            .set_resample_ratio_relative(1.0, true)
            .map_err(|source| StemWriterError::Resample { source })?;
        if rate_hz != self.epoch.rate_hz {
            self.switch_rate(rate_hz)?;
        }
        let start_ms = self.position_ms();
        tracing::info!(
            capture.stem = %self.stem,
            device.name = %device,
            audio.sample_rate_hz = rate_hz,
            stem.position_ms = start_ms,
            "stem.device_switched"
        );
        self.devices.push(DeviceRecord {
            start_ms,
            name: device,
        });
        self.is_after_switch = true;
        Ok(())
    }

    /// Makes the samples durable, then the header.
    pub(crate) fn sync(&mut self) -> Result<(), StemWriterError> {
        self.wav.sync()
    }

    /// Writes every held sample, flushes the resampler and syncs the file.
    pub(crate) fn finish(&mut self) -> Result<(), StemWriterError> {
        if !self.is_stopped() {
            self.write_everything_held()?;
            self.close_epoch()?;
        }
        self.wav.sync()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.wav.samples_written() >= SEGMENT_SAMPLE_LIMIT
    }

    /// Ends this file at the current position: the resampler's tail goes in, an open run ahead
    /// closes with the lead it still has, and the file is synced. `record` then lists it.
    pub(crate) fn end_file(&mut self) -> Result<(), StemWriterError> {
        self.close_epoch()?;
        if let Some(index) = self.open_ahead.take() {
            let lead_ns = (-self.filtered_drift_ns().unwrap_or(0)).max(0);
            self.close_ahead(index, lead_ns);
            self.carried_lead_ns = Some(lead_ns);
        }
        self.wav.sync()
    }

    /// Opens the next file where the ended one stopped. The stream, its anchor in host time and
    /// its steering carry over. A lead left from a run ahead opens the new file's first span, and
    /// silence that did not fit opens the file.
    pub(crate) fn start_file(
        &mut self,
        file: String,
        path: &std::path::Path,
    ) -> Result<(), StemWriterError> {
        let wav = StemWav::create(path)?;
        let ended_ns = whole_ns(ns_for_samples(self.wav.samples_written(), OUTPUT_RATE_HZ));
        let new_anchor = self.anchor_host_ns.map(|anchor_ns| anchor_ns + ended_ns);
        self.wav = wav;
        self.file = file;
        self.anchor_host_ns = new_anchor;
        let rate_hz = self.epoch.rate_hz;
        let device = self
            .devices
            .last()
            .map(|device| device.name.clone())
            .unwrap_or_default();
        self.devices = vec![DeviceRecord {
            start_ms: 0.0,
            name: device,
        }];
        self.rates_in = vec![RateRecord {
            start_ms: 0.0,
            rate_hz,
        }];
        self.gaps.clear();
        self.runs_ahead.clear();
        if let Some(lead_ns) = self.carried_lead_ns.take()
            && lead_ns > AHEAD_CLOSED_NS
        {
            self.open_ahead_span(lead_ns, new_anchor.unwrap_or(0));
        }
        if let Some(carried) = self.carried_gap.take() {
            self.write_gap_silence(carried.gap, carried.samples)?;
        }
        self.epoch = RateEpoch::new(
            rate_hz,
            self.wav.samples_written(),
            1.0 + self.steering.stretch,
        )?;
        Ok(())
    }

    /// The stem as the manifest lists it, with the header's sample count from the last sync.
    pub(crate) fn record(&self, started_at: Option<jiff::Timestamp>) -> StemRecord {
        let synced = self.wav.synced_samples();
        let last_host_time_ns = self.anchor_host_ns.map(|anchor_ns| {
            let file_end_ns =
                signed_ns(u128::from(anchor_ns) + ns_for_samples(synced, OUTPUT_RATE_HZ));
            u64::try_from(file_end_ns + self.filtered_drift_ns().unwrap_or(0)).unwrap_or(anchor_ns)
        });
        StemRecord {
            stem: self.stem,
            file: self.file.clone(),
            first_host_time_ns: self.anchor_host_ns,
            started_at,
            last_host_time_ns,
            sample_rate_hz: OUTPUT_RATE_HZ,
            samples_written: synced,
            devices: self.devices.clone(),
            rates_in: self.rates_in.clone(),
            gaps: self.gaps.clone(),
            runs_ahead: self.runs_ahead.clone(),
            ring_losses: self.ring_losses,
            stopped_by: self.stopped_by.clone(),
        }
    }

    pub(crate) fn first_host_time_ns(&self) -> Option<u64> {
        self.anchor_host_ns
    }

    pub(crate) fn progress(&self) -> StemProgress {
        StemProgress {
            stem: self.stem,
            file: self.file.clone(),
            samples_written: self.wav.samples_written(),
            samples_in: self.samples_in,
            rate_in_hz: self.epoch.rate_hz,
            first_host_time_ns: self.anchor_host_ns,
            gap_count: self.gaps.len(),
            gap_total_ms: self
                .gaps
                .iter()
                .fold(0.0, |total, gap| total + gap.length_ms),
            gap_largest_ms: self
                .gaps
                .iter()
                .map(|gap| gap.length_ms)
                .fold(0.0, f64::max),
            drift_ms: self.filtered_drift_ns().map(ms_from_ns),
            steering_ppm: self.steering.stretch * 1e6,
            ahead_count: self.runs_ahead.len(),
            peak_recent: self.peak_recent,
            peak: self.peak,
            stopped_by: self.stopped_by.clone(),
        }
    }

    pub(crate) fn reset_recent_peak(&mut self) {
        self.peak_recent = 0.0;
    }

    /// The low-passed drift, or the raw one before steering has run.
    fn filtered_drift_ns(&self) -> Option<i128> {
        self.steering.filtered_drift_ns().or(self.drift_ns)
    }

    fn received_index(&self) -> u64 {
        self.held_start_index + self.held.len() as u64
    }

    fn write_everything_held(&mut self) -> Result<(), StemWriterError> {
        self.place_held()?;
        let received = self.received_index();
        self.write_held_until(received)
    }

    /// Writes held samples up to each pending stamp, then applies the stamp. Samples past the
    /// newest buffer's accepted ones wait for their own stamp, so a gap lands before them. Once
    /// silence carries over to the next file, everything waits for the rotation.
    fn place_held(&mut self) -> Result<(), StemWriterError> {
        while let Some(&stamp) = self.pending.front() {
            if stamp.sample_index > self.received_index() || self.carried_gap.is_some() {
                break;
            }
            self.write_held_until(stamp.sample_index)?;
            self.apply_stamp(stamp)?;
            self.pending.pop_front();
        }
        if self.carried_gap.is_some() {
            return Ok(());
        }
        if let Some(newest) = self.newest_stamp {
            let buffer_end = newest.sample_index + u64::from(newest.accepted_count);
            self.write_held_until(buffer_end.min(self.received_index()))?;
        }
        let hold_limit = u64::from(self.epoch.rate_hz) * HOLD_LIMIT_MS / 1_000;
        if self.held.len() as u64 > hold_limit {
            let received = self.received_index();
            self.write_held_until(received)?;
        }
        Ok(())
    }

    /// Writes held samples up to a stream index. Where a buffer lost its tail to a full ring,
    /// silence of the lost length goes in right after its last accepted sample.
    fn write_held_until(&mut self, end_index: u64) -> Result<(), StemWriterError> {
        if let Some((loss_index, lost)) = self.pending_loss
            && loss_index <= end_index
        {
            self.feed_held_until(loss_index)?;
            if self.held_start_index == loss_index {
                self.pending_loss = None;
                let start_host_time_ns = self.host_time_at_position_ns();
                self.fill_silence(
                    ns_for_samples(lost, self.epoch.rate_hz),
                    GapCause::RingOverflow,
                    start_host_time_ns,
                )?;
            }
        }
        self.feed_held_until(end_index)
    }

    fn feed_held_until(&mut self, end_index: u64) -> Result<(), StemWriterError> {
        let Some(count) = end_index.checked_sub(self.held_start_index) else {
            return Ok(());
        };
        let count = usize::try_from(count)
            .unwrap_or(usize::MAX)
            .min(self.held.len());
        if count == 0 {
            return Ok(());
        }
        let mut held = std::mem::take(&mut self.held);
        let outcome = self.feed(&held[..count]);
        held.drain(..count);
        self.held = held;
        self.held_start_index += count as u64;
        outcome
    }

    fn apply_stamp(&mut self, stamp: BufferStamp) -> Result<(), StemWriterError> {
        let next_index = self.held_start_index;
        let previous = self
            .newest_stamp
            .map(|newest| (newest, self.newest_loss_frames));
        if stamp.sample_rate_hz != 0 && stamp.sample_rate_hz != self.epoch.rate_hz {
            self.switch_rate(stamp.sample_rate_hz)?;
        }
        self.newest_loss_frames = 0;
        if stamp.sample_index == next_index && stamp.frame_count > stamp.accepted_count {
            let lost = u64::from(stamp.frame_count - stamp.accepted_count);
            self.pending_loss = Some((stamp.sample_index + u64::from(stamp.accepted_count), lost));
            self.newest_loss_frames = lost;
        }

        // A stamp whose samples went in unplaced (they waited past the hold limit) still places
        // what comes next.
        let since_stamp_ns = ns_for_samples(
            next_index.saturating_sub(stamp.sample_index),
            self.epoch.rate_hz,
        );
        let host_next_ns = whole_ns(u128::from(stamp.host_time_ns) + since_stamp_ns);
        if self.anchor_host_ns.is_none() {
            let anchor_ns = host_next_ns.saturating_sub(whole_ns(self.position_ns()));
            self.anchor_host_ns = Some(anchor_ns);
            self.drift_ns = Some(0);
            tracing::info!(
                capture.stem = %self.stem,
                stem.first_host_time_ns = anchor_ns,
                audio.sample_rate_hz = self.epoch.rate_hz,
                "stem.anchored"
            );
        } else {
            if let Some((previous, previous_loss_frames)) = previous {
                let rate_hz = if previous.sample_rate_hz == 0 {
                    self.epoch.rate_hz
                } else {
                    previous.sample_rate_hz
                };
                let frames =
                    stamp.sample_index.saturating_sub(previous.sample_index) + previous_loss_frames;
                let step_ns = i128::from(stamp.host_time_ns)
                    - i128::from(previous.host_time_ns)
                    - signed_ns(ns_for_samples(frames, rate_hz));
                if self.stalls.is_stall(step_ns) {
                    self.fill_stall(step_ns, host_next_ns)?;
                }
            }
            let drift_ns = self.drift_at(host_next_ns);
            self.drift_ns = Some(drift_ns);
            self.follow_host_time(drift_ns, host_next_ns)?;
        }
        self.is_after_switch = false;
        self.newest_stamp = Some(stamp);
        Ok(())
    }

    /// Host time minus the file's position in host time, for the next input sample. Silence
    /// waiting for the next file counts as written.
    fn drift_at(&self, host_next_ns: u64) -> i128 {
        let anchor_ns = self.anchor_host_ns.unwrap_or(host_next_ns);
        let carried_ns = self
            .carried_gap
            .map_or(0, |carried| ns_for_samples(carried.samples, OUTPUT_RATE_HZ));
        let file_host_ns = u128::from(anchor_ns) + self.position_ns() + carried_ns;
        i128::from(host_next_ns) - signed_ns(file_host_ns)
    }

    /// Host time of the next input sample, by the file position and the newest drift.
    fn host_time_at_position_ns(&self) -> u64 {
        self.anchor_host_ns.map_or(0, |anchor_ns| {
            let position_ns = signed_ns(u128::from(anchor_ns) + self.position_ns());
            u64::try_from(position_ns + self.drift_ns.unwrap_or(0)).unwrap_or(anchor_ns)
        })
    }

    /// The device delivered nothing for `step_ns`. Outside a run ahead the step becomes silence.
    /// Inside one, the silence also takes up the lead: the span closes at the gap with the lead
    /// it had, and the audio after the gap sits at its host time again.
    fn fill_stall(&mut self, step_ns: i128, host_next_ns: u64) -> Result<(), StemWriterError> {
        let start_host_time_ns = u64::try_from(i128::from(host_next_ns) - step_ns).unwrap_or(0);
        let Some(index) = self.open_ahead.take() else {
            return self.fill_silence(
                u128::try_from(step_ns).unwrap_or(0),
                GapCause::NoAudioDelivered,
                start_host_time_ns,
            );
        };
        let drift_ns = self.drift_at(host_next_ns);
        self.close_ahead(index, (step_ns - drift_ns).max(0));
        self.steering.restart_filter();
        if drift_ns > 0 {
            self.fill_silence(
                u128::try_from(drift_ns).unwrap_or(0),
                GapCause::NoAudioDelivered,
                start_host_time_ns,
            )
        } else {
            if -drift_ns > AHEAD_CLOSED_NS {
                self.open_ahead_span(-drift_ns, host_next_ns);
            }
            Ok(())
        }
    }

    /// Drift past the limit with no stall step to explain it is filled with silence, or opens a
    /// run ahead. Anything smaller goes to steering, which stretches or squeezes the audio by at
    /// most 1,000 ppm. The anchor never moves.
    fn follow_host_time(&mut self, drift_ns: i128, host_ns: u64) -> Result<(), StemWriterError> {
        if drift_ns > DRIFT_LIMIT_NS {
            let cause = if self.is_after_switch {
                GapCause::DeviceSwitch
            } else {
                GapCause::NoAudioDelivered
            };
            if let Some(index) = self.open_ahead.take() {
                self.close_ahead(index, 0);
            }
            let start_host_time_ns = u64::try_from(i128::from(host_ns) - drift_ns).unwrap_or(0);
            self.fill_silence(
                u128::try_from(drift_ns).unwrap_or(0),
                cause,
                start_host_time_ns,
            )?;
            self.drift_ns = Some(0);
            self.steering.last_host_ns = Some(host_ns);
            self.steering.restart_filter();
            return Ok(());
        }
        if drift_ns < -DRIFT_LIMIT_NS {
            if let Some(index) = self.open_ahead {
                let span = &mut self.runs_ahead[index];
                span.max_ahead_ms = span.max_ahead_ms.max(ms_from_ns(-drift_ns));
            } else {
                self.open_ahead_span(-drift_ns, host_ns);
                self.steering.restart_filter();
            }
        }
        let ratio = self.steering.update(drift_ns, host_ns);
        if let Some(index) = self.open_ahead
            && self.filtered_drift_ns().unwrap_or(drift_ns) > -AHEAD_CLOSED_NS
        {
            self.open_ahead = None;
            self.close_ahead(index, 0);
        }
        self.epoch
            .resampler
            .set_resample_ratio_relative(ratio, true)
            .map_err(|source| StemWriterError::Resample { source })
    }

    fn open_ahead_span(&mut self, ahead_ns: i128, host_ns: u64) {
        let start_ms = self.position_ms();
        let lead_ms = ms_from_ns(ahead_ns);
        tracing::warn!(
            capture.stem = %self.stem,
            stem.position_ms = start_ms,
            stem.ahead_ms = lead_ms,
            "stem.audio_ahead"
        );
        self.open_ahead = Some(self.runs_ahead.len());
        self.runs_ahead.push(AheadRecord {
            start_ms,
            start_host_time_ns: host_ns,
            start_ahead_ms: lead_ms,
            max_ahead_ms: lead_ms,
            end_ms: None,
            end_ahead_ms: 0.0,
        });
    }

    /// Ends a run ahead at the current position, with the lead the file still has there.
    fn close_ahead(&mut self, index: usize, lead_ns: i128) {
        let end_ms = self.position_ms();
        let span = &mut self.runs_ahead[index];
        span.end_ms = Some(end_ms);
        span.end_ahead_ms = ms_from_ns(lead_ns);
    }

    /// Host-time span from the file's first sample to the next input sample.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a file holds at most 24 h of samples, far below 2^52"
    )]
    fn position_ns(&self) -> u128 {
        let position_samples = self.epoch.start_sample as f64 + self.epoch.out_position;
        u128_from_f64(position_samples * 1e9 / f64::from(OUTPUT_RATE_HZ))
    }

    fn position_ms(&self) -> f64 {
        ms_from_ns(signed_ns(self.position_ns()))
    }

    fn fill_silence(
        &mut self,
        length_ns: u128,
        cause: GapCause,
        start_host_time_ns: u64,
    ) -> Result<(), StemWriterError> {
        let gap = GapRecord {
            start_ms: self.position_ms(),
            start_host_time_ns,
            length_ms: ms_from_ns(signed_ns(length_ns)),
            cause,
        };
        tracing::warn!(
            capture.stem = %self.stem,
            gap.start_ms = gap.start_ms,
            gap.start_host_time_ns = start_host_time_ns,
            gap.length_ms = gap.length_ms,
            gap.cause = %cause,
            "stem.gap_filled"
        );
        self.new_gaps.push(gap);
        if length_ns < RESAMPLER_BYPASS_NS {
            self.gaps.push(gap);
            let silence_samples = samples_for_ns(length_ns, self.epoch.rate_hz);
            return self.queue_input(std::iter::repeat_n(
                0.0,
                usize::try_from(silence_samples).unwrap_or(0),
            ));
        }
        let rate_hz = self.epoch.rate_hz;
        self.close_epoch()?;
        self.write_gap_silence(gap, output_samples_for(length_ns))?;
        self.epoch = RateEpoch::new(
            rate_hz,
            self.wav.samples_written(),
            1.0 + self.steering.stretch,
        )?;
        Ok(())
    }

    /// Writes a gap's silence straight to the file, past the resampler, and lists it. Silence
    /// past the rotation limit carries over to open the next file.
    fn write_gap_silence(&mut self, gap: GapRecord, samples: u64) -> Result<(), StemWriterError> {
        let start_ms = ms_for_output(self.wav.samples_written());
        let room = SEGMENT_SAMPLE_LIMIT.saturating_sub(self.wav.samples_written());
        let in_file = samples.min(room);
        if in_file > 0 {
            self.gaps.push(GapRecord {
                start_ms,
                length_ms: ms_for_output(in_file),
                ..gap
            });
        }
        if in_file < samples {
            self.carried_gap = Some(CarriedGap {
                gap: GapRecord {
                    start_ms: 0.0,
                    start_host_time_ns: gap.start_host_time_ns
                        + whole_ns(ns_for_samples(in_file, OUTPUT_RATE_HZ)),
                    ..gap
                },
                samples: samples - in_file,
            });
        }
        let silence = [0.0; 1_024];
        let mut silence_left = in_file;
        while silence_left > 0 {
            let count =
                usize::try_from(silence_left).map_or(silence.len(), |left| left.min(silence.len()));
            self.wav.write(&silence[..count])?;
            silence_left -= count as u64;
        }
        Ok(())
    }

    fn switch_rate(&mut self, rate_hz: u32) -> Result<(), StemWriterError> {
        let old_rate_hz = self.epoch.rate_hz;
        let is_file_empty = self.wav.samples_written() == 0 && self.epoch.queued.is_empty();
        self.close_epoch()?;
        self.epoch = RateEpoch::new(
            rate_hz,
            self.wav.samples_written(),
            1.0 + self.steering.stretch,
        )?;
        self.output_scratch
            .resize(self.epoch.resampler.output_frames_max(), 0.0);
        let start_ms = self.position_ms();
        tracing::info!(
            capture.stem = %self.stem,
            audio.sample_rate_hz = rate_hz,
            audio.previous_sample_rate_hz = old_rate_hz,
            stem.position_ms = start_ms,
            "stem.rate_changed"
        );
        // A rate known before any audio replaces the opened rate rather than adding a stretch.
        if let Some(last) = self.rates_in.last_mut()
            && is_file_empty
        {
            last.rate_hz = rate_hz;
        } else {
            self.rates_in.push(RateRecord { start_ms, rate_hz });
        }
        Ok(())
    }

    fn feed(&mut self, samples: &[f32]) -> Result<(), StemWriterError> {
        let peak = samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        self.peak_recent = self.peak_recent.max(peak);
        self.peak = self.peak.max(peak);
        self.samples_in += samples.len() as u64;
        self.queue_input(samples.iter().copied())
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "one drain's sample count is far below 2^52"
    )]
    fn queue_input(&mut self, samples: impl Iterator<Item = f32>) -> Result<(), StemWriterError> {
        let before = self.epoch.queued.len();
        self.epoch.queued.extend(samples);
        let added = self.epoch.queued.len() - before;
        self.epoch.out_position +=
            added as f64 * base_ratio(self.epoch.rate_hz) * (1.0 + self.steering.stretch);
        while self.epoch.queued.len() >= self.epoch.resampler.input_frames_next() {
            self.resample_chunk()?;
        }
        Ok(())
    }

    /// Runs one resampler call on a full chunk at the front of the queue.
    fn resample_chunk(&mut self) -> Result<(), StemWriterError> {
        let needed = self.epoch.resampler.input_frames_next();
        let produced = run_resampler(
            &mut self.epoch.resampler,
            &self.epoch.queued[..needed],
            &mut self.output_scratch,
        )?;
        self.epoch.queued.drain(..needed);
        self.emit(produced, None)
    }

    /// Flushes the resampler's tail so the stretch holds exactly its due samples, keeping every
    /// later sample at its host time.
    fn close_epoch(&mut self) -> Result<(), StemWriterError> {
        let due = self.epoch.samples_out_due();
        let needed = self.epoch.resampler.input_frames_next();
        let mut padded = vec![0.0; needed];
        // The queue holds under one chunk, and the delay is a fraction of one: a few calls. The
        // bound guards a resampler that stops producing.
        let mut calls_left = 8;
        while self.epoch.samples_out < due && calls_left > 0 {
            calls_left -= 1;
            let take = self.epoch.queued.len().min(needed);
            padded.fill(0.0);
            padded[..take].copy_from_slice(&self.epoch.queued[..take]);
            self.epoch.queued.drain(..take);
            let produced =
                run_resampler(&mut self.epoch.resampler, &padded, &mut self.output_scratch)?;
            self.emit(produced, Some(due))?;
        }
        // Input still queued once the due count is out is under one output sample: the gap
        // between the steering ratio a sample was queued under and the one it ran under.
        self.epoch.queued.clear();
        Ok(())
    }

    /// Puts the first `produced` scratch samples in the file after the resampler's start-up
    /// delay, and never past `due` samples for the stretch.
    fn emit(&mut self, produced: usize, due: Option<u64>) -> Result<(), StemWriterError> {
        let skip = self.epoch.delay_left.min(produced);
        self.epoch.delay_left -= skip;
        let room = due.map_or(usize::MAX, |due| {
            usize::try_from(due.saturating_sub(self.epoch.samples_out)).unwrap_or(usize::MAX)
        });
        let keep = (produced - skip).min(room);
        self.epoch.samples_out += keep as u64;
        self.wav.write(&self.output_scratch[skip..skip + keep])
    }
}

fn run_resampler(
    resampler: &mut Async<f32>,
    input_frames: &[f32],
    scratch: &mut [f32],
) -> Result<usize, StemWriterError> {
    let input = InterleavedSlice::new(input_frames, 1, input_frames.len())
        .map_err(|source| StemWriterError::BufferShape { source })?;
    let scratch_len = scratch.len();
    let mut output = InterleavedSlice::new_mut(scratch, 1, scratch_len)
        .map_err(|source| StemWriterError::BufferShape { source })?;
    let (_, produced) = resampler
        .process_into_buffer(&input, &mut output, None)
        .map_err(|source| StemWriterError::Resample { source })?;
    Ok(produced)
}

fn base_ratio(rate_hz: u32) -> f64 {
    f64::from(OUTPUT_RATE_HZ) / f64::from(rate_hz.max(1))
}

fn ns_for_samples(samples: u64, rate_hz: u32) -> u128 {
    u128::from(samples) * NS_PER_S / u128::from(rate_hz.max(1))
}

fn samples_for_ns(length_ns: u128, rate_hz: u32) -> u64 {
    let samples = (length_ns * u128::from(rate_hz) + NS_PER_S / 2) / NS_PER_S;
    u64::try_from(samples).unwrap_or(u64::MAX)
}

fn output_samples_for(length_ns: u128) -> u64 {
    samples_for_ns(length_ns, OUTPUT_RATE_HZ)
}

#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts stay far below 2^52"
)]
fn ms_for_output(samples: u64) -> f64 {
    samples as f64 * 1e3 / f64::from(OUTPUT_RATE_HZ)
}

fn whole_ns(nanos: u128) -> u64 {
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

fn signed_ns(nanos: u128) -> i128 {
    i128::try_from(nanos).unwrap_or(i128::MAX)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a non-negative span far below 2^64 ns"
)]
fn u128_from_f64(nanos: f64) -> u128 {
    nanos.round().max(0.0) as u128
}

#[expect(
    clippy::cast_precision_loss,
    reason = "spans within a call stay far below 2^52 ns"
)]
fn ms_from_ns(nanos: i128) -> f64 {
    nanos as f64 / 1e6
}
