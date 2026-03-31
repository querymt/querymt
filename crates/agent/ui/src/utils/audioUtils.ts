/**
 * Audio utilities for voice input/output.
 *
 * Browsers record audio in WebM/Opus (or MP4/AAC on Safari), but izwi's
 * ASR models expect WAV/PCM. This module converts recorded audio to 16kHz
 * mono 16-bit WAV using the Web Audio API.
 */

const TARGET_SAMPLE_RATE = 16000;

/**
 * Convert an audio Blob (any format the browser can decode) to a 16kHz
 * mono 16-bit PCM WAV ArrayBuffer.
 *
 * Uses `AudioContext.decodeAudioData()` to decode the source format,
 * resamples to 16 kHz mono via `OfflineAudioContext`, and encodes
 * the result as a standard RIFF/WAVE file.
 */
export async function convertToWav(blob: Blob): Promise<ArrayBuffer> {
  const arrayBuffer = await blob.arrayBuffer();

  // Decode the browser's native format (WebM/Opus, MP4/AAC, etc.)
  const audioCtx = new AudioContext();
  let audioBuffer: AudioBuffer;
  try {
    audioBuffer = await audioCtx.decodeAudioData(arrayBuffer);
  } finally {
    await audioCtx.close();
  }

  // Resample to target sample rate, mono
  const numFrames = Math.ceil(audioBuffer.duration * TARGET_SAMPLE_RATE);
  const offlineCtx = new OfflineAudioContext(1, numFrames, TARGET_SAMPLE_RATE);
  const source = offlineCtx.createBufferSource();
  source.buffer = audioBuffer;
  source.connect(offlineCtx.destination);
  source.start(0);
  const resampled = await offlineCtx.startRendering();

  // Get mono PCM samples (Float32 in [-1, 1])
  const samples = resampled.getChannelData(0);

  // Encode as WAV
  return encodeWav(samples, TARGET_SAMPLE_RATE);
}

/**
 * Encode Float32 PCM samples as a 16-bit mono WAV file.
 */
function encodeWav(samples: Float32Array, sampleRate: number): ArrayBuffer {
  const numChannels = 1;
  const bitsPerSample = 16;
  const bytesPerSample = bitsPerSample / 8;
  const dataLength = samples.length * bytesPerSample;
  const headerLength = 44;
  const totalLength = headerLength + dataLength;

  const buffer = new ArrayBuffer(totalLength);
  const view = new DataView(buffer);

  // RIFF header
  writeString(view, 0, 'RIFF');
  view.setUint32(4, totalLength - 8, true);
  writeString(view, 8, 'WAVE');

  // fmt sub-chunk
  writeString(view, 12, 'fmt ');
  view.setUint32(16, 16, true);                              // sub-chunk size
  view.setUint16(20, 1, true);                               // PCM format
  view.setUint16(22, numChannels, true);                     // channels
  view.setUint32(24, sampleRate, true);                      // sample rate
  view.setUint32(28, sampleRate * numChannels * bytesPerSample, true); // byte rate
  view.setUint16(32, numChannels * bytesPerSample, true);    // block align
  view.setUint16(34, bitsPerSample, true);                   // bits per sample

  // data sub-chunk
  writeString(view, 36, 'data');
  view.setUint32(40, dataLength, true);

  // Write PCM samples (clamp Float32 → Int16)
  let offset = headerLength;
  for (let i = 0; i < samples.length; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    const int16 = s < 0 ? s * 0x8000 : s * 0x7FFF;
    view.setInt16(offset, int16, true);
    offset += 2;
  }

  return buffer;
}

function writeString(view: DataView, offset: number, str: string) {
  for (let i = 0; i < str.length; i++) {
    view.setUint8(offset + i, str.charCodeAt(i));
  }
}
