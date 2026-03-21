/// Generate a shutter-click WAV sound in memory.
pub fn generate_shutter_wav() -> Vec<u8> {
    let sample_rate: u32 = 44100;
    let duration_ms: u32 = 150;
    let num_samples = (sample_rate * duration_ms / 1000) as usize;
    let bits_per_sample: u16 = 16;
    let num_channels: u16 = 1;
    let byte_rate = sample_rate * num_channels as u32 * bits_per_sample as u32 / 8;
    let block_align = num_channels * bits_per_sample / 8;
    let data_size = (num_samples * 2) as u32;

    let mut wav = Vec::with_capacity(44 + data_size as usize);

    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&num_channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());

    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    // Decaying noise burst using simple LCG PRNG
    let mut rng: u32 = 48271;
    for i in 0..num_samples {
        let t = i as f32 / num_samples as f32;
        let decay = (-t * 10.0).exp();

        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        let noise = ((rng >> 16) as i16 as f32) / 32768.0;

        let sample = (noise * decay * 16000.0) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    wav
}

/// Play a WAV from memory asynchronously using Win32 PlaySound.
pub fn play_shutter(wav_data: &[u8]) {
    const SND_MEMORY: u32 = 0x0004;
    const SND_ASYNC: u32 = 0x0001;

    #[link(name = "winmm")]
    unsafe extern "system" {
        fn PlaySoundW(psz_sound: *const u8, hmod: *mut core::ffi::c_void, fdw_sound: u32) -> i32;
    }

    unsafe {
        PlaySoundW(wav_data.as_ptr(), std::ptr::null_mut(), SND_MEMORY | SND_ASYNC);
    }
}
