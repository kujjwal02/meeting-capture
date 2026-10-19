// run.mjs injects this into the host's Meet tab before any page script runs, with the speech
// filled in.
// Replaces the microphone with a looping speech track and the camera with a canvas, so Meet
// produces captions in an unattended test call.
(() => {
  const speechBase64 = "__SPEECH_BASE64__";
  let speechTrack;
  async function speech() {
    if (speechTrack && speechTrack.readyState === "live") return speechTrack;
    const context = new AudioContext();
    const bytes = Uint8Array.from(atob(speechBase64), (c) => c.charCodeAt(0));
    const buffer = await context.decodeAudioData(bytes.buffer);
    const source = context.createBufferSource();
    source.buffer = buffer;
    source.loop = true;
    const destination = context.createMediaStreamDestination();
    source.connect(destination);
    source.start();
    window.__fakeMicContext = context;
    context.resume();
    speechTrack = destination.stream.getAudioTracks()[0];
    return speechTrack;
  }
  function camera() {
    const canvas = document.createElement("canvas");
    canvas.width = 640;
    canvas.height = 360;
    const draw = canvas.getContext("2d");
    setInterval(() => {
      draw.fillStyle = "#246";
      draw.fillRect(0, 0, 640, 360);
      draw.fillStyle = "#fff";
      draw.font = "32px sans-serif";
      draw.fillText(`mc-meet test ${new Date().toISOString().slice(11, 19)}`, 40, 180);
    }, 500);
    return canvas.captureStream(5).getVideoTracks()[0];
  }
  const devices = navigator.mediaDevices;
  devices.getUserMedia = async (constraints) => {
    const tracks = [];
    if (constraints?.audio) tracks.push(await speech());
    if (constraints?.video) tracks.push(camera());
    return new MediaStream(tracks);
  };
  devices.enumerateDevices = async () => [
    {
      deviceId: "fake-mic",
      kind: "audioinput",
      label: "Test speech",
      groupId: "fake",
      toJSON() {
        return this;
      },
    },
    {
      deviceId: "fake-cam",
      kind: "videoinput",
      label: "Test camera",
      groupId: "fake",
      toJSON() {
        return this;
      },
    },
    {
      deviceId: "default",
      kind: "audiooutput",
      label: "Default",
      groupId: "fake",
      toJSON() {
        return this;
      },
    },
  ];
  const query = navigator.permissions.query.bind(navigator.permissions);
  navigator.permissions.query = (descriptor) =>
    descriptor && (descriptor.name === "microphone" || descriptor.name === "camera")
      ? Promise.resolve({
          state: "granted",
          status: "granted",
          onchange: null,
          addEventListener() {},
          removeEventListener() {},
        })
      : query(descriptor);
})();
