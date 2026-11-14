use crate::audio::{
    convert_pcm_i16_to_stereo_f32, trim_stereo_ring_buffer_hysteresis, AudioRuntimeControls,
    AudioSink, AudioSource, RING_BUFFER_DURATION_MS, TARGET_SAMPLE_RATE, VOICE_MAX_BACKLOG_SECS,
    VOICE_TARGET_CUSHION_SECS,
};
use anyhow::{anyhow, bail, Result};
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use coreaudio_sys::*;
use rtrb::{Consumer, Producer, RingBuffer};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct VirtualAudioDevice {
    pub id: AudioObjectID,
    pub uid: String,
    pub name: String,
    pub manufacturer: String,
    pub input_channels: u32,
    pub output_channels: u32,
    pub sample_rate: u32,
    pub priority: u8,
}

impl VirtualAudioDevice {
    pub fn has_output(&self) -> bool {
        self.output_channels > 0
    }

    pub fn has_input(&self) -> bool {
        self.input_channels > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingSelection {
    pub output_sink_uid: Option<String>,
    pub input_capture_uid: Option<String>,
}

/// Strictly classifies whether a CoreAudio device is an eligible user virtual audio bus
/// (e.g. BlackHole, VB-Cable, Loopback, Soundflower, Virtual Audio) and NEVER a physical speaker/mic
/// or an app-internal capture plugin (like Teams/Zoom/NDI internal screen-share drivers).
pub fn classify_virtual_device(
    name: &str,
    manufacturer: &str,
    transport_type: u32,
) -> Option<u8> {
    let lower_name = name.to_lowercase();
    let lower_mfg = manufacturer.to_lowercase();

    // Explicitly block physical transports first: zero chance of routing to MacBook speakers/mics,
    // USB headsets, Bluetooth AirPods, HDMI/DisplayPort monitors, Thunderbolt, AirPlay, etc.
    let physical_transports = [
        kAudioDeviceTransportTypeBuiltIn,
        kAudioDeviceTransportTypeUSB,
        kAudioDeviceTransportTypeBluetooth,
        kAudioDeviceTransportTypeBluetoothLE,
        kAudioDeviceTransportTypePCI,
        kAudioDeviceTransportTypeFireWire,
        kAudioDeviceTransportTypeHDMI,
        kAudioDeviceTransportTypeDisplayPort,
        kAudioDeviceTransportTypeAirPlay,
        kAudioDeviceTransportTypeAVB,
        kAudioDeviceTransportTypeThunderbolt,
    ];
    if physical_transports.contains(&transport_type) {
        return None;
    }

    // Block conference/capture apps' own internal virtual screen-audio drivers so they are never
    // confused with user-routable loopback buses like BlackHole or VB-Cable.
    let excluded_internal_drivers = [
        "microsoft teams audio",
        "zoomaudiodevice",
        "webexmediaaudiodevice",
        "ndi audio",
        "steam streaming",
        "parsec virtual",
        "discord",
    ];
    if excluded_internal_drivers
        .iter()
        .any(|pat| lower_name.contains(pat))
    {
        return None;
    }

    // Priority ranking (higher = preferred default):
    // 100: BlackHole 2ch (primary Mic bus)
    //  95: BlackHole 16ch (secondary Speaker/Capture bus)
    //  90: Other BlackHole buses (64ch, etc.)
    //  85: VB-Cable / Cable
    //  80: Rogue Amoeba Loopback / Soundflower / Existential Audio / GroundControl
    //  60: Any other virtual-transport or explicitly named virtual cable bus
    if lower_name.contains("blackhole 2ch") {
        return Some(100);
    }
    if lower_name.contains("blackhole 16ch") {
        return Some(95);
    }
    if lower_name.contains("blackhole") || lower_mfg.contains("existential audio") {
        return Some(90);
    }
    if lower_name.contains("vb-cable")
        || lower_name.contains("cable")
        || lower_mfg.contains("vb audio")
    {
        return Some(85);
    }
    if lower_name.contains("loopback")
        || lower_name.contains("soundflower")
        || lower_mfg.contains("rogue amoeba")
        || lower_name.contains("groundcontrol")
        || lower_name.contains("virtual audio")
    {
        return Some(80);
    }
    if lower_name.contains("virtual") || transport_type == kAudioDeviceTransportTypeVirtual {
        return Some(60);
    }

    None
}

/// Enumerates ONLY virtual audio devices from CoreAudio HAL, sorted by priority then name.
pub fn enumerate_virtual_devices() -> Vec<VirtualAudioDevice> {
    let mut result = Vec::new();

    for id in query_all_audio_device_ids() {
        let Some(uid) = query_cfstring_property(
            id,
            kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal,
        ) else {
            continue;
        };

        let name = query_cfstring_property(
            id,
            kAudioObjectPropertyName,
            kAudioObjectPropertyScopeGlobal,
        )
        .unwrap_or_else(|| uid.clone());

        let manufacturer = query_cfstring_property(
            id,
            kAudioObjectPropertyManufacturer,
            kAudioObjectPropertyScopeGlobal,
        )
        .unwrap_or_default();

        let transport_type = query_u32_property(
            id,
            kAudioDevicePropertyTransportType,
            kAudioObjectPropertyScopeGlobal,
        )
        .unwrap_or(kAudioDeviceTransportTypeUnknown);

        let Some(priority) = classify_virtual_device(&name, &manufacturer, transport_type) else {
            continue;
        };

        let input_channels = query_stream_channel_count(id, kAudioObjectPropertyScopeInput);
        let output_channels = query_stream_channel_count(id, kAudioObjectPropertyScopeOutput);
        if input_channels == 0 && output_channels == 0 {
            continue;
        }

        let sample_rate = query_f64_property(
            id,
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
        )
        .map(|r| r.round() as u32)
        .unwrap_or(TARGET_SAMPLE_RATE);

        result.push(VirtualAudioDevice {
            id,
            uid,
            name,
            manufacturer,
            input_channels,
            output_channels,
            sample_rate,
            priority,
        });
    }

    result.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.name.cmp(&b.name))
    });
    result
}

