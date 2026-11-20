//! System-audio capture through a Core Audio process tap (macOS 14.2+).
//!
//! Pipeline:
//!
//! 1. A private, global stereo `CATapDescription` that excludes this
//!    process, so the Mumble voices we play back are not re-broadcast.
//! 2. A private aggregate device on the default output device whose only
//!    input is that tap.
//! 3. A C `IOProc` on the aggregate device copies each block into an
//!    interleaved `Vec<f32>` and hands it to a bounded queue (never
//!    blocks the real-time thread; blocks are dropped when it is full).
//! 4. A forwarder thread batches ~40 ms of audio and sends it to the
//!    webview as raw bytes over a Tauri IPC channel.
//!
//! `AudioHardwareCreateProcessTap` / `AudioHardwareDestroyProcessTap` are
//! resolved with `dlsym` at runtime: linking them directly would stop the
//! app from launching on macOS older than 14.2.
//!
//! Everything is torn down in [`Drop`], including after a partial start.

#![allow(
    unsafe_code,
    reason = "Core Audio and Objective-C interop are FFI; \
              every unsafe block has a SAFETY comment justifying it."
)]

use std::ffi::{c_char, c_void, CStr};
use std::mem::{size_of, MaybeUninit};
use std::ptr::{self, NonNull};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::JoinHandle;

use objc2_06::rc::Retained;
use objc2_06::runtime::AnyClass;
use objc2_06::AllocAnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceUID,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
    kAudioTapPropertyFormat, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareDestroyAggregateDevice, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector, CATapDescription,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM, AudioBuffer,
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation_03::{NSArray, NSDictionary, NSNumber, NSObject, NSString};
use tauri::ipc::{Channel, InvokeResponseBody};
use tracing::{debug, info, warn};

use super::CaptureFormat;

/// Seconds of audio per IPC message (~25 messages/s).
const BLOCK_SECONDS: f64 = 0.04;
/// `IOProc` blocks (~10 ms each) the queue holds before dropping new ones.
const QUEUE_BLOCKS: usize = 64;
/// `dlsym` pseudo-handle that searches every loaded image.
const RTLD_DEFAULT: *mut c_void = -2_isize as *mut c_void;

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

type CreateTapFn = unsafe extern "C" fn(*const CATapDescription, *mut AudioObjectID) -> i32;
type DestroyTapFn = unsafe extern "C" fn(AudioObjectID) -> i32;

/// State shared with the real-time `IOProc`.
#[derive(Debug)]
struct ProcContext {
    tx: SyncSender<Vec<f32>>,
    non_interleaved: bool,
}

/// A running system-audio capture.  Dropping it stops the capture and
/// releases the tap, the aggregate device and the forwarder thread.
#[derive(Debug)]
pub(crate) struct SystemAudioCapture {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    /// Freed only after the `IOProc` is destroyed (the `IOProc` borrows it).
    context: Option<Box<ProcContext>>,
    worker: Option<JoinHandle<()>>,
    destroy_tap: DestroyTapFn,
    format: CaptureFormat,
}

