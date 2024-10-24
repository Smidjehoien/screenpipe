use std::path::PathBuf;
use std::sync::Arc;

use crate::core::AudioSegment;
use crate::vad_engine::{SpeechBoundary, VadEngine};
use crate::AudioDevice;
use anyhow::Result;
use candle_transformers::models::whisper::{self as m};

use realfft::num_complex::{Complex32, ComplexFloat};
use realfft::RealFftPlanner;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use tracing::debug;

pub fn normalize_v2(audio: &[f32]) -> Vec<f32> {
    let rms = (audio.iter().map(|&x| x * x).sum::<f32>() / audio.len() as f32).sqrt();
    let peak = audio
        .iter()
        .fold(0.0f32, |max, &sample| max.max(sample.abs()));

    let target_rms = 0.2; // Adjust as needed
    let target_peak = 0.95; // Adjust as needed

    let rms_scaling = target_rms / rms;
    let peak_scaling = target_peak / peak;

    let scaling_factor = rms_scaling.min(peak_scaling);

    audio
        .iter()
        .map(|&sample| sample * scaling_factor)
        .collect()
}

pub fn spectral_subtraction(audio: &[f32], d: f32) -> Result<Vec<f32>> {
    let mut real_planner = RealFftPlanner::<f32>::new();
    let window_size = 1600; // 16k sample rate - 100ms
    let r2c = real_planner.plan_fft_forward(window_size);

    let mut y = r2c.make_output_vec();

    let mut padded_audio = audio.to_vec();

    padded_audio.append(&mut vec![0.0f32; window_size - audio.len()]);

    let mut indata = padded_audio;
    r2c.process(&mut indata, &mut y)?;

    let mut processed_audio = y
        .iter()
        .map(|&x| {
            let magnitude_y = x.abs().powf(2.0);

            let div = 1.0 - (d / magnitude_y);

            let gain = {
                if div > 0.0 {
                    f32::sqrt(div)
                } else {
                    0.0f32
                }
            };

            x * gain
        })
        .collect::<Vec<Complex32>>();

    let c2r = real_planner.plan_fft_inverse(window_size);

    let mut outdata = c2r.make_output_vec();

    c2r.process(&mut processed_audio, &mut outdata)?;

    Ok(outdata)
}

// not an average of non-speech segments, but I don't know how much pause time we
// get. for now, we will just assume the noise is constant (kinda defeats the purpose)
// but oh well
pub fn average_noise_spectrum(audio: &[f32]) -> f32 {
    let mut total_sum = 0.0f32;

    for sample in audio {
        let magnitude = sample.abs();

        total_sum += magnitude.powf(2.0);
    }

    total_sum / audio.len() as f32
}

pub fn audio_to_mono(audio: &[f32], channels: u16) -> Vec<f32> {
    let mut mono_samples = Vec::with_capacity(audio.len() / channels as usize);

    // Iterate over the audio slice in chunks, each containing `channels` samples
    for chunk in audio.chunks(channels as usize) {
        // Sum the samples from all channels in the current chunk
        let sum: f32 = chunk.iter().sum();

        // Calculate the averagechannelsono sample
        let mono_sample = sum / channels as f32;

        // Store the computed mono sample
        mono_samples.push(mono_sample);
    }

    mono_samples
}

#[derive(Debug, Clone)]
pub struct AudioInput {
    pub data: Arc<Vec<AudioSegment>>,
    pub sample_rate: u32,
    pub channels: u16,
    pub device: Arc<AudioDevice>,
    pub output_path: Arc<PathBuf>,
}

pub fn audio_frames_to_speech_frames(
    data: &[f32],
    device: Arc<AudioDevice>,
    sample_rate: u32,
    vad_engine: &mut Box<dyn VadEngine + Send>,
) -> Result<Option<Vec<f32>>> {
    let audio_data = if sample_rate != m::SAMPLE_RATE as u32 {
        debug!(
            "device: {}, resampling from {} Hz to {} Hz",
            device,
            sample_rate,
            m::SAMPLE_RATE
        );
        resample(data.as_ref(), sample_rate, m::SAMPLE_RATE as u32)?
    } else {
        data.to_vec()
    };

    let audio_data = normalize_v2(&audio_data);

    const FRAME_SIZE: usize = 1600; // 100ms frame size for 16kHz audio
    let mut speech_frames = Vec::new();
    let mut total_frames = 0;
    let mut speech_frame_count = 0;
    let mut noise = 0.;
    let mut is_speech_active = false;

    for chunk in audio_data.chunks(FRAME_SIZE) {
        total_frames += 1;

        // Use the new speech boundary detection
        match vad_engine.detect_speech_boundaries(chunk)? {
            SpeechBoundary::Start => {
                is_speech_active = true;
                let processed_audio = spectral_subtraction(chunk, noise)?;
                speech_frames.extend(processed_audio);
                speech_frame_count += 1;
            }
            SpeechBoundary::Continuing if is_speech_active => {
                let processed_audio = spectral_subtraction(chunk, noise)?;
                speech_frames.extend(processed_audio);
                speech_frame_count += 1;
            }
            SpeechBoundary::End => {
                is_speech_active = false;
            }
            SpeechBoundary::Silence => {
                noise = average_noise_spectrum(chunk);
            }
            _ => {}
        }
    }

    let speech_duration_ms = speech_frame_count * 100; // Each frame is 100ms
    let speech_ratio = speech_frame_count as f32 / total_frames as f32;
    let min_speech_ratio = vad_engine.get_min_speech_ratio();

    debug!(
        "device: {}, total audio frames processed: {}, frames that include speech: {}, speech duration: {}ms, speech ratio: {:.2}, min required ratio: {:.2}",
        device,
        total_frames,
        speech_frame_count,
        speech_duration_ms,
        speech_ratio,
        min_speech_ratio
    );

    // If no speech frames detected or speech ratio is too low, return no frames
    if speech_frames.is_empty() || speech_ratio < min_speech_ratio {
        debug!(
            "device: {}, insufficient speech detected (ratio: {:.2}, min required: {:.2}), no speech frames",
            device,
            speech_ratio,
            min_speech_ratio
        );
        Ok(None)
    } else {
        Ok(Some(speech_frames))
    }
}

fn resample(input: &[f32], from_sample_rate: u32, to_sample_rate: u32) -> Result<Vec<f32>> {
    debug!("Resampling audio");
    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };

    let mut resampler = SincFixedIn::<f32>::new(
        to_sample_rate as f64 / from_sample_rate as f64,
        2.0,
        params,
        input.len(),
        1,
    )?;

    let waves_in = vec![input.to_vec()];
    debug!("Performing resampling");
    let waves_out = resampler.process(&waves_in, None)?;
    debug!("Resampling complete");
    Ok(waves_out.into_iter().next().unwrap())
}