/// Computes smart default virtual routing:
/// - If both `BlackHole 2ch` and `BlackHole 16ch` exist -> Output = `BlackHole 2ch`, Input = `BlackHole 16ch`.
/// - If only 1 virtual bus exists (e.g. `BlackHole 2ch` or `VB-Cable`) -> Output = that bus, Input = `None` (`RecvOnly`).
/// - If 2+ distinct virtual buses exist -> Output = top output bus, Input = top *different* input bus.
pub fn select_default_routing(devices: &[VirtualAudioDevice]) -> RoutingSelection {
    let outputs: Vec<&VirtualAudioDevice> = devices.iter().filter(|d| d.has_output()).collect();
    let inputs: Vec<&VirtualAudioDevice> = devices.iter().filter(|d| d.has_input()).collect();

    if outputs.is_empty() {
        return RoutingSelection {
            output_sink_uid: None,
            input_capture_uid: None,
        };
    }

    let bh_2ch = outputs
        .iter()
        .find(|d| d.name.to_lowercase().contains("blackhole 2ch"))
        .copied();
    let bh_16ch = inputs
        .iter()
        .find(|d| d.name.to_lowercase().contains("blackhole 16ch"))
        .copied();

    if let (Some(out_dev), Some(in_dev)) = (bh_2ch, bh_16ch) {
        if !is_same_virtual_bus(out_dev, in_dev) {
            return RoutingSelection {
                output_sink_uid: Some(out_dev.uid.clone()),
                input_capture_uid: Some(in_dev.uid.clone()),
            };
        }
    }

    let chosen_output = bh_2ch.unwrap_or(outputs[0]);
    let chosen_input = inputs
        .into_iter()
        .find(|cand| !is_same_virtual_bus(chosen_output, cand))
        .map(|d| d.uid.clone());

    RoutingSelection {
        output_sink_uid: Some(chosen_output.uid.clone()),
        input_capture_uid: chosen_input,
    }
}

/// Prevents selecting the exact same virtual bus as both Output Sink and Input Capture
/// (which would route the mobile mic straight back to itself on a loopback cable).
pub fn is_same_virtual_bus(a: &VirtualAudioDevice, b: &VirtualAudioDevice) -> bool {
    if a.uid == b.uid || a.id == b.id {
        return true;
    }
    a.name.trim().eq_ignore_ascii_case(b.name.trim())
}

/// Validates persisted or user-updated routing against currently available virtual devices,
/// enforcing the never-same-bus rule and falling back cleanly when hardware changes.
pub fn sanitize_routing_selection(
    devices: &[VirtualAudioDevice],
    desired_output_uid: Option<&str>,
    desired_input_uid: Option<&str>,
) -> RoutingSelection {
    let defaults = select_default_routing(devices);

    let output_dev = desired_output_uid
        .and_then(|uid| devices.iter().find(|d| d.uid == uid && d.has_output()))
        .or_else(|| {
            defaults
                .output_sink_uid
                .as_deref()
                .and_then(|uid| devices.iter().find(|d| d.uid == uid))
        });

    let Some(out_dev) = output_dev else {
        return RoutingSelection {
            output_sink_uid: None,
            input_capture_uid: None,
        };
    };

    // Note: `desired_input_uid == None` is an explicit valid choice (`None (RecvOnly)`).
    let input_uid = match desired_input_uid {
        None => None,
        Some(req_uid) => devices
            .iter()
            .find(|d| d.uid == req_uid && d.has_input() && !is_same_virtual_bus(out_dev, d))
            .map(|d| d.uid.clone()),
    };

    RoutingSelection {
        output_sink_uid: Some(out_dev.uid.clone()),
        input_capture_uid: input_uid,
    }
}

/// Formats the counterpart device name users should select inside Zoom / Microsoft Teams / Webex.
pub fn meeting_app_counterpart_name(device: Option<&VirtualAudioDevice>) -> String {
    let Some(dev) = device else {
        return "None (RecvOnly — keep your normal Mac speakers in Zoom/Teams)".to_string();
    };
    let name = &dev.name;
    if name.contains("CABLE Input") {
        return "CABLE Output (VB-Audio Virtual Cable)".to_string();
    }
    if name.contains("CABLE Output") {
        return "CABLE Input (VB-Audio Virtual Cable)".to_string();
    }
    name.clone()
}

/// Lightweight wait-free CoreAudio device list change listener (`kAudioHardwarePropertyDevices`).
pub struct DeviceChangeWatcher {
    dirty: Arc<AtomicBool>,
    raw_flag_ptr: *const AtomicBool,
}

unsafe impl Send for DeviceChangeWatcher {}
unsafe impl Sync for DeviceChangeWatcher {}