impl SystemAudioCapture {
    /// Starts capturing everything the Mac plays except this process and
    /// streams it to `on_audio` as interleaved little-endian `f32` blocks.
    pub(crate) fn start(on_audio: Channel<InvokeResponseBody>) -> Result<Self, String> {
        // SAFETY: the symbol names match the `CoreAudio` declarations
        // `OSStatus AudioHardwareCreateProcessTap(CATapDescription*, AudioObjectID*)`
        // and `OSStatus AudioHardwareDestroyProcessTap(AudioObjectID)`.
        let (create_tap, destroy_tap) = unsafe {
            (
                lookup::<CreateTapFn>(c"AudioHardwareCreateProcessTap"),
                lookup::<DestroyTapFn>(c"AudioHardwareDestroyProcessTap"),
            )
        };
        let (Some(create_tap), Some(destroy_tap)) = (create_tap, destroy_tap) else {
            return Err("system audio capture requires macOS 14.2 or newer".into());
        };
        if AnyClass::get(c"CATapDescription").is_none() {
            return Err("system audio capture requires macOS 14.2 or newer".into());
        }

        let mut capture = Self {
            tap: 0,
            aggregate: 0,
            io_proc: None,
            context: None,
            worker: None,
            destroy_tap,
            format: CaptureFormat { sample_rate: 0.0, channels: 0 },
        };

        let description = tap_description();
        let mut tap = 0;
        // SAFETY: `description` is a valid CATapDescription and `tap` a
        // valid out-pointer.
        check(
            unsafe { create_tap(Retained::as_ptr(&description), &raw mut tap) },
            "AudioHardwareCreateProcessTap",
        )?;
        capture.tap = tap;

        // SAFETY: kAudioTapPropertyFormat holds an AudioStreamBasicDescription.
        let asbd: AudioStreamBasicDescription =
            unsafe { get_property(tap, kAudioTapPropertyFormat, ptr::null(), 0)? };
        let (format, non_interleaved) = parse_format(&asbd)?;
        capture.format = format;

        // SAFETY: `UUID` is a plain getter on a fully initialised description.
        let tap_uid = unsafe { description.UUID() }.UUIDString();
        let output_uid = default_output_device_uid()?;
        let aggregate_description = aggregate_description(&tap_uid, &output_uid);
        let mut aggregate = 0;
        // SAFETY: NSDictionary is toll-free bridged to CFDictionary, and
        // `aggregate` is a valid out-pointer.
        check(
            unsafe {
                let dict = &*Retained::as_ptr(&aggregate_description).cast::<CFDictionary>();
                AudioHardwareCreateAggregateDevice(dict, NonNull::from(&mut aggregate))
            },
            "AudioHardwareCreateAggregateDevice",
        )?;
        capture.aggregate = aggregate;

        let (tx, rx) = sync_channel(QUEUE_BLOCKS);
        let context = Box::new(ProcContext { tx, non_interleaved });
        let client_data: *const ProcContext = &raw const *context;
        capture.context = Some(context);
        let mut io_proc: AudioDeviceIOProcID = None;
        // SAFETY: `io_proc_callback` matches AudioDeviceIOProc, and
        // `client_data` points at the boxed context, which `Drop` frees
        // only after destroying the `IOProc`.
        check(
            unsafe {
                AudioDeviceCreateIOProcID(
                    aggregate,
                    Some(io_proc_callback),
                    client_data.cast_mut().cast(),
                    NonNull::from(&mut io_proc),
                )
            },
            "AudioDeviceCreateIOProcID",
        )?;
        capture.io_proc = io_proc;

        capture.worker = Some(spawn_forwarder(rx, on_audio, format)?);

        // SAFETY: device and `IOProc` were created above.
        check(unsafe { AudioDeviceStart(aggregate, io_proc) }, "AudioDeviceStart")?;

        info!(
            sample_rate = format.sample_rate,
            channels = format.channels,
            non_interleaved,
            "system audio capture started",
        );
        Ok(capture)
    }

    /// Format of the blocks sent to the frontend.
    pub(crate) fn format(&self) -> CaptureFormat {
        self.format
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        // SAFETY: every id is either 0/None (never created) or was created
        // by `start` and has not been destroyed yet.  AudioDeviceStop and
        // AudioDeviceDestroyIOProcID return only after the `IOProc` has
        // stopped running, so the context can be freed afterwards.
        unsafe {
            if self.aggregate != 0 {
                if self.io_proc.is_some() {
                    let _ = AudioDeviceStop(self.aggregate, self.io_proc);
                    let _ = AudioDeviceDestroyIOProcID(self.aggregate, self.io_proc);
                }
                let _ = AudioHardwareDestroyAggregateDevice(self.aggregate);
            }
            if self.tap != 0 {
                let _ = (self.destroy_tap)(self.tap);
            }
        }
        // Dropping the sender ends the forwarder loop.
        drop(self.context.take());
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                warn!("system audio forwarder thread panicked");
            }
        }
        debug!("system audio capture stopped");
    }
}

