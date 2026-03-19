use teamtalk::Client;
use teamtalk::client::ffi;

/// Inject a PCM audio block into the TeamTalk mixer.
pub fn inject_audio_block(
    client: &Client,
    samples: &[i16],
    sample_rate: i32,
    channels: i32,
    stream_id: i32,
    sample_index: u32,
) -> bool {
    let mut block: ffi::AudioBlock = unsafe { std::mem::zeroed() };
    block.nStreamID = stream_id;
    block.nSampleRate = sample_rate;
    block.nChannels = channels;
    block.lpRawAudio = samples.as_ptr() as *mut std::ffi::c_void;
    block.nSamples = (samples.len() / channels as usize) as i32;
    block.uSampleIndex = sample_index;
    block.uStreamTypes = ffi::StreamType::STREAMTYPE_VOICE as u32;

    client.insert_audio_block(&block)
}

/// Flush/clear the audio stream by inserting a NULL audio block.
/// Call this on stop/pause to immediately silence the stream.
pub fn flush_audio(client: &Client) {
    let mut block: ffi::AudioBlock = unsafe { std::mem::zeroed() };
    block.nStreamID = 0;
    block.nSampleRate = 48000;
    block.nChannels = 2;
    block.lpRawAudio = std::ptr::null_mut();
    block.nSamples = 0;
    block.uSampleIndex = 0;
    block.uStreamTypes = ffi::StreamType::STREAMTYPE_VOICE as u32;
    client.insert_audio_block(&block);
}