unsafe extern "C" fn hardware_devices_listener(
    _in_object_id: AudioObjectID,
    _in_number_addresses: UInt32,
    _in_addresses: *const AudioObjectPropertyAddress,
    in_client_data: *mut c_void,
) -> OSStatus {
    if !in_client_data.is_null() {
        let flag = unsafe { &*(in_client_data as *const AtomicBool) };
        flag.store(true, Ordering::Release);
    }
    0
}

impl Default for DeviceChangeWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceChangeWatcher {
    pub fn new() -> Self {
        let dirty = Arc::new(AtomicBool::new(false));
        let raw_flag_ptr = Arc::into_raw(dirty.clone());

        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };

        unsafe {
            let _ = AudioObjectAddPropertyListener(
                kAudioObjectSystemObject,
                &addr,
                Some(hardware_devices_listener),
                raw_flag_ptr as *mut c_void,
            );
        }

        Self {
            dirty,
            raw_flag_ptr,
        }
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

impl Drop for DeviceChangeWatcher {
    fn drop(&mut self) {
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        unsafe {
            let _ = AudioObjectRemovePropertyListener(
                kAudioObjectSystemObject,
                &addr,
                Some(hardware_devices_listener),
                self.raw_flag_ptr as *mut c_void,
            );
            let _ = Arc::from_raw(self.raw_flag_ptr);
        }
    }
}

/// Context owned exclusively by the CoreAudio real-time Output Sink IOProc while active.
/// Zero heap allocations, zero locks, zero logging syscalls inside `sink_io_proc`.
struct SinkCallbackContext {
    consumer: Consumer<f32>,
    controls: Arc<AudioRuntimeControls>,
    sample_rate: f32,
}

unsafe extern "C" fn sink_io_proc(
    _in_device: AudioObjectID,
    _in_now: *const AudioTimeStamp,
    _in_input_data: *const AudioBufferList,
    _in_input_time: *const AudioTimeStamp,
    out_output_data: *mut AudioBufferList,
    _in_output_time: *const AudioTimeStamp,
    in_client_data: *mut c_void,
) -> OSStatus {
    if out_output_data.is_null() || in_client_data.is_null() {
        return 0;
    }

    let ctx = unsafe { &mut *(in_client_data as *mut SinkCallbackContext) };
    let abl = unsafe { &mut *out_output_data };
    let num_buffers = abl.mNumberBuffers as usize;
    if num_buffers == 0 {
        return 0;
    }

    let buffers = unsafe { slice::from_raw_parts_mut(abl.mBuffers.as_mut_ptr(), num_buffers) };

    // Determine frame count for this render quantum
    let ch0 = (buffers[0].mNumberChannels as usize).max(1);
    let frames = (buffers[0].mDataByteSize as usize) / (size_of::<f32>() * ch0);

    if frames == 0 {
        return 0;
    }

    // 1. Apply O(1) wait-free hysteresis backlog trimming before reading playout quantum
    let (max_backlog_frames, cushion_frames) = ctx.controls.thresholds_frames();
    let _ = trim_stereo_ring_buffer_hysteresis(
        &mut ctx.consumer,
        max_backlog_frames,
        cushion_frames,
    );

    // 2. Report current jitter buffer fill (ms) via relaxed atomic
    let buffered_frames_now = (ctx.consumer.slots() / 2) as f32;
    let fill_ms = (buffered_frames_now * 1000.0) / ctx.sample_rate.max(8_000.0);
    ctx.controls.store_buffer_fill_ms(fill_ms);

    let muted = ctx.controls.is_mute_incoming();
    let gain = if muted {
        0.0_f32
    } else {
        ctx.controls.output_gain()
    };

    let mut sum_sq = 0.0_f32;

    // 3. Render stereo samples into interleaved (e.g. BlackHole 2ch/16ch, VB-Cable)
    //    or non-interleaved CoreAudio output buffers.
    if num_buffers == 1 {
        let out_ch = (buffers[0].mNumberChannels as usize).max(1);
        let total_floats = (buffers[0].mDataByteSize as usize) / size_of::<f32>();
        if buffers[0].mData.is_null() || total_floats == 0 {
            return 0;
        }
        let dst = unsafe { slice::from_raw_parts_mut(buffers[0].mData as *mut f32, total_floats) };
        dst.fill(0.0);

        let safe_frames = frames.min(total_floats / out_ch);
        for f in 0..safe_frames {
            let l_raw = ctx.consumer.pop().unwrap_or(0.0);
            let r_raw = ctx.consumer.pop().unwrap_or(0.0);
            let l = (l_raw * gain).clamp(-1.0, 1.0);
            let r = (r_raw * gain).clamp(-1.0, 1.0);

            let base = f * out_ch;
            if out_ch == 1 {
                let m = (l + r) * 0.5;
                dst[base] = m;
                sum_sq += m * m;
            } else {
                dst[base] = l;
                dst[base + 1] = r;
                sum_sq += 0.5 * (l * l + r * r);
            }
        }
    } else {
        // Non-interleaved planar buffers (1 channel per AudioBuffer)
        for buf in buffers.iter_mut() {
            if !buf.mData.is_null() && buf.mDataByteSize > 0 {
                let len = (buf.mDataByteSize as usize) / size_of::<f32>();
                let slice_out = unsafe { slice::from_raw_parts_mut(buf.mData as *mut f32, len) };
                slice_out.fill(0.0);
            }
        }

        let has_right = num_buffers >= 2 && !buffers[1].mData.is_null();
        let len0 = (buffers[0].mDataByteSize as usize) / size_of::<f32>();
        let safe_frames = frames.min(len0);

        for f in 0..safe_frames {
            let l_raw = ctx.consumer.pop().unwrap_or(0.0);
            let r_raw = ctx.consumer.pop().unwrap_or(0.0);
            let l = (l_raw * gain).clamp(-1.0, 1.0);
            let r = (r_raw * gain).clamp(-1.0, 1.0);

            if !buffers[0].mData.is_null() {
                unsafe {
                    *(buffers[0].mData as *mut f32).add(f) = l;
                }
            }
            if has_right {
                let len1 = (buffers[1].mDataByteSize as usize) / size_of::<f32>();
                if f < len1 {
                    unsafe {
                        *(buffers[1].mData as *mut f32).add(f) = r;
                    }
                }
            }
            sum_sq += 0.5 * (l * l + r * r);
        }
    }

    let rms = (sum_sq / (frames as f32)).sqrt();
    ctx.controls.store_incoming_vu(rms);

    0
}

pub struct CoreAudioVirtualSink {
    controls: Arc<AudioRuntimeControls>,
    device_id: Option<AudioObjectID>,
    device_name: String,
    device_sample_rate: u32,
    proc_id: AudioDeviceIOProcID,
    callback_ctx_ptr: *mut SinkCallbackContext,
    producer: Option<Producer<f32>>,
    is_running: bool,
    scratch_stereo: Vec<f32>,
    scratch_resampled: Vec<f32>,
}

// Safety: `callback_ctx_ptr` is only mutated during `initialize`/`start`/`stop`/`drop` on the owner thread,
// while `sink_io_proc` exclusively accesses `SinkCallbackContext` between `AudioDeviceStart` and `AudioDeviceStop`.
unsafe impl Send for CoreAudioVirtualSink {}

impl CoreAudioVirtualSink {
    pub fn new(controls: Arc<AudioRuntimeControls>) -> Self {
        Self {
            controls,
            device_id: None,
            device_name: String::new(),
            device_sample_rate: TARGET_SAMPLE_RATE,
            proc_id: None,
            callback_ctx_ptr: null_mut(),
            producer: None,
            is_running: false,
            scratch_stereo: Vec::with_capacity(4096),
            scratch_resampled: Vec::with_capacity(4096),
        }
    }

