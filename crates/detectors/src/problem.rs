//! The closed list of problems a call can have, and the layer each one points at.

/// The part of the system a problem points at. Its code goes in `mc.problem.layer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Layer {
    /// The `mc-capture audio` process.
    AudioHelper,
    /// The video helper, its stream or Screen Recording.
    Video,
    /// The process tap, its aggregate or its permission.
    AudioRemote,
    /// The mic stream, its device or its permission.
    AudioMic,
    /// mc-meet's port or the browser.
    Extension,
    /// The Meet tab.
    MeetPage,
    /// Meet's caption feature or its button.
    MeetCaptions,
    /// mc-meet's frame parser.
    CaptionDecoder,
    /// The Slack window reader.
    HuddleReader,
    /// No caption arrives and no cause is in view.
    Captions,
}

impl Layer {
    /// Lower layers first: on a tie, the verdict blames the earlier one, since a lower layer's
    /// fault explains the symptoms above it.
    pub const BY_DEPTH: [Self; 10] = [
        Self::AudioHelper,
        Self::Video,
        Self::AudioRemote,
        Self::AudioMic,
        Self::Extension,
        Self::MeetPage,
        Self::MeetCaptions,
        Self::CaptionDecoder,
        Self::HuddleReader,
        Self::Captions,
    ];

    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::AudioHelper => "audio_helper",
            Self::Video => "video",
            Self::AudioRemote => "audio_remote",
            Self::AudioMic => "audio_mic",
            Self::Extension => "extension",
            Self::MeetPage => "meet_page",
            Self::MeetCaptions => "meet_captions",
            Self::CaptionDecoder => "caption_decoder",
            Self::HuddleReader => "huddle_reader",
            Self::Captions => "captions",
        }
    }

    /// Whether the layer records media rather than captions.
    #[must_use]
    pub fn is_capture(self) -> bool {
        match self {
            Self::AudioHelper | Self::Video | Self::AudioRemote | Self::AudioMic => true,
            Self::Extension
            | Self::MeetPage
            | Self::MeetCaptions
            | Self::CaptionDecoder
            | Self::HuddleReader
            | Self::Captions => false,
        }
    }

    /// Whether the layer records audio, for the wording of a notification.
    #[must_use]
    pub fn is_audio(self) -> bool {
        match self {
            Self::AudioHelper | Self::AudioRemote | Self::AudioMic => true,
            Self::Video
            | Self::Extension
            | Self::MeetPage
            | Self::MeetCaptions
            | Self::CaptionDecoder
            | Self::HuddleReader
            | Self::Captions => false,
        }
    }
}

/// Why part of a call is not recording. One closed list for every detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Problem {
    /// Meet's captions are off, so no caption arrives.
    MeetCaptionsOff,
    /// Meet refuses captions: its button is disabled or says they are unavailable.
    MeetCaptionsUnavailable,
    /// Frames arrive on a caption channel, and none of them decode.
    CaptionParseFailures,
    /// Captions are on and nothing arrived for 2 minutes.
    CaptionsSilent,
    /// Nothing arrived for 2 minutes, and the source cannot tell whether captions are on.
    NoCaptions,
    /// No mc-meet port holds the call: the browser quit or crashed, or the extension stopped.
    ExtensionDisconnected,
    /// Every Meet tab of the call closed, reloaded or crashed, and none came back yet.
    MeetPageAway,
    /// Slack's captions are off.
    HuddleCaptionsOff,
    /// No layout profile can read the huddle window.
    HuddleCaptionsUnreadable,
    MicPermissionRefused,
    /// The mic stream stopped and no replacement records yet.
    MicLost,
    /// The mic delivers only zeros.
    MicDigitalSilence,
    AudioCapturePermissionRefused,
    /// The process tap stopped or died and no rebuilt tap records yet.
    RemoteLost,
    /// The tap delivers only zeros while the user talks, so nobody answers in the recording.
    RemoteDigitalSilence,
    /// The audio helper exited, hung or did not start.
    AudioHelperDown,
    /// The Mac woke and no audio segment opened.
    AudioNotResumed,
    ScreenRecordingRefused,
    /// The video helper exited, hung or did not start.
    VideoHelperDown,
    /// The window stream stopped or stalled and the helper rebuilds it.
    VideoStreamLost,
}