/// Global stereo tap of everything except this process.
fn tap_description() -> Retained<CATapDescription> {
    let excluded: Vec<Retained<NSNumber>> = match own_process_object() {
        Ok(id) if id != 0 => vec![NSNumber::numberWithUnsignedInt(id)],
        Ok(_) => {
            // The process has not opened an audio client yet, so it has
            // nothing to exclude (and no Mumble playback to leak).
            debug!("no Core Audio process object for this process yet");
            Vec::new()
        }
        Err(e) => {
            warn!("could not resolve own Core Audio process object: {e}");
            Vec::new()
        }
    };
    let excluded = NSArray::from_retained_slice(&excluded);
    // SAFETY: freshly allocated description initialised with an
    // NSArray<NSNumber> of process object ids, as the API requires.
    unsafe {
        let description = CATapDescription::initStereoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            &excluded,
        );
        description.setName(&NSString::from_str("Fancy Mumble screen share"));
        description.setPrivate(true);
        description
    }
}

/// Aggregate device whose only input is the tap identified by `tap_uid`.
fn aggregate_description(tap_uid: &NSString, output_uid: &NSString) -> Retained<NSDictionary<NSString, NSObject>> {
    let yes = NSNumber::numberWithBool(true);
    let no = NSNumber::numberWithBool(false);
    let aggregate_uid = NSString::from_str(&format!(
        "com.fancymumble.screenshare-tap.{}",
        std::process::id()
    ));
    let name = NSString::from_str("Fancy Mumble screen share");

    let sub_device = NSDictionary::<NSString, NSObject>::from_slices(
        &[&*key(kAudioSubDeviceUIDKey)],
        &[as_object(output_uid)],
    );
    let tap = NSDictionary::<NSString, NSObject>::from_slices(
        &[&*key(kAudioSubTapUIDKey), &*key(kAudioSubTapDriftCompensationKey)],
        &[as_object(tap_uid), as_object(&*yes)],
    );
    let sub_devices = NSArray::from_retained_slice(&[sub_device]);
    let taps = NSArray::from_retained_slice(&[tap]);

    NSDictionary::from_slices(
        &[
            &*key(kAudioAggregateDeviceUIDKey),
            &*key(kAudioAggregateDeviceNameKey),
            &*key(kAudioAggregateDeviceMainSubDeviceKey),
            &*key(kAudioAggregateDeviceIsPrivateKey),
            &*key(kAudioAggregateDeviceIsStackedKey),
            &*key(kAudioAggregateDeviceTapAutoStartKey),
            &*key(kAudioAggregateDeviceSubDeviceListKey),
            &*key(kAudioAggregateDeviceTapListKey),
        ],
        &[
            as_object(&*aggregate_uid),
            as_object(&*name),
            as_object(output_uid),
            as_object(&*yes),
            as_object(&*no),
            as_object(&*yes),
            as_object(&*sub_devices),
            as_object(&*taps),
        ],
    )
}

fn key(name: &CStr) -> Retained<NSString> {
    NSString::from_str(name.to_str().unwrap_or_default())
}

fn as_object<T: AsRef<NSObject> + ?Sized>(object: &T) -> &NSObject {
    object.as_ref()
}

/// Checks the tap format is 32-bit float PCM and returns it.
fn parse_format(asbd: &AudioStreamBasicDescription) -> Result<(CaptureFormat, bool), String> {
    let is_float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
    if asbd.mFormatID != kAudioFormatLinearPCM || !is_float || asbd.mBitsPerChannel != 32 {
        return Err(format!(
            "unsupported tap format: id={:#x} flags={:#x} bits={}",
            asbd.mFormatID, asbd.mFormatFlags, asbd.mBitsPerChannel
        ));
    }
    if asbd.mChannelsPerFrame == 0 || asbd.mSampleRate <= 0.0 {
        return Err("tap reported an empty format".into());
    }
    let format = CaptureFormat { sample_rate: asbd.mSampleRate, channels: asbd.mChannelsPerFrame };
    Ok((format, asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0))
}