    fn cleanup_hal(&mut self) {
        self.stop();
        if let (Some(dev_id), Some(_)) = (self.device_id, self.proc_id) {
            unsafe {
                let _ = AudioDeviceDestroyIOProcID(dev_id, self.proc_id);
            }
        }
        self.proc_id = None;
        if !self.callback_ctx_ptr.is_null() {
            unsafe {
                let _ = Box::from_raw(self.callback_ctx_ptr);
            }
            self.callback_ctx_ptr = null_mut();
        }
        self.producer = None;
        self.device_id = None;
    }
}

impl AudioSink for CoreAudioVirtualSink {
    fn initialize(
        &mut self,
        sample_rate: u32,
        _channels: u16,
        target_device_uid: Option<&str>,
    ) -> Result<()> {
        self.cleanup_hal();

        let virtual_devices = enumerate_virtual_devices();
        let outputs: Vec<&VirtualAudioDevice> =
            virtual_devices.iter().filter(|d| d.has_output()).collect();

        if outputs.is_empty() {
            bail!(
                "No virtual audio output sink found. Install BlackHole (`brew install --cask blackhole-2ch`) and try again."
            );
        }

        let target = if let Some(uid) = target_device_uid {
            outputs
                .iter()
                .find(|d| d.uid == uid)
                .copied()
                .ok_or_else(|| {
                    anyhow!(
                        "Selected virtual output device '{}' is no longer available.",
                        uid
                    )
                })?
        } else {
            outputs[0]
        };

        // Attempt to set nominal sample rate to 48000 Hz if supported by the virtual driver
        let _ = try_set_nominal_sample_rate(target.id, sample_rate as f64);
        let actual_rate = query_f64_property(
            target.id,
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
        )
        .map(|r| r.round() as u32)
        .unwrap_or(target.sample_rate)
        .max(8_000);

        self.controls.set_device_sample_rate(actual_rate);
        self.controls
            .configure_thresholds_for_rate(actual_rate, VOICE_MAX_BACKLOG_SECS, VOICE_TARGET_CUSHION_SECS);

        // Allocate 500ms wait-free stereo SPSC ring buffer
        let ring_frames = ((actual_rate as usize) * RING_BUFFER_DURATION_MS) / 1000;
        let (producer, consumer) = RingBuffer::<f32>::new(ring_frames * 2);

        let ctx = Box::new(SinkCallbackContext {
            consumer,
            controls: self.controls.clone(),
            sample_rate: actual_rate as f32,
        });
        let ctx_ptr = Box::into_raw(ctx);

        let mut proc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                target.id,
                Some(sink_io_proc),
                ctx_ptr as *mut c_void,
                &mut proc_id,
            )
        };

        if status != 0 || proc_id.is_none() {
            unsafe {
                let _ = Box::from_raw(ctx_ptr);
            }
            bail!(
                "CoreAudio AudioDeviceCreateIOProcID failed for '{}' (OSStatus = {})",
                target.name,
                status
            );
        }

