use crossbeam_channel::Sender;
use librespot_playback::audio_backend::{Open, Sink, SinkError, SinkResult};
use librespot_playback::config::AudioFormat;
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;

/// Custom Sink that sends PCM i16 audio data through a crossbeam channel
/// to the audio pipeline thread for TeamTalk injection.
pub struct TeamTalkSink {
    sender: Sender<Vec<i16>>,
}

impl TeamTalkSink {
    pub fn new(sender: Sender<Vec<i16>>) -> Self {
        Self { sender }
    }
}

impl Open for TeamTalkSink {
    fn open(_device: Option<String>, _format: AudioFormat) -> Self {
        // This constructor is required by the trait but we use new() with sender instead.
        // The Player's sink_builder closure will construct via new().
        // Create a dummy sink with a disconnected channel - it will error on write.
        tracing::error!("TeamTalkSink::open() called directly - this should not happen");
        let (tx, _) = crossbeam_channel::bounded(0);
        Self { sender: tx }
    }
}

impl Sink for TeamTalkSink {
    fn start(&mut self) -> SinkResult<()> {
        tracing::debug!("TeamTalkSink started");
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        tracing::debug!("TeamTalkSink stopped");
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        match packet {
            AudioPacket::Samples(samples) => {
                // Convert f64 samples to i16
                let pcm_data = converter.f64_to_s16(&samples);
                self.sender.send(pcm_data).map_err(|e| {
                    SinkError::OnWrite(format!("Failed to send PCM data: {e}"))
                })?;
            }
            AudioPacket::Raw(_) => {
                // Raw passthrough packets are not supported for TeamTalk injection
                tracing::warn!("Received raw audio packet, ignoring (passthrough not supported)");
            }
        }
        Ok(())
    }
}
