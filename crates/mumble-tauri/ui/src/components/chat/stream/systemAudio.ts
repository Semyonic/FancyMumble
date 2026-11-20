/**
 * Native system-audio fallback for screen sharing on macOS.
 *
 * WebKit's `getDisplayMedia` never returns an audio track, so on macOS the
 * Rust side captures the Mac's output with a Core Audio process tap
 * (`start_system_audio_capture`) and streams raw PCM over a Tauri channel.
 * Here it is played through an AudioWorklet into a MediaStream destination,
 * whose audio track is added to the screen-share stream.
 */
import { Channel, invoke } from "@tauri-apps/api/core";
import workletUrl from "./systemAudioWorklet.js?url";

interface CaptureFormat {
  readonly sampleRate: number;
  readonly channels: number;
}

/** A running native capture; `stop()` releases everything (idempotent). */
export interface SystemAudioCapture {
  readonly track: MediaStreamTrack;
  stop(): void;
}

/** Blocks buffered while the worklet loads (~2 s at 40 ms per block). */
const MAX_PENDING_BLOCKS = 50;
/** Upper bound on buffered audio inside the worklet. */
const MAX_BUFFER_SECONDS = 0.5;

/**
 * Creates the AudioContext for the native fallback, or `null` where it does
 * not apply.  Call it synchronously from the user's click, before any
 * `await`, so the context is allowed to start.
 */
export function prepareSystemAudioContext(): AudioContext | null {
  const isMac = navigator.userAgent.includes("Macintosh");
  return isMac && typeof AudioContext !== "undefined" ? new AudioContext() : null;
}

/** Starts native capture and returns it as a MediaStreamTrack. */
export async function startSystemAudio(ctx: AudioContext): Promise<SystemAudioCapture> {
  const pending: ArrayBuffer[] = [];
  let node: AudioWorkletNode | null = null;
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (block) => {
    if (node) node.port.postMessage(block, [block]);
    else if (pending.length < MAX_PENDING_BLOCKS) pending.push(block);
  };

  const format = await invoke<CaptureFormat>("start_system_audio_capture", { onAudio: channel });
  try {
    await ctx.audioWorklet.addModule(workletUrl);
    node = new AudioWorkletNode(ctx, "fm-system-audio", {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [Math.min(format.channels, 2)],
      processorOptions: {
        channels: format.channels,
        sourceRate: format.sampleRate,
        maxSeconds: MAX_BUFFER_SECONDS,
      },
    });
    for (const block of pending.splice(0)) node.port.postMessage(block, [block]);

    const destination = ctx.createMediaStreamDestination();
    node.connect(destination);
    if (ctx.state !== "running") await ctx.resume();
    const track = destination.stream.getAudioTracks()[0];
    if (!track) throw new Error("MediaStream destination produced no audio track");

    const workletNode = node;
    let stopped = false;
    return {
      track,
      stop() {
        if (stopped) return;
        stopped = true;
        channel.onmessage = () => {};
        invoke("stop_system_audio_capture").catch((e) =>
          console.warn("[screenshare] stop_system_audio_capture failed", e),
        );
        workletNode.disconnect();
        track.stop();
        ctx.close().catch(() => {});
      },
    };
  } catch (e) {
    channel.onmessage = () => {};
    invoke("stop_system_audio_capture").catch(() => {});
    throw e;
  }
}