/// Batches `IOProc` blocks into ~[`BLOCK_SECONDS`] messages for the webview.
fn spawn_forwarder(
    rx: Receiver<Vec<f32>>,
    on_audio: Channel<InvokeResponseBody>,
    format: CaptureFormat,
) -> Result<JoinHandle<()>, String> {
    let frames_per_block = (format.sample_rate * BLOCK_SECONDS).ceil().max(1.0) as usize;
    let samples_per_block = frames_per_block * format.channels as usize;
    std::thread::Builder::new()
        .name("system-audio-ipc".into())
        .spawn(move || {
            let mut pending: Vec<f32> = Vec::with_capacity(samples_per_block * 2);
            while let Ok(samples) = rx.recv() {
                pending.extend_from_slice(&samples);
                if pending.len() < samples_per_block {
                    continue;
                }
                let bytes: Vec<u8> = pending.iter().flat_map(|s| s.to_le_bytes()).collect();
                pending.clear();
                if let Err(e) = on_audio.send(InvokeResponseBody::Raw(bytes)) {
                    warn!("system audio: webview channel closed: {e}");
                    break;
                }
            }
        })
        .map_err(|e| format!("failed to spawn system audio thread: {e}"))
}

/// Real-time `IOProc`: copy the tap's input into an interleaved block.
unsafe extern "C-unwind" fn io_proc_callback(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    // SAFETY: `client_data` is the ProcContext registered in `start`,
    // which outlives the `IOProc` (see `Drop`).
    let context = unsafe { &*client_data.cast::<ProcContext>() };
    // SAFETY: Core Audio passes a valid buffer list whose `mBuffers`
    // array holds `mNumberBuffers` entries for the duration of the call.
    let buffers = unsafe {
        let list = input.as_ref();
        std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize)
    };
    // SAFETY: each buffer's `mData` holds `mDataByteSize` bytes of f32
    // samples (format checked in `parse_format`).
    if let Some(block) = unsafe { interleave(buffers, context.non_interleaved) } {
        // Full queue: drop the block rather than block the audio thread.
        let _ = context.tx.try_send(block);
    }
    0
}

/// Converts Core Audio buffers to one interleaved `f32` block.
///
/// # Safety
///
/// Every buffer's `mData` must be null or point to `mDataByteSize` bytes of
/// properly aligned `f32` samples.
unsafe fn interleave(buffers: &[AudioBuffer], non_interleaved: bool) -> Option<Vec<f32>> {
    // SAFETY: forwarded from the caller's contract.
    let samples = |buffer: &AudioBuffer| -> Option<&[f32]> {
        let data = NonNull::new(buffer.mData.cast::<f32>())?;
        let len = buffer.mDataByteSize as usize / size_of::<f32>();
        Some(unsafe { std::slice::from_raw_parts(data.as_ptr(), len) })
    };

    if !non_interleaved {
        return buffers.first().and_then(samples).map(<[f32]>::to_vec);
    }

    let channels: Vec<&[f32]> = buffers.iter().map(samples).collect::<Option<_>>()?;
    let frames = channels.iter().map(|c| c.len()).min()?;
    let mut out = Vec::with_capacity(frames * channels.len());
    for frame in 0..frames {
        out.extend(channels.iter().map(|c| c[frame]));
    }
    Some(out)
}

/// Core Audio process object for this process (0 if it has none yet).
fn own_process_object() -> Result<AudioObjectID, String> {
    let pid: i32 = std::process::id().cast_signed();
    // SAFETY: the property takes a pid_t qualifier and yields an AudioObjectID.
    unsafe {
        get_property(
            system_object(),
            kAudioHardwarePropertyTranslatePIDToProcessObject,
            (&raw const pid).cast(),
            size_of::<i32>() as u32,
        )
    }
}