        // Disable input stream usage on this Output Sink IOProc so macOS does not activate capture
        // DMA or prompt for mic permission when running RecvOnly / 440Hz sink tests.
        configure_ioproc_stream_usage(target.id, proc_id, false, true);

        self.device_id = Some(target.id);
        self.device_name = target.name.clone();
        self.device_sample_rate = actual_rate;
        self.proc_id = proc_id;
        self.callback_ctx_ptr = ctx_ptr;
        self.producer = Some(producer);

        tracing::debug!(
            "Initialized CoreAudio Virtual Sink '{}' (id={}, rate={} Hz)",
            self.device_name,
            target.id,
            actual_rate
        );
        Ok(())
    }

    fn write_pcm_i16(&mut self, pcm_data: &[i16], source_channels: u16, source_sample_rate: u32) {
        let Some(producer) = self.producer.as_mut() else {
            return;
        };
        if pcm_data.is_empty() {
            return;
        }

        convert_pcm_i16_to_stereo_f32(
            pcm_data,
            source_channels,
            source_sample_rate,
            self.device_sample_rate,
            &mut self.scratch_stereo,
            &mut self.scratch_resampled,
        );

        let needed = self.scratch_resampled.len();
        let free_slots = producer.slots();
        let writable = (free_slots.min(needed) / 2) * 2;
        if writable == 0 {
            return;
        }

        if let Ok(chunk) = producer.write_chunk_uninit(writable) {
            chunk.fill_from_iter(self.scratch_resampled[..writable].iter().copied());
        }
    }

    fn configure_buffer_thresholds(&self, max_backlog_secs: f64, target_cushion_secs: f64) {
        self.controls.configure_thresholds_for_rate(
            self.device_sample_rate,
            max_backlog_secs,
            target_cushion_secs,
        );
        tracing::debug!(
            "Configured CoreAudio sink hysteresis: max_backlog={:.0}ms, cushion={:.0}ms",
            max_backlog_secs * 1000.0,
            target_cushion_secs * 1000.0
        );
    }

    fn start(&mut self) -> Result<()> {
        if self.is_running {
            return Ok(());
        }
        let dev_id = self
            .device_id
            .ok_or_else(|| anyhow!("CoreAudioVirtualSink is not initialized"))?;
        let status = unsafe { AudioDeviceStart(dev_id, self.proc_id) };
        if status != 0 {
            bail!(
                "CoreAudio AudioDeviceStart failed for '{}' (OSStatus = {})",
                self.device_name,
                status
            );
        }
        self.is_running = true;
        Ok(())
    }

    fn stop(&mut self) {
        if self.is_running {
            if let Some(dev_id) = self.device_id {
                unsafe {
                    let _ = AudioDeviceStop(dev_id, self.proc_id);
                }
            }
            self.is_running = false;
            self.controls.store_incoming_vu(0.0);
            self.controls.store_buffer_fill_ms(0.0);
        }
    }

    fn device_name(&self) -> &str {
        &self.device_name
    }

    fn device_sample_rate(&self) -> u32 {
        self.device_sample_rate
    }
}

impl Drop for CoreAudioVirtualSink {
    fn drop(&mut self) {
        self.cleanup_hal();
    }
}

/// Context owned exclusively by the CoreAudio real-time Input Capture IOProc while active.
/// Zero heap allocations, zero locks, zero logging syscalls inside `source_io_proc`.
struct SourceCallbackContext {
    producer: Producer<f32>,
    controls: Arc<AudioRuntimeControls>,
}

unsafe extern "C" fn source_io_proc(
    _in_device: AudioObjectID,
    _in_now: *const AudioTimeStamp,
    in_input_data: *const AudioBufferList,
    _in_input_time: *const AudioTimeStamp,
    _out_output_data: *mut AudioBufferList,
    _in_output_time: *const AudioTimeStamp,
    in_client_data: *mut c_void,
) -> OSStatus {
    if in_input_data.is_null() || in_client_data.is_null() {
        return 0;
    }

    let ctx = unsafe { &mut *(in_client_data as *mut SourceCallbackContext) };
    let abl = unsafe { &*in_input_data };
    let num_buffers = abl.mNumberBuffers as usize;
    if num_buffers == 0 {
        return 0;
    }

    let buffers = unsafe { slice::from_raw_parts(abl.mBuffers.as_ptr(), num_buffers) };
    if buffers[0].mData.is_null() || buffers[0].mDataByteSize == 0 {
        return 0;
    }

    let mut sum_sq = 0.0_f32;
    let mut captured_frames = 0_usize;

    if num_buffers == 1 {
        let in_ch = (buffers[0].mNumberChannels as usize).max(1);
        let total_floats = (buffers[0].mDataByteSize as usize) / size_of::<f32>();
        let frames = total_floats / in_ch;
        let src = unsafe { slice::from_raw_parts(buffers[0].mData as *const f32, total_floats) };

        let max_writable_frames = ctx.producer.slots() / 2;
        let frames_to_push = frames.min(max_writable_frames);

        if frames_to_push > 0 {
            if let Ok(chunk) = ctx.producer.write_chunk_uninit(frames_to_push * 2) {
                let mut f_idx = 0_usize;
                chunk.fill_from_iter(std::iter::from_fn(|| {
                    let frame = f_idx / 2;
                    let is_right = (f_idx & 1) == 1;
                    f_idx += 1;
                    let base = frame * in_ch;
                    let sample = if is_right && in_ch >= 2 {
                        src[base + 1]
                    } else {
                        src[base]
                    };
                    sum_sq += 0.5 * sample * sample;
                    Some(sample)
                }));
                captured_frames = frames_to_push;
            }
        }
    } else {
        let len0 = (buffers[0].mDataByteSize as usize) / size_of::<f32>();
        let ch0 = unsafe { slice::from_raw_parts(buffers[0].mData as *const f32, len0) };
        let ch1_opt = if num_buffers >= 2 && !buffers[1].mData.is_null() {
            let len1 = (buffers[1].mDataByteSize as usize) / size_of::<f32>();
            Some(unsafe { slice::from_raw_parts(buffers[1].mData as *const f32, len1) })
        } else {
            None
        };

        let max_writable_frames = ctx.producer.slots() / 2;
        let frames_to_push = len0.min(max_writable_frames);

        if frames_to_push > 0 {
            if let Ok(chunk) = ctx.producer.write_chunk_uninit(frames_to_push * 2) {
                let mut f_idx = 0_usize;
                chunk.fill_from_iter(std::iter::from_fn(|| {
                    let frame = f_idx / 2;
                    let is_right = (f_idx & 1) == 1;
                    f_idx += 1;
                    let sample = if is_right {
                        if let Some(ch1) = ch1_opt {
                            if frame < ch1.len() {
                                ch1[frame]
                            } else {
                                ch0[frame]
                            }
                        } else {
                            ch0[frame]
                        }
                    } else {
                        ch0[frame]
                    };
                    sum_sq += 0.5 * sample * sample;
                    Some(sample)
                }));
                captured_frames = frames_to_push;
            }
        }
    }

    if captured_frames > 0 {
        let rms = (sum_sq / (captured_frames as f32)).sqrt();
        ctx.controls.store_outgoing_vu(rms);
    } else {
        ctx.controls.store_outgoing_vu(0.0);
    }

    0
}

