/**
 * AudioWorklet processor that plays interleaved f32 PCM blocks posted by
 * the main thread (native system audio, see systemAudio.ts).
 *
 * - Ring buffer bounded to `maxSeconds`; on overrun the oldest audio is
 *   skipped so latency stays bounded.
 * - Waits for a small cushion before (re)starting to absorb IPC jitter.
 * - Linear resampling when the capture rate differs from the context rate.
 */
class SystemAudioProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { channels, sourceRate, maxSeconds } = options.processorOptions;
    this.channels = channels;
    // `sampleRate` is the AudioWorkletGlobalScope's context rate.
    this.step = sourceRate / sampleRate;
    this.capacity = Math.ceil(sourceRate * maxSeconds);
    this.cushion = Math.ceil(sourceRate * 0.08);
    this.ring = new Float32Array(this.capacity * channels);
    this.written = 0; // frames written (monotonic)
    this.readPos = 0; // fractional frames read (monotonic)
    this.playing = false;
    this.port.onmessage = (e) => this.push(new Float32Array(e.data));
  }

  push(samples) {
    const ch = this.channels;
    const cap = this.capacity;
    const frames = Math.floor(samples.length / ch);
    for (let f = 0; f < frames; f++) {
      const base = ((this.written + f) % cap) * ch;
      for (let c = 0; c < ch; c++) this.ring[base + c] = samples[f * ch + c];
    }
    this.written += frames;
    if (this.written - this.readPos > cap - 1) {
      this.readPos = this.written - this.cushion;
    }
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const ch = this.channels;
    const cap = this.capacity;
    if (!this.playing) {
      if (this.written - this.readPos < this.cushion) return true;
      this.playing = true;
    }
    for (let i = 0; i < out[0].length; i++) {
      const i0 = Math.floor(this.readPos);
      if (i0 + 1 >= this.written) {
        // Underrun: the rest of this quantum stays silent.
        this.playing = false;
        break;
      }
      const frac = this.readPos - i0;
      const a = (i0 % cap) * ch;
      const b = ((i0 + 1) % cap) * ch;
      for (let c = 0; c < out.length; c++) {
        const src = Math.min(c, ch - 1);
        const s0 = this.ring[a + src];
        out[c][i] = s0 + (this.ring[b + src] - s0) * frac;
      }
      this.readPos += this.step;
    }
    return true;
  }
}

registerProcessor("fm-system-audio", SystemAudioProcessor);
