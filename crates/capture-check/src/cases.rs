//! The faults the check runs, each with its timetable and what the detectors must say.

use std::time::Duration;

/// How long each case runs before its `stop`.
pub(crate) const CASE_LENGTH: Duration = Duration::from_secs(40);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Helper {
    Audio,
    Video,
}

impl Helper {
    /// The `mc-capture` subcommand, which is also `mc.capture.helper`.
    pub(crate) fn subcommand(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

/// What the detectors must say about a case. Times are seconds since the helper started.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Expect {
    NoProblem,
    Problem {
        code: &'static str,
        raised_from_s: u64,
        raised_by_s: u64,
        /// The problem is still raised at this time.
        still_raised_at_s: Option<u64>,
        cleared_by_s: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Case {
    pub name: &'static str,
    pub helper: Helper,
    /// The `MC_CAPTURE_INJECT` value.
    pub timetable: &'static str,
    /// The injected event the helper's dump must show.
    pub trigger: &'static str,
    /// The driver sends no `start` at a wake, so audio stays down.
    pub is_wake_start_withheld: bool,
    pub expect: Expect,
}

const fn problem(code: &'static str, raised_s: (u64, u64), cleared_by_s: Option<u64>) -> Expect {
    Expect::Problem {
        code,
        raised_from_s: raised_s.0,
        raised_by_s: raised_s.1,
        still_raised_at_s: None,
        cleared_by_s,
    }
}

const fn audio(
    name: &'static str,
    timetable: &'static str,
    trigger: &'static str,
    expect: Expect,
) -> Case {
    Case {
        name,
        helper: Helper::Audio,
        timetable,
        trigger,
        is_wake_start_withheld: false,
        expect,
    }
}

const fn video(
    name: &'static str,
    timetable: &'static str,
    trigger: &'static str,
    expect: Expect,
) -> Case {
    Case {
        name,
        helper: Helper::Video,
        timetable,
        trigger,
        is_wake_start_withheld: false,
        expect,
    }
}

pub(crate) const CASES: [Case; 13] = [
    audio(
        "mic-lost",
        "8:mic-lost",
        "mic-lost",
        problem("mic_lost", (8, 11), Some(15)),
    ),
    audio(
        "mic-unavailable",
        "8:candidate-no-buffers,9:mic-lost",
        "mic-lost",
        Expect::Problem {
            code: "mic_lost",
            raised_from_s: 9,
            raised_by_s: 12,
            still_raised_at_s: Some(15),
            cleared_by_s: Some(35),
        },
    ),
    audio(
        "mic-zeros",
        "8:mic-zeros,30:mic-sound",
        "mic-zeros",
        problem("mic_digital_silence", (18, 21), Some(32)),
    ),
    audio(
        "tap-silent-open",
        "0:tap-silent-open",
        "tap-silent-open",
        problem("remote_lost", (5, 9), Some(12)),
    ),
    audio(
        "tap-stall",
        "8:tap-stall",
        "tap-stall",
        problem("remote_lost", (8, 11), Some(15)),
    ),
    audio(
        "tap-stall-quiet",
        "8:tap-stall-quiet",
        "tap-stall-quiet",
        problem("remote_lost", (13, 16), Some(20)),
    ),
    audio(
        "tap-dead",
        "8:tap-dead",
        "tap-dead",
        problem("remote_lost", (8, 11), Some(15)),
    ),
    audio(
        "routine",
        "5:default-input,6:default-output,7:device-list,8:hal-probe,9:tap-format,10:lid-closed,11:lid-open",
        "default-input",
        Expect::NoProblem,
    ),
    audio(
        "sleep-wake",
        "8:sleep,12:wake,13:dark-wake",
        "sleep",
        Expect::NoProblem,
    ),
    Case {
        is_wake_start_withheld: true,
        ..audio(
            "wake-no-start",
            "8:sleep,12:wake",
            "wake",
            problem("audio_not_resumed", (32, 34), None),
        )
    },
    video(
        "video-stall",
        "8:video-stall,25:video-resume",
        "video-stall",
        problem("video_stream_lost", (13, 18), Some(35)),
    ),
    video(
        "video-still",
        "8:video-still,30:video-resume",
        "video-still",
        Expect::NoProblem,
    ),
    video(
        "screen-recording-refused",
        "0:screen-recording-refused,15:screen-recording-restored",
        "screen-recording-refused",
        problem("screen_recording_refused", (0, 3), Some(30)),
    ),
];

/// The cases named on the command line, in table order, or every case when none is named.
pub(crate) fn chosen(names: &[String]) -> anyhow::Result<Vec<Case>> {
    if let Some(unknown) = names
        .iter()
        .find(|name| !CASES.iter().any(|case| case.name == name.as_str()))
    {
        let valid: Vec<&str> = CASES.iter().map(|case| case.name).collect();
        anyhow::bail!("unknown case {unknown}; valid cases: {}", valid.join(", "));
    }
    Ok(CASES
        .into_iter()
        .filter(|case| names.is_empty() || names.iter().any(|name| name == case.name))
        .collect())
}

/// The timetable for a helper that starts `elapsed_s` into the case: later events move earlier
/// by that much, and past ones drop. The last past Screen Recording state stays, at 0, since it
/// holds until the next one.
pub(crate) fn timetable_from(timetable: &str, elapsed_s: u64) -> String {
    let mut carried_state = None;
    let mut later = Vec::new();
    for entry in timetable.split(',') {
        let Some((at, event)) = entry.split_once(':') else {
            continue;
        };
        let Ok(at_s) = at.parse::<u64>() else {
            continue;
        };
        if at_s >= elapsed_s {
            later.push(format!("{}:{event}", at_s - elapsed_s));
        } else if event.starts_with("screen-recording-") {
            carried_state = Some(format!("0:{event}"));
        }
    }
    carried_state
        .into_iter()
        .chain(later)
        .collect::<Vec<_>>()
        .join(",")
}
