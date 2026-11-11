//! Hand-rolled protobuf for `flow_api.v1.TranscriptionService`. Field numbers and enum values
//! were recovered from the Wispr Flow desktop app (`app.asar`, v1.6.1376), so no `.proto` or
//! `protoc` is needed. Only the fields the transcriber requires are encoded; unknown response
//! fields are skipped.

/// WAV, the encoding this engine sends. Opus is value 2.
const AUDIO_ENCODING_WAV: u64 = 1;
const ENVIRONMENT_PRODUCTION: u64 = 1;
const COMMIT_FINAL: u64 = 1;
const COMMIT_CONTINUE: u64 = 2;
/// `Client.platform` for the macOS build.
const PLATFORM_MAC: u64 = 2;

const APP_VERSION: (u64, u64, u64) = (1, 6, 1376);

/// The `flow_api.v1.Language` enum values this engine can ask for.
fn language_value(code: &str) -> Option<u64> {
    match code {
        "en" => Some(1),
        "engb" => Some(2),
        "hi" => Some(20),
        "hien" => Some(21),
        _ => None,
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "each byte written is the low 7 or 8 bits of the remaining value"
)]
fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while value > 0x7F {
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

fn tag(field_number: u64, wire_type: u64) -> Vec<u8> {
    varint((field_number << 3) | wire_type)
}

fn len_field(field_number: u64, body: &[u8]) -> Vec<u8> {
    let mut out = tag(field_number, 2);
    out.extend(varint(body.len() as u64));
    out.extend_from_slice(body);
    out
}

fn string_field(field_number: u64, text: &str) -> Vec<u8> {
    len_field(field_number, text.as_bytes())
}

fn varint_field(field_number: u64, value: u64) -> Vec<u8> {
    let mut out = tag(field_number, 0);
    out.extend(varint(value));
    out
}

fn version() -> Vec<u8> {
    let (major, minor, patch) = APP_VERSION;
    let mut out = varint_field(1, major);
    out.extend(varint_field(2, minor));
    out.extend(varint_field(3, patch));
    out
}

fn client() -> Vec<u8> {
    // Client{ name(1), platform(2)=MAC, version(3) }
    let mut out = string_field(1, "Wispr Flow");
    out.extend(varint_field(2, PLATFORM_MAC));
    out.extend(len_field(3, &version()));
    out
}

fn metadata(user_id: &str, session_id: &str, request_id: &str) -> Vec<u8> {
    let mut out = string_field(1, user_id);
    out.extend(string_field(2, session_id));
    out.extend(string_field(3, request_id));
    out.extend(varint_field(4, AUDIO_ENCODING_WAV));
    out.extend(varint_field(5, ENVIRONMENT_PRODUCTION));
    out.extend(len_field(6, &client()));
    out
}

fn preferences(language_codes: &[String]) -> Vec<u8> {
    // Preferences{ user(1)=empty User{}, language(2)=packed repeated enum }
    let mut out = len_field(1, &[]);
    let mut packed = Vec::new();
    for code in language_codes {
        if let Some(value) = language_value(code) {
            packed.extend(varint(value));
        }
    }
    if !packed.is_empty() {
        out.extend(len_field(2, &packed));
    }
    out
}

/// `Request{ init = Init{ metadata(1), preferences(2) }, commit = CONTINUE }`, the first message.
pub(super) fn init_request(
    user_id: &str,
    session_id: &str,
    request_id: &str,
    language_codes: &[String],
) -> Vec<u8> {
    let mut init = len_field(1, &metadata(user_id, session_id, request_id));
    init.extend(len_field(2, &preferences(language_codes)));
    let mut out = len_field(1, &init);
    out.extend(varint_field(4, COMMIT_CONTINUE));
    out
}

/// `Request{ payload = Payload{ audio_file = AudioFile{ data(1) } }, commit = FINAL }`, the audio.
pub(super) fn audio_file_request(audio_bytes: &[u8]) -> Vec<u8> {
    let audio_file = len_field(1, audio_bytes); // AudioFile.data(1)
    let payload = len_field(2, &audio_file); // Payload.audio_file(2)
    let mut out = len_field(3, &payload); // Request.payload(3)
    out.extend(varint_field(4, COMMIT_FINAL)); // Request.commit(4)
    out
}

/// The text fields found in a response frame, newest wins. Any non-empty field marks a usable
/// reply.
#[derive(Default)]
pub(super) struct ResponseText {
    pub(super) result_plaintext: String,
    pub(super) result_html: String,
    pub(super) raw_text: String,
    pub(super) formatted_text: String,
}

impl ResponseText {
    /// The best plaintext across the fields, preferring the finished result.
    pub(super) fn best_plaintext(&self) -> &str {
        for text in [
            &self.result_plaintext,
            &self.result_html,
            &self.formatted_text,
            &self.raw_text,
        ] {
            if !text.is_empty() {
                return text;
            }
        }
        ""
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut shift = 0_u32;
        let mut value = 0_u64;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    /// The next length-delimited field as `(field_number, bytes)`. Varint and fixed-width fields
    /// are consumed and skipped: this parser reads only the message's nested fields.
    fn next_bytes_field(&mut self) -> Option<(u64, &'a [u8])> {
        while self.pos < self.buf.len() {
            let tag = self.read_varint()?;
            let field_number = tag >> 3;
            match tag & 7 {
                0 => {
                    self.read_varint()?;
                }
                2 => {
                    let length = usize::try_from(self.read_varint()?).ok()?;
                    let end = self.pos.checked_add(length)?;
                    let slice = self.buf.get(self.pos..end)?;
                    self.pos = end;
                    return Some((field_number, slice));
                }
                5 => self.pos = self.pos.checked_add(4)?,
                1 => self.pos = self.pos.checked_add(8)?,
                _ => return None,
            }
        }
        None
    }
}

/// `Transcription{ html(1), plaintext(2) }`.
fn transcription_text(buf: &[u8]) -> (String, String) {
    let mut html = String::new();
    let mut plaintext = String::new();
    let mut reader = Reader::new(buf);
    while let Some((field_number, bytes)) = reader.next_bytes_field() {
        let text = String::from_utf8_lossy(bytes).into_owned();
        match field_number {
            1 => html = text,
            2 => plaintext = text,
            _ => {}
        }
    }
    (html, plaintext)
}

/// The transcription text of one `Response` frame.
///
/// `Response{ result(1)=Result, state(2)=State }`; `Result.output(1)` and
/// `State.raw_text(2)`/`formatted_text(3)` each carry a `Transcription`.
pub(super) fn parse_response(buf: &[u8], into: &mut ResponseText) {
    let mut reader = Reader::new(buf);
    while let Some((field_number, bytes)) = reader.next_bytes_field() {
        match field_number {
            1 => {
                // Result{ output(1) = Transcription }
                let mut inner = Reader::new(bytes);
                while let Some((inner_number, output)) = inner.next_bytes_field() {
                    if inner_number == 1 {
                        let (html, plaintext) = transcription_text(output);
                        if !html.is_empty() {
                            into.result_html = html;
                        }
                        if !plaintext.is_empty() {
                            into.result_plaintext = plaintext;
                        }
                    }
                }
            }
            2 => {
                // State{ raw_text(2), formatted_text(3) }
                let mut inner = Reader::new(bytes);
                while let Some((inner_number, state)) = inner.next_bytes_field() {
                    let (_, plaintext) = transcription_text(state);
                    if plaintext.is_empty() {
                        continue;
                    }
                    match inner_number {
                        2 => into.raw_text = plaintext,
                        3 => into.formatted_text = plaintext,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}
