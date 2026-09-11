//! Reading and writing the stored recording.
//!
//! WAV, uncompressed, because encoding is free and every speech-to-text API accepts it. The
//! size argument that usually counts against it does not apply here: 16 kHz mono is about
//! 2 MB a minute, and these recordings are seconds long.

use std::path::Path;

use hound::{SampleFormat, WavSpec, WavWriter};

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: hound::Error,
    },
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: hound::Error,
    },
}

/// Write mono samples as a 16-bit WAV.
///
/// Returns the file size, which goes into `session.json` so that retention can work on the
/// metadata without stat-ing every recording.
pub fn write_mono(path: &Path, samples: &[f32], sample_rate: u32) -> Result<u64, WavError> {
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut writer = WavWriter::create(path, spec).map_err(|source| WavError::Write {
        path: path.display().to_string(),
        source,
    })?;

    for &sample in samples {
        writer
            .write_sample(to_i16(sample))
            .map_err(|source| WavError::Write {
                path: path.display().to_string(),
                source,
            })?;
    }

    writer.finalize().map_err(|source| WavError::Write {
        path: path.display().to_string(),
        source,
    })?;

    Ok(std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0))
}

/// Read a mono WAV back as floats. Used by tests and by anything replaying a recording.
pub fn read_mono(path: &Path) -> Result<(Vec<f32>, u32), WavError> {
    let mut reader = hound::WavReader::open(path).map_err(|source| WavError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let spec = reader.spec();

    let samples: Result<Vec<f32>, hound::Error> = match spec.sample_format {
        SampleFormat::Int => reader
            .samples::<i16>()
            .map(|sample| sample.map(|s| f32::from(s) / f32::from(i16::MAX)))
            .collect(),
        SampleFormat::Float => reader.samples::<f32>().collect(),
    };

    let samples = samples.map_err(|source| WavError::Read {
        path: path.display().to_string(),
        source,
    })?;

    // Fold any extra channels down, so callers always get mono regardless of the source.
    let mono = crate::resample::to_mono(&samples, spec.channels);
    Ok((mono, spec.sample_rate))
}

/// Convert to 16-bit, clamping rather than wrapping.
///
/// Wrapping would turn a sample slightly over full scale into a loud sample of the opposite
/// sign — an audible click, in the loudest part of the recording, which is exactly where it
/// is most noticeable.
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_survive_a_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("audio.wav");
        let original: Vec<f32> = (0..1_000).map(|n| (n as f32 * 0.05).sin() * 0.8).collect();

        let bytes = write_mono(&path, &original, 16_000).expect("write");
        assert!(
            bytes > 2_000,
            "a 1000-sample file should not be {bytes} bytes"
        );

        let (read_back, rate) = read_mono(&path).expect("read");
        assert_eq!(rate, 16_000);
        assert_eq!(read_back.len(), original.len());
        for (a, b) in read_back.iter().zip(original.iter()) {
            // 16-bit quantisation is the only loss allowed here.
            assert!((a - b).abs() < 1e-4, "{a} != {b}");
        }
    }

    #[test]
    fn samples_beyond_full_scale_are_clamped_not_wrapped() {
        // Wrapping turns a sample slightly too loud into a loud sample of the opposite sign:
        // an audible click, in the loudest part of the recording.
        assert_eq!(to_i16(1.5), i16::MAX);
        assert_eq!(to_i16(-1.5), -i16::MAX);
        assert_eq!(to_i16(0.0), 0);
    }

    #[test]
    fn an_empty_recording_still_produces_a_valid_file() {
        // A cancelled or silent session must not leave a file that nothing can open.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("empty.wav");

        write_mono(&path, &[], 16_000).expect("write");
        let (samples, rate) = read_mono(&path).expect("read");

        assert!(samples.is_empty());
        assert_eq!(rate, 16_000);
    }

    #[test]
    fn the_declared_rate_is_what_comes_back() {
        for rate in [8_000, 16_000, 44_100, 48_000] {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join("r.wav");
            write_mono(&path, &[0.1, 0.2], rate).expect("write");
            assert_eq!(read_mono(&path).expect("read").1, rate);
        }
    }
}