pub struct CoreAudioVirtualSource {
    controls: Arc<AudioRuntimeControls>,
    device_id: Option<AudioObjectID>,
    device_name: String,
    device_sample_rate: u32,
    proc_id: AudioDeviceIOProcID,
    callback_ctx_ptr: *mut SourceCallbackContext,
    consumer: Option<Consumer<f32>>,
    is_running: bool,
}

unsafe impl Send for CoreAudioVirtualSource {}

impl CoreAudioVirtualSource {
    pub fn new(controls: Arc<AudioRuntimeControls>) -> Self {
        Self {
            controls,
            device_id: None,
            device_name: String::new(),
            device_sample_rate: TARGET_SAMPLE_RATE,
            proc_id: None,
            callback_ctx_ptr: null_mut(),
            consumer: None,
            is_running: false,
        }
    }

    fn cleanup_hal(&mut self) {
        self.stop();
        if let (Some(dev_id), Some(_)) = (self.device_id, self.proc_id) {
            unsafe {
                let _ = AudioDeviceDestroyIOProcID(dev_id, self.proc_id);
            }
        }
        self.proc_id = None;
        if !self.callback_ctx_ptr.is_null() {
            unsafe {
                let _ = Box::from_raw(self.callback_ctx_ptr);
            }
            self.callback_ctx_ptr = null_mut();
        }
        self.consumer = None;
        self.device_id = None;
    }
}

impl AudioSource for CoreAudioVirtualSource {
    fn initialize(&mut self, target_device_uid: &str) -> Result<()> {
        self.cleanup_hal();

        let virtual_devices = enumerate_virtual_devices();
        let target = virtual_devices
            .iter()
            .find(|d| d.uid == target_device_uid && d.has_input())
            .ok_or_else(|| {
                anyhow!(
                    "Selected virtual input device '{}' is no longer available.",
                    target_device_uid
                )
            })?;

        let _ = try_set_nominal_sample_rate(target.id, TARGET_SAMPLE_RATE as f64);
        let actual_rate = query_f64_property(
            target.id,
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
        )
        .map(|r| r.round() as u32)
        .unwrap_or(target.sample_rate)
        .max(8_000);

        let ring_frames = ((actual_rate as usize) * RING_BUFFER_DURATION_MS) / 1000;
        let (producer, consumer) = RingBuffer::<f32>::new(ring_frames * 2);

        let ctx = Box::new(SourceCallbackContext {
            producer,
            controls: self.controls.clone(),
        });
        let ctx_ptr = Box::into_raw(ctx);

        let mut proc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                target.id,
                Some(source_io_proc),
                ctx_ptr as *mut c_void,
                &mut proc_id,
            )
        };

        if status != 0 || proc_id.is_none() {
            unsafe {
                let _ = Box::from_raw(ctx_ptr);
            }
            bail!(
                "CoreAudio AudioDeviceCreateIOProcID (Capture) failed for '{}' (OSStatus = {})",
                target.name,
                status
            );
        }

        // Enable ONLY Input stream usage and disable Output stream usage on this Capture IOProc
        // so we never write silence onto the virtual cable's render side.
        configure_ioproc_stream_usage(target.id, proc_id, true, false);

        self.device_id = Some(target.id);
        self.device_name = target.name.clone();
        self.device_sample_rate = actual_rate;
        self.proc_id = proc_id;
        self.callback_ctx_ptr = ctx_ptr;
        self.consumer = Some(consumer);

        tracing::debug!(
            "Initialized CoreAudio Virtual Capture '{}' (id={}, rate={} Hz)",
            self.device_name,
            target.id,
            actual_rate
        );
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        if self.is_running {
            return Ok(());
        }
        let dev_id = self
            .device_id
            .ok_or_else(|| anyhow!("CoreAudioVirtualSource is not initialized"))?;
        let status = unsafe { AudioDeviceStart(dev_id, self.proc_id) };
        if status != 0 {
            bail!(
                "CoreAudio AudioDeviceStart (Capture) failed for '{}' (OSStatus = {})",
                self.device_name,
                status
            );
        }
        self.is_running = true;
        Ok(())
    }

    fn stop(&mut self) {
        if self.is_running {
            if let Some(dev_id) = self.device_id {
                unsafe {
                    let _ = AudioDeviceStop(dev_id, self.proc_id);
                }
            }
            self.is_running = false;
            self.controls.store_outgoing_vu(0.0);
        }
    }

    fn read_stereo_f32(&mut self, out: &mut [f32]) -> usize {
        let Some(consumer) = self.consumer.as_mut() else {
            return 0;
        };
        let available = consumer.slots();
        let to_read = (available.min(out.len()) / 2) * 2;
        if to_read == 0 {
            return 0;
        }
        if let Ok(chunk) = consumer.read_chunk(to_read) {
            let (first, second) = chunk.as_slices();
            out[..first.len()].copy_from_slice(first);
            if !second.is_empty() {
                out[first.len()..first.len() + second.len()].copy_from_slice(second);
            }
            chunk.commit_all();
            return to_read;
        }
        0
    }

    fn device_sample_rate(&self) -> u32 {
        self.device_sample_rate
    }

    fn device_name(&self) -> &str {
        &self.device_name
    }
}

