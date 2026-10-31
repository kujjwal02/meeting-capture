// Fragmented MP4 writer for one video track: an init segment (ftyp + moov) per part and one
// moof + mdat pair per fragment. The app writes the bytes as they come, so the layout is fixed:
// track 1, 90 kHz media time, one sample entry whose config box is the encoder's description.
// No DOM, so plain Node can load it.

const TRACK_ID = 1;
const MEDIA_TIMESCALE = 90000;
const MOVIE_TIMESCALE = 1000;
const KEY_SAMPLE_FLAGS = 0x02000000;
const DELTA_SAMPLE_FLAGS = 0x01010000;
const UNITY_MATRIX = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];

function u8(...values) {
  return Uint8Array.from(values);
}

function u16(value) {
  const bytes = new Uint8Array(2);
  new DataView(bytes.buffer).setUint16(0, value);
  return bytes;
}

function u32(value) {
  const bytes = new Uint8Array(4);
  new DataView(bytes.buffer).setUint32(0, value);
  return bytes;
}

function u64(value) {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setBigUint64(0, BigInt(value));
  return bytes;
}

function zeros(length) {
  return new Uint8Array(length);
}

function ascii(text) {
  return Uint8Array.from(text, (character) => character.charCodeAt(0));
}

function concat(parts) {
  const length = parts.reduce((sum, part) => sum + part.length, 0);
  const joined = new Uint8Array(length);
  let offset = 0;
  for (const part of parts) {
    joined.set(part, offset);
    offset += part.length;
  }
  return joined;
}

function box(type, ...payloads) {
  const body = concat(payloads);
  return concat([u32(8 + body.length), ascii(type), body]);
}

function fullBox(type, version, flags, ...payloads) {
  return box(
    type,
    u8(version, (flags >> 16) & 0xff, (flags >> 8) & 0xff, flags & 0xff),
    ...payloads,
  );
}

function matrix() {
  return concat(UNITY_MATRIX.map(u32));
}

function sampleEntryTypes(codec) {
  switch (codec) {
    case "hevc":
      return { entryType: "hvc1", configType: "hvcC" };
    case "h264":
      return { entryType: "avc1", configType: "avcC" };
  }
  throw new Error(`fmp4: codec must be "hevc" or "h264", got ${JSON.stringify(codec)}`);
}

function visualSampleEntry(codec, width, height, description) {
  const { entryType, configType } = sampleEntryTypes(codec);
  return box(
    entryType,
    zeros(6),
    u16(1),
    zeros(16),
    u16(width),
    u16(height),
    u32(0x00480000),
    u32(0x00480000),
    u32(0),
    u16(1),
    zeros(32),
    u16(0x0018),
    u16(0xffff),
    box(configType, description),
  );
}

// description: the encoder's decoderConfig.description bytes (hvcC or avcC payload).
export function initSegment({ codec, width, height, description }) {
  const ftyp = box("ftyp", ascii("iso6"), u32(0), ascii("iso6"), ascii("iso5"), ascii("mp41"));
  const mvhd = fullBox(
    "mvhd",
    0,
    0,
    u32(0),
    u32(0),
    u32(MOVIE_TIMESCALE),
    u32(0),
    u32(0x00010000),
    u16(0x0100),
    zeros(2),
    zeros(8),
    matrix(),
    zeros(24),
    u32(TRACK_ID + 1),
  );
  const tkhd = fullBox(
    "tkhd",
    0,
    3,
    u32(0),
    u32(0),
    u32(TRACK_ID),
    u32(0),
    u32(0),
    zeros(8),
    u16(0),
    u16(0),
    u16(0),
    zeros(2),
    matrix(),
    u32(width * 0x10000),
    u32(height * 0x10000),
  );
  const mdhd = fullBox(
    "mdhd",
    0,
    0,
    u32(0),
    u32(0),
    u32(MEDIA_TIMESCALE),
    u32(0),
    u16(0x55c4),
    u16(0),
  );
  const hdlr = fullBox("hdlr", 0, 0, u32(0), ascii("vide"), zeros(12), ascii("VideoHandler\0"));
  const vmhd = fullBox("vmhd", 0, 1, u16(0), zeros(6));
  const dinf = box("dinf", fullBox("dref", 0, 0, u32(1), fullBox("url ", 0, 1)));
  const stbl = box(
    "stbl",
    fullBox(
      "stsd",
      0,
      0,
      u32(1),
      visualSampleEntry(codec, width, height, new Uint8Array(description)),
    ),
    fullBox("stts", 0, 0, u32(0)),
    fullBox("stsc", 0, 0, u32(0)),
    fullBox("stsz", 0, 0, u32(0), u32(0)),
    fullBox("stco", 0, 0, u32(0)),
  );
  const trak = box("trak", tkhd, box("mdia", mdhd, hdlr, box("minf", vmhd, dinf, stbl)));
  const mvex = box("mvex", fullBox("trex", 0, 0, u32(TRACK_ID), u32(1), u32(0), u32(0), u32(0)));
  return concat([ftyp, box("moov", mvhd, trak, mvex)]);
}

function movieFragment(sequenceNumber, baseDecodeTicks, samples, dataOffset) {
  const trunEntries = samples.map((sample) =>
    concat([
      u32(sample.durationTicks),
      u32(sample.bytes.length),
      u32(sample.isKey ? KEY_SAMPLE_FLAGS : DELTA_SAMPLE_FLAGS),
    ]),
  );
  return box(
    "moof",
    fullBox("mfhd", 0, 0, u32(sequenceNumber)),
    box(
      "traf",
      fullBox("tfhd", 0, 0x020000, u32(TRACK_ID)),
      fullBox("tfdt", 1, 0, u64(baseDecodeTicks)),
      fullBox("trun", 0, 0x000701, u32(samples.length), u32(dataOffset), ...trunEntries),
    ),
  );
}

// samples: [{ bytes: Uint8Array, durationTicks, isKey }], durations in 90 kHz ticks.
export function fragment({ sequenceNumber, baseDecodeTicks, samples }) {
  // The data offset does not change the moof's size, so one sizing pass finds it.
  const moofLength = movieFragment(sequenceNumber, baseDecodeTicks, samples, 0).length;
  const moof = movieFragment(sequenceNumber, baseDecodeTicks, samples, moofLength + 8);
  const mdat = box("mdat", ...samples.map((sample) => sample.bytes));
  return concat([moof, mdat]);
}