impl Problem {
    /// The code in `error.type`. Meet's and Slack's captions off share one, as they did before
    /// the layers split them, so older reports read the same.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::MeetCaptionsOff | Self::HuddleCaptionsOff => "captions_off",
            Self::MeetCaptionsUnavailable => "meet_captions_unavailable",
            Self::CaptionParseFailures => "caption_parse_failures",
            Self::CaptionsSilent => "captions_silent",
            Self::NoCaptions => "no_captions",
            Self::ExtensionDisconnected => "extension_disconnected",
            Self::MeetPageAway => "meet_page_away",
            Self::HuddleCaptionsUnreadable => "huddle_captions_unreadable",
            Self::MicPermissionRefused => "mic_permission_refused",
            Self::MicLost => "mic_lost",
            Self::MicDigitalSilence => "mic_digital_silence",
            Self::AudioCapturePermissionRefused => "audio_capture_permission_refused",
            Self::RemoteLost => "remote_lost",
            Self::RemoteDigitalSilence => "remote_digital_silence",
            Self::AudioHelperDown => "audio_helper_down",
            Self::AudioNotResumed => "audio_not_resumed",
            Self::ScreenRecordingRefused => "screen_recording_refused",
            Self::VideoHelperDown => "video_helper_down",
            Self::VideoStreamLost => "video_stream_lost",
        }
    }

    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::MeetCaptionsOff | Self::HuddleCaptionsOff => "captions are off",
            Self::MeetCaptionsUnavailable => "Meet captions unavailable (Google)",
            Self::CaptionParseFailures => "captions arrive but cannot be read",
            Self::CaptionsSilent => "no caption for 2 min while captions are on",
            Self::NoCaptions => "no caption for 2 min",
            Self::ExtensionDisconnected => "mc-meet disconnected",
            Self::MeetPageAway => "the Meet tab closed or reloaded",
            Self::HuddleCaptionsUnreadable => "the huddle window cannot be read",
            Self::MicPermissionRefused => "the Microphone permission is off",
            Self::MicLost => "your mic is lost",
            Self::MicDigitalSilence => "your mic records only silence",
            Self::AudioCapturePermissionRefused => "the System Audio Recording permission is off",
            Self::RemoteLost => "the call's sound is lost",
            Self::RemoteDigitalSilence => "the call's sound records only silence",
            Self::AudioHelperDown => "the audio recorder stopped",
            Self::AudioNotResumed => "audio did not restart after sleep",
            Self::ScreenRecordingRefused => "the Screen Recording permission is off",
            Self::VideoHelperDown => "the video recorder stopped",
            Self::VideoStreamLost => "the window video stopped",
        }
    }

    #[must_use]
    pub fn layer(self) -> Layer {
        match self {
            Self::ExtensionDisconnected => Layer::Extension,
            Self::MeetPageAway => Layer::MeetPage,
            Self::MeetCaptionsOff | Self::MeetCaptionsUnavailable => Layer::MeetCaptions,
            Self::CaptionParseFailures => Layer::CaptionDecoder,
            Self::HuddleCaptionsOff | Self::HuddleCaptionsUnreadable => Layer::HuddleReader,
            Self::CaptionsSilent | Self::NoCaptions => Layer::Captions,
            Self::MicPermissionRefused | Self::MicLost | Self::MicDigitalSilence => Layer::AudioMic,
            Self::AudioCapturePermissionRefused | Self::RemoteLost | Self::RemoteDigitalSilence => {
                Layer::AudioRemote
            }
            Self::AudioHelperDown | Self::AudioNotResumed => Layer::AudioHelper,
            Self::ScreenRecordingRefused | Self::VideoHelperDown | Self::VideoStreamLost => {
                Layer::Video
            }
        }
    }

    /// Whether the problem gets a notification once it lasts 30 s.
    // A tab that went away usually means the call ended, and mc-meet ends it in 5 minutes if no
    // tab comes back. A disconnect with no mc-meet port left is a browser that quit; with one
    // left, the extension lost the call while the browser runs.
    #[must_use]
    pub fn deserves_notice(self, is_extension_connected: bool) -> bool {
        match self {
            Self::MeetPageAway => false,
            Self::ExtensionDisconnected => is_extension_connected,
            Self::MeetCaptionsOff
            | Self::MeetCaptionsUnavailable
            | Self::CaptionParseFailures
            | Self::CaptionsSilent
            | Self::NoCaptions
            | Self::HuddleCaptionsOff
            | Self::HuddleCaptionsUnreadable
            | Self::MicPermissionRefused
            | Self::MicLost
            | Self::MicDigitalSilence
            | Self::AudioCapturePermissionRefused
            | Self::RemoteLost
            | Self::RemoteDigitalSilence
            | Self::AudioHelperDown
            | Self::AudioNotResumed
            | Self::ScreenRecordingRefused
            | Self::VideoHelperDown
            | Self::VideoStreamLost => true,
        }
    }
}