/// UID of the current default output device.
fn default_output_device_uid() -> Result<Retained<NSString>, String> {
    // SAFETY: the default output device property yields an AudioObjectID.
    let device: AudioObjectID = unsafe {
        get_property(system_object(), kAudioHardwarePropertyDefaultOutputDevice, ptr::null(), 0)?
    };
    // SAFETY: kAudioDevicePropertyDeviceUID yields a +1 CFStringRef, which
    // is toll-free bridged to NSString and owned by the caller.
    unsafe {
        let uid: *mut NSString = get_property(device, kAudioDevicePropertyDeviceUID, ptr::null(), 0)?;
        Retained::from_raw(uid).ok_or_else(|| "default output device has no UID".to_string())
    }
}

fn system_object() -> AudioObjectID {
    kAudioObjectSystemObject.cast_unsigned()
}

/// Reads a fixed-size global property.
///
/// # Safety
///
/// `T` must be the type Core Audio stores for `selector` on `object`, and
/// `qualifier` / `qualifier_size` must describe the qualifier it expects.
unsafe fn get_property<T>(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    qualifier: *const c_void,
    qualifier_size: u32,
) -> Result<T, String> {
    let address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    // SAFETY: `value` has room for `size` bytes; the caller guarantees the
    // property's type and qualifier.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            qualifier_size,
            qualifier,
            NonNull::from(&mut size),
            NonNull::new_unchecked(value.as_mut_ptr().cast()),
        )
    };
    check(status, "AudioObjectGetPropertyData")?;
    if size as usize != size_of::<T>() {
        return Err(format!("property {selector:#x} has size {size}, expected {}", size_of::<T>()));
    }
    // SAFETY: Core Audio filled all `size_of::<T>()` bytes.
    Ok(unsafe { value.assume_init() })
}

/// Resolves a `CoreAudio` function at runtime.
///
/// # Safety
///
/// `F` must be the function-pointer type of the symbol `name`.
unsafe fn lookup<F: Copy>(name: &CStr) -> Option<F> {
    // SAFETY: dlsym is thread-safe; `name` is NUL-terminated.
    let symbol = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `F` is the symbol's pointer type.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&symbol) })
}

fn check(status: i32, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        let code = status.to_be_bytes();
        let fourcc = if code.iter().all(u8::is_ascii_graphic) {
            format!(" '{}'", String::from_utf8_lossy(&code))
        } else {
            String::new()
        };
        Err(format!("{what} failed: OSStatus {status}{fourcc}"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tauri::ipc::{Channel, InvokeResponseBody};

    use super::SystemAudioCapture;

    /// Captures real system audio while `afplay` plays a system sound.
    /// Needs the "System Audio Recording" permission for the test runner
    /// (without it the tap delivers silence), so it is opt-in:
    /// `cargo test -p mumble-tauri --lib system_audio -- --ignored`.
    #[test]
    #[ignore = "plays a sound and needs the macOS audio capture permission"]
    fn captures_system_audio() {
        let bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&bytes);
        let channel: Channel<InvokeResponseBody> = Channel::new(move |body| {
            if let InvokeResponseBody::Raw(raw) = body {
                sink.lock().unwrap().extend_from_slice(&raw);
            }
            Ok(())
        });

        let capture = SystemAudioCapture::start(channel).expect("start capture");
        let format = capture.format();
        let status = std::process::Command::new("afplay")
            .arg("/System/Library/Sounds/Glass.aiff")
            .status()
            .expect("run afplay");
        assert!(status.success());
        std::thread::sleep(Duration::from_millis(300));
        drop(capture);

        let bytes = bytes.lock().unwrap();
        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let peak = samples.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        let seconds = samples.len() as f64 / f64::from(format.channels) / format.sample_rate;
        println!("format={format:?} seconds={seconds:.2} peak={peak}");
        assert!(seconds > 0.5, "too little audio delivered: {seconds:.2}s");
        assert!(peak > 0.001, "only silence captured (permission missing?)");
    }
}