impl Drop for CoreAudioVirtualSource {
    fn drop(&mut self) {
        self.cleanup_hal();
    }
}

// --- Low-level CoreAudio HAL Property Helpers ---

fn query_all_audio_device_ids() -> Vec<AudioObjectID> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut data_size: UInt32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &addr, 0, null(), &mut data_size)
    };
    if status != 0 || data_size == 0 {
        return Vec::new();
    }

    let count = (data_size as usize) / size_of::<AudioObjectID>();
    let mut ids = vec![0 as AudioObjectID; count];
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &addr,
            0,
            null(),
            &mut data_size,
            ids.as_mut_ptr() as *mut c_void,
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids
}

fn query_cfstring_property(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<String> {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut cf_str: CFStringRef = null();
    let mut size = size_of::<CFStringRef>() as UInt32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            &addr,
            0,
            null(),
            &mut size,
            (&mut cf_str as *mut CFStringRef) as *mut c_void,
        )
    };
    if status != 0 || cf_str.is_null() {
        return None;
    }

    let wrapped = unsafe { CFString::wrap_under_create_rule(cf_str) };
    Some(wrapped.to_string())
}

fn query_u32_property(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<u32> {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut val: UInt32 = 0;
    let mut size = size_of::<UInt32>() as UInt32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            &addr,
            0,
            null(),
            &mut size,
            (&mut val as *mut UInt32) as *mut c_void,
        )
    };
    if status == 0 {
        Some(val)
    } else {
        None
    }
}

fn query_f64_property(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<f64> {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut val: f64 = 0.0;
    let mut size = size_of::<f64>() as UInt32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            &addr,
            0,
            null(),
            &mut size,
            (&mut val as *mut f64) as *mut c_void,
        )
    };
    if status == 0 {
        Some(val)
    } else {
        None
    }
}

fn try_set_nominal_sample_rate(device_id: AudioObjectID, desired_rate: f64) -> bool {
    if let Some(current) = query_f64_property(
        device_id,
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    ) {
        if (current - desired_rate).abs() < 1.0 {
            return true;
        }
    }

    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut settable: Boolean = 0;
    let status = unsafe { AudioObjectIsPropertySettable(device_id, &addr, &mut settable) };
    if status != 0 || settable == 0 {
        return false;
    }

    let rate = desired_rate;
    let set_status = unsafe {
        AudioObjectSetPropertyData(
            device_id,
            &addr,
            0,
            null(),
            size_of::<f64>() as UInt32,
            (&rate as *const f64) as *const c_void,
        )
    };
    set_status == 0
}

fn query_stream_channel_count(
    device_id: AudioObjectID,
    scope: AudioObjectPropertyScope,
) -> u32 {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut data_size: UInt32 = 0;
    let status =
        unsafe { AudioObjectGetPropertyDataSize(device_id, &addr, 0, null(), &mut data_size) };
    if status != 0 || (data_size as usize) < size_of::<AudioBufferList>() {
        return 0;
    }

    // Allocate u64-aligned storage for variable-length AudioBufferList
    let u64_len = (data_size as usize).div_ceil(size_of::<u64>());
    let mut backing = vec![0_u64; u64_len];
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &addr,
            0,
            null(),
            &mut data_size,
            backing.as_mut_ptr() as *mut c_void,
        )
    };
    if status != 0 {
        return 0;
    }

    let abl = unsafe { &*(backing.as_ptr() as *const AudioBufferList) };
    let num_buffers = abl.mNumberBuffers as usize;
    if num_buffers == 0 {
        return 0;
    }

    let buffers = unsafe { slice::from_raw_parts(abl.mBuffers.as_ptr(), num_buffers) };
    buffers.iter().map(|b| b.mNumberChannels).sum()
}

