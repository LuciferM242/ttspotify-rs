use crossbeam_channel::Sender;
use librespot_playback::audio_backend::{Open, Sink, SinkError, SinkResult};
use librespot_playback::config::AudioFormat;
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;

/// Custom Sink that sends PCM i16 audio data through a crossbeam channel
/// to the audio pipeline thread for TeamTalk injection.
pub struct TeamTalkSink {
    sender: Sender<Vec<i16>>,
    /// Set when constructed via the trait's `open()` fallback (no real channel).
    /// Guarantees `write` fails loudly instead of silently discarding audio.
    disconnected: bool,
}

impl TeamTalkSink {
    pub fn new(sender: Sender<Vec<i16>>) -> Self {
        Self {
            sender,
            disconnected: false,
        }
    }
}

impl Open for TeamTalkSink {
    fn open(_device: Option<String>, _format: AudioFormat) -> Self {
        // Required by the trait, but the bot always builds the sink via new() with a
        // real channel from its sink_builder closure. If librespot ever calls this
        // instead, the sink has no channel: flag it so write() returns a clear error
        // rather than dying silently with no audio.
        tracing::error!("TeamTalkSink::open() called directly - this should not happen");
        let (tx, _) = crossbeam_channel::bounded(0);
        Self {
            sender: tx,
            disconnected: true,
        }
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
        if self.disconnected {
            return Err(SinkError::NotConnected(
                "TeamTalkSink has no audio channel (constructed via open() fallback)".to_string(),
            ));
        }
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
