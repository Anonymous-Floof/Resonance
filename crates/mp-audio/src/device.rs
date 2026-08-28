//! Output device selection and stream construction.
//!
//! Kept separate from the engine so the device can be re-opened (a headset is
//! unplugged, the user picks a different output) without disturbing the decode
//! pipeline.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig, SupportedStreamConfig};

use crate::error::{AudioError, Result};

/// An output device the user can choose between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
}

/// Human-readable name for a device, falling back if the backend will not say.
fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "Unknown device".to_owned())
}

/// Every output device currently available, default first.
///
/// Errors are swallowed deliberately: a single misbehaving device should not
/// prevent the settings screen from listing the others.
pub fn list_outputs() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default_name = host.default_output_device().map(|d| device_name(&d));

    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };

    let mut infos: Vec<DeviceInfo> = devices
        .map(|device| {
            let name = device_name(&device);
            let is_default = Some(&name) == default_name.as_ref();
            DeviceInfo { name, is_default }
        })
        .collect();

    infos.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
    infos.dedup();
    infos
}

/// An opened device together with the format it agreed to.
pub struct Output {
    pub device: Device,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
    pub name: String,
}

impl Output {
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.config.channels as usize
    }
}

/// Open `preferred` by name, or the system default if it is absent or gone.
///
/// Falling back rather than failing matters: a config that names a Bluetooth
/// headset should still start the player when that headset is switched off.
pub fn open(preferred: Option<&str>, buffer_frames: Option<u32>) -> Result<Output> {
    let host = cpal::default_host();

    let device = preferred
        .and_then(|wanted| {
            let found = host
                .output_devices()
                .ok()?
                .find(|d| device_name(d) == wanted);

            if found.is_none() {
                tracing::warn!("output device {wanted:?} not found, using the system default");
            }
            found
        })
        .or_else(|| host.default_output_device())
        .ok_or(AudioError::NoOutputDevice)?;

    let name = device_name(&device);

    let supported: SupportedStreamConfig = device
        .default_output_config()
        .map_err(|err| AudioError::DeviceInit(err.to_string()))?;

    let sample_format = supported.sample_format();
    let mut config: StreamConfig = supported.config();

    // A larger buffer trades latency for resilience against scheduling hiccups.
    // Left unset, the backend picks something sensible for the device.
    if let Some(frames) = buffer_frames {
        config.buffer_size = cpal::BufferSize::Fixed(frames);
    }

    tracing::info!(
        "audio output: {name} @ {} Hz, {} ch, {sample_format:?}",
        config.sample_rate,
        config.channels
    );

    Ok(Output {
        device,
        config,
        sample_format,
        name,
    })
}

/// Build and start an output stream.
///
/// `fill` is handed an interleaved `f32` buffer to populate and must be
/// real-time safe: no allocation, no locking, no I/O. Sample-format conversion
/// to whatever the device actually wants happens here, so `fill` only ever deals
/// in `f32`.
pub fn build_stream<F>(output: &Output, mut fill: F) -> Result<cpal::Stream>
where
    F: FnMut(&mut [f32]) + Send + 'static,
{
    let error_callback = |err| tracing::error!("audio output error: {err}");

    // `cpal` is generic over the device's sample type, but the engine only
    // speaks `f32`. For integer formats we render into a scratch buffer and
    // convert on the way out.
    macro_rules! build_converting {
        ($sample:ty) => {{
            let mut scratch: Vec<f32> = Vec::new();
            output.device.build_output_stream::<$sample, _, _>(
                output.config,
                move |out: &mut [$sample], _| {
                    if scratch.len() < out.len() {
                        // Grows once, during the first few callbacks, then never
                        // again; the device buffer size does not change.
                        scratch.resize(out.len(), 0.0);
                    }
                    let block = &mut scratch[..out.len()];
                    block.fill(0.0);
                    fill(block);

                    for (dst, src) in out.iter_mut().zip(block.iter()) {
                        *dst = <$sample as cpal::Sample>::from_sample(*src);
                    }
                },
                error_callback,
                None,
            )
        }};
    }

    let stream = match output.sample_format {
        SampleFormat::F32 => output.device.build_output_stream::<f32, _, _>(
            output.config,
            move |out: &mut [f32], _| {
                out.fill(0.0);
                fill(out);
            },
            error_callback,
            None,
        ),
        SampleFormat::I16 => build_converting!(i16),
        SampleFormat::U16 => build_converting!(u16),
        SampleFormat::I32 => build_converting!(i32),
        SampleFormat::F64 => build_converting!(f64),
        SampleFormat::I8 => build_converting!(i8),
        SampleFormat::U8 => build_converting!(u8),
        other => {
            return Err(AudioError::DeviceInit(format!(
                "unsupported device sample format {other:?}"
            )));
        }
    }
    .map_err(|err| AudioError::DeviceInit(err.to_string()))?;

    stream
        .play()
        .map_err(|err| AudioError::DeviceInit(err.to_string()))?;

    Ok(stream)
}