fn configure_ioproc_stream_usage(
    device_id: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    enable_input: bool,
    enable_output: bool,
) {
    let raw_proc_ptr: *mut c_void = unsafe { zeroed_or_proc_ptr(proc_id) };
    if raw_proc_ptr.is_null() {
        return;
    }

    set_scope_stream_usage(
        device_id,
        raw_proc_ptr,
        kAudioObjectPropertyScopeInput,
        enable_input,
    );
    set_scope_stream_usage(
        device_id,
        raw_proc_ptr,
        kAudioObjectPropertyScopeOutput,
        enable_output,
    );
}

unsafe fn zeroed_or_proc_ptr(proc_id: AudioDeviceIOProcID) -> *mut c_void {
    match proc_id {
        Some(f) => f as *mut c_void,
        None => null_mut(),
    }
}

fn set_scope_stream_usage(
    device_id: AudioObjectID,
    raw_proc_ptr: *mut c_void,
    scope: AudioObjectPropertyScope,
    enabled: bool,
) {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyIOProcStreamUsage,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut data_size: UInt32 = 0;
    let status =
        unsafe { AudioObjectGetPropertyDataSize(device_id, &addr, 0, null(), &mut data_size) };
    if status != 0 || (data_size as usize) < size_of::<AudioHardwareIOProcStreamUsage>() {
        return;
    }

    let u64_len = (data_size as usize).div_ceil(size_of::<u64>());
    let mut backing = vec![0_u64; u64_len];
    let usage_ptr = backing.as_mut_ptr() as *mut AudioHardwareIOProcStreamUsage;
    unsafe {
        (*usage_ptr).mIOProc = raw_proc_ptr;
    }

    let get_status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &addr,
            0,
            null(),
            &mut data_size,
            usage_ptr as *mut c_void,
        )
    };
    if get_status != 0 {
        return;
    }

    unsafe {
        let num_streams = (*usage_ptr).mNumberStreams as usize;
        let flags = slice::from_raw_parts_mut((*usage_ptr).mStreamIsOn.as_mut_ptr(), num_streams);
        let flag_val: UInt32 = if enabled { 1 } else { 0 };
        for flag in flags {
            *flag = flag_val;
        }
        let _ = AudioObjectSetPropertyData(
            device_id,
            &addr,
            0,
            null(),
            data_size,
            usage_ptr as *const c_void,
        );
    }
    let _ = unsafe { zeroed::<UInt32>() };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_dev(uid: &str, name: &str, in_ch: u32, out_ch: u32, priority: u8) -> VirtualAudioDevice {
        VirtualAudioDevice {
            id: priority as u32,
            uid: uid.to_string(),
            name: name.to_string(),
            manufacturer: "Test".to_string(),
            input_channels: in_ch,
            output_channels: out_ch,
            sample_rate: 48_000,
            priority,
        }
    }

    #[test]
    fn physical_and_internal_conference_drivers_are_excluded() {
        assert_eq!(
            classify_virtual_device("MacBook Pro Speakers", "Apple Inc.", kAudioDeviceTransportTypeBuiltIn),
            None
        );
        assert_eq!(
            classify_virtual_device("LG HDR 4K", "GSM", kAudioDeviceTransportTypeDisplayPort),
            None
        );
        assert_eq!(
            classify_virtual_device("Microsoft Teams Audio", "Microsoft Corp.", kAudioDeviceTransportTypeVirtual),
            None
        );
        assert_eq!(
            classify_virtual_device("BlackHole 2ch", "Existential Audio Inc.", kAudioDeviceTransportTypeVirtual),
            Some(100)
        );
        assert_eq!(
            classify_virtual_device("BlackHole 16ch", "Existential Audio Inc.", kAudioDeviceTransportTypeVirtual),
            Some(95)
        );
        assert_eq!(
            classify_virtual_device("VB-Cable", "VB Audio", kAudioDeviceTransportTypeVirtual),
            Some(85)
        );
    }

    #[test]
    fn single_bus_defaults_to_recv_only_and_blocks_same_bus_loopback() {
        let devices = vec![mock_dev("BlackHole2ch_UID", "BlackHole 2ch", 2, 2, 100)];
        let defaults = select_default_routing(&devices);
        assert_eq!(defaults.output_sink_uid.as_deref(), Some("BlackHole2ch_UID"));
        assert_eq!(defaults.input_capture_uid, None);

        // Attempting to select the exact same virtual bus for both Output and Input must be rejected
        let sanitized = sanitize_routing_selection(
            &devices,
            Some("BlackHole2ch_UID"),
            Some("BlackHole2ch_UID"),
        );
        assert_eq!(sanitized.output_sink_uid.as_deref(), Some("BlackHole2ch_UID"));
        assert_eq!(sanitized.input_capture_uid, None);
    }

    #[test]
    fn dual_blackhole_buses_default_to_2ch_sink_and_16ch_capture() {
        let devices = vec![
            mock_dev("BH2_UID", "BlackHole 2ch", 2, 2, 100),
            mock_dev("BH16_UID", "BlackHole 16ch", 16, 16, 95),
        ];
        let defaults = select_default_routing(&devices);
        assert_eq!(defaults.output_sink_uid.as_deref(), Some("BH2_UID"));
        assert_eq!(defaults.input_capture_uid.as_deref(), Some("BH16_UID"));
    }
}
