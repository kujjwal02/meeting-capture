//! The app log line for each helper report. The app and the inject check both log reports
//! through here, so a detector reads the same lines from either.

use crate::{AudioReport, DeviceChangeCause, Stem, VideoReport};

/// Logs one audio helper report for the call. Heartbeats stay DEBUG with no `event.name`:
/// detectors read the level rows instead.
pub fn log_audio_report(call_id: &str, report: &AudioReport) {
    match report {
        AudioReport::Recording {
            segment,
            stream,
            device,
            rate_hz,
            first_host_time_ns,
            mic_route,
        } => tracing::info!(
            event.name = "capture.audio.recording",
            call.id = call_id,
            segment.number = segment,
            audio.stream = %stream,
            audio.device = device,
            audio.rate_hz = rate_hz,
            audio.first_host_time_ns = first_host_time_ns,
            mic.reason = mic_route.as_ref().map(|route| route.reason.as_str()),
            "audio stream recording"
        ),
        AudioReport::Gap {
            segment,
            stream,
            start_ms,
            length_ms,
            cause,
            ..
        } => tracing::warn!(
            event.name = "capture.audio.gap",
            call.id = call_id,
            segment.number = segment,
            audio.stream = %stream,
            audio.gap_start_ms = start_ms,
            audio.gap_length_ms = length_ms,
            error.type = %cause,
            "audio gap"
        ),
        AudioReport::Heartbeat { segment, .. } => tracing::debug!(
            call.id = call_id,
            segment.number = segment,
            "audio heartbeat"
        ),
        AudioReport::Rotated {
            from_segment,
            to_segment,
        } => tracing::info!(
            event.name = "capture.audio.rotated",
            call.id = call_id,
            segment.number = to_segment,
            segment.previous_number = from_segment,
            "audio segment rotated"
        ),
        AudioReport::Stopped { segment } => tracing::info!(
            event.name = "capture.audio.segment_closed",
            call.id = call_id,
            segment.number = segment,
            "audio segment closed"
        ),
        AudioReport::DeviceChanged { .. } => log_device_changed(call_id, report),
        AudioReport::MicUnavailable {
            segment,
            old_device,
            cause,
            message,
        } => log_mic_unavailable(call_id, *segment, old_device.as_deref(), *cause, message),
        AudioReport::Permission { service, state } => tracing::info!(
            event.name = "capture.audio.permission",
            call.id = call_id,
            permission.service = %service,
            permission.state = %state,
            "permission checked"
        ),
        AudioReport::SystemSleep { segment } => tracing::info!(
            event.name = "capture.audio.sleep",
            call.id = call_id,
            segment.number = segment,
            "the Mac is going to sleep"
        ),
        AudioReport::SystemWake => tracing::info!(
            event.name = "capture.audio.wake",
            call.id = call_id,
            "the Mac woke"
        ),
        AudioReport::Error { kind, message } => tracing::error!(
            event.name = "capture.audio.error",
            call.id = call_id,
            error.type = %kind,
            exception.message = message,
            "audio helper error"
        ),
    }
}

// Apart from `log_audio_report`, which would pass a function's length limit with it.
fn log_device_changed(call_id: &str, report: &AudioReport) {
    let AudioReport::DeviceChanged {
        segment,
        stream,
        old_device,
        new_device,
        rate_hz,
        cause,
        mic_route,
    } = report
    else {
        return;
    };
    tracing::info!(
        event.name = "capture.audio.device_changed",
        call.id = call_id,
        segment.number = segment,
        audio.stream = %stream,
        audio.previous_device = old_device.as_deref(),
        audio.device = new_device,
        audio.rate_hz = rate_hz,
        audio.change_cause = %cause,
        mic.reason = mic_route.as_ref().map(|route| route.reason.as_str()),
        "audio device changed"
    );
}

fn log_mic_unavailable(
    call_id: &str,
    segment: u32,
    old_device: Option<&str>,
    cause: Option<DeviceChangeCause>,
    message: &str,
) {
    tracing::warn!(
        event.name = "capture.audio.mic_unavailable",
        call.id = call_id,
        segment.number = segment,
        audio.stream = Stem::Mic.as_str(),
        audio.previous_device = old_device,
        audio.change_cause = cause.map(|cause| cause.to_string()),
        exception.message = message,
        "audio mic unavailable"
    );
}

/// Logs one video helper report for the call. Heartbeats stay DEBUG with no `event.name`.
pub fn log_video_report(call_id: &str, report: &VideoReport) {
    match report {
        VideoReport::Recording {
            segment,
            window,
            width_px,
            height_px,
            codec,
            first_host_time_ns,
        } => tracing::info!(
            event.name = "capture.video.recording",
            call.id = call_id,
            segment.number = segment,
            video.window_id = window.window_id,
            video.width_px = width_px,
            video.height_px = height_px,
            video.codec = %codec,
            video.first_host_time_ns = first_host_time_ns,
            "video recording"
        ),
        VideoReport::Gap {
            segment,
            length_ms,
            cause,
            ..
        } => tracing::warn!(
            event.name = "capture.video.gap",
            call.id = call_id,
            segment.number = segment,
            video.gap_length_ms = length_ms,
            error.type = %cause,
            "video gap"
        ),
        VideoReport::WindowChanged {
            segment,
            cause,
            window,
            ..
        } => tracing::info!(
            event.name = "capture.video.window_changed",
            call.id = call_id,
            segment.number = segment,
            video.change_cause = %cause,
            video.window_id = window.as_ref().map(|window| window.window_id),
            "video window changed"
        ),
        VideoReport::Permission { service, state } => tracing::info!(
            event.name = "capture.video.permission",
            call.id = call_id,
            permission.service = %service,
            permission.state = %state,
            "permission checked"
        ),
        VideoReport::Heartbeat { segment, .. } => tracing::debug!(
            call.id = call_id,
            segment.number = segment,
            "video heartbeat"
        ),
        VideoReport::Stopped { segment } => tracing::info!(
            event.name = "capture.video.segment_closed",
            call.id = call_id,
            segment.number = segment,
            "video segment closed"
        ),
        VideoReport::Error { kind, message } => tracing::error!(
            event.name = "capture.video.error",
            call.id = call_id,
            error.type = %kind,
            exception.message = message,
            "video helper error"
        ),
    }
}
