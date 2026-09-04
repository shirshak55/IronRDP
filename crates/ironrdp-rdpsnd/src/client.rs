use std::borrow::Cow;

use ironrdp_core::{Decode as _, Encode as _, EncodeResult, ReadCursor, cast_length, impl_as_any};
use ironrdp_dvc::{DvcChannelListener, DvcClientProcessor, DvcEncode, DvcMessage, DvcProcessor, DynamicChannelId};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::{PduResult, encode_err, pdu_other_err};
use ironrdp_svc::{CompressionCondition, SvcClientProcessor, SvcMessage, SvcProcessor};
use tracing::{debug, error, warn};

use crate::pdu::{self, AudioFormat, PitchPdu, ServerAudioFormatPdu, TrainingPdu, VolumePdu};

pub trait RdpsndClientHandler: Send + core::fmt::Debug {
    fn get_flags(&self) -> pdu::AudioFormatFlags {
        pdu::AudioFormatFlags::empty()
    }

    fn get_formats(&self) -> &[AudioFormat];

    /// Play a wave block for the given negotiated format.
    ///
    /// `format` is the entry from the client format list advertised during
    /// negotiation (MS-RDPEA `wFormatNo` indexes that list).
    fn wave(&mut self, format: &AudioFormat, ts: u32, data: Cow<'_, [u8]>);

    fn set_volume(&mut self, volume: VolumePdu);

    fn set_pitch(&mut self, pitch: PitchPdu);

    fn close(&mut self);
}

#[derive(Debug)]
pub struct NoopRdpsndBackend;

impl RdpsndClientHandler for NoopRdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &[]
    }

    fn wave(&mut self, _format: &AudioFormat, _ts: u32, _data: Cow<'_, [u8]>) {}

    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum RdpsndState {
    Start,
    WaitingForTraining,
    Ready,
    /// Waiting for the bare Wave payload after a pre-v8 WaveInfo PDU.
    ExpectingWave,
    Stop,
}

/// Pending pre-v8 WaveInfo fields until the following Wave payload arrives.
#[derive(Debug, Clone)]
struct PendingWave {
    timestamp: u16,
    format_no: u16,
    block_no: u8,
    data_prefix: [u8; 4],
    /// Total audio length including the four-byte prefix (MS-RDPEA `n`).
    audio_length: u16,
}

/// Required for rdpdr to work: [\[MS-RDPEFS\] Appendix A<1>]
///
/// [\[MS-RDPEFS\] Appendix A<1>]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/fd28bfd9-dae2-4a78-abe1-b4efa208b7aa#Appendix_A_1
#[derive(Debug)]
pub struct Rdpsnd {
    handler: Box<dyn RdpsndClientHandler>,
    quality_mode: pdu::QualityMode,
    state: RdpsndState,
    server_format: Option<ServerAudioFormatPdu>,
    /// Formats advertised to the server, in wire order.
    ///
    /// Wave/Wave2 `wFormatNo` indexes this list (MS-RDPEA).
    client_formats: Vec<AudioFormat>,
    pending_wave: Option<PendingWave>,
}

impl Rdpsnd {
    pub const NAME: ChannelName = ChannelName::from_static(b"rdpsnd\0\0");

    pub fn new(handler: Box<dyn RdpsndClientHandler>) -> Self {
        Self {
            handler,
            quality_mode: pdu::QualityMode::High,
            state: RdpsndState::Start,
            server_format: None,
            client_formats: Vec::new(),
            pending_wave: None,
        }
    }

    #[must_use]
    pub fn with_quality_mode(mut self, quality_mode: pdu::QualityMode) -> Self {
        self.quality_mode = quality_mode;
        self
    }

    pub fn get_format(&self, format_no: u16) -> PduResult<&AudioFormat> {
        self.client_formats
            .get(usize::from(format_no))
            .ok_or_else(|| pdu_other_err!("invalid format"))
    }

    pub fn version(&self) -> PduResult<pdu::Version> {
        let server_format = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no version"))?;

        Ok(server_format.version)
    }

    fn client_formats(&mut self) -> PduResult<pdu::ClientAudioOutputPdu> {
        // Windows seems to be confused if the client replies with more formats, or unknown formats (e.g.: opus).
        // Keep only formats also offered by the server, preserving handler order so
        // wFormatNo stays a stable index into this reply list.
        let server_formats = &self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no server format"))?
            .formats;

        let formats: Vec<AudioFormat> = self
            .handler
            .get_formats()
            .iter()
            .filter(|client_fmt| {
                server_formats
                    .iter()
                    .any(|server_fmt| client_fmt.matches_for_negotiation(server_fmt))
            })
            .cloned()
            .collect();

        self.client_formats = formats.clone();

        let pdu = pdu::ClientAudioFormatPdu {
            version: self.version()?,
            flags: self.handler.get_flags() | pdu::AudioFormatFlags::ALIVE,
            formats,
            volume_left: 0xFFFF,
            volume_right: 0xFFFF,
            pitch: 0x00010000,
            dgram_port: 0,
        };
        Ok(pdu::ClientAudioOutputPdu::AudioFormat(pdu))
    }

    fn quality_mode(&mut self) -> pdu::ClientAudioOutputPdu {
        pdu::ClientAudioOutputPdu::QualityMode(pdu::QualityModePdu {
            quality_mode: self.quality_mode,
        })
    }

    fn training_confirm(pdu: &TrainingPdu) -> PduResult<pdu::ClientAudioOutputPdu> {
        let pack_size: EncodeResult<_> = cast_length!("wPackSize", pdu.data.len());
        let pack_size = pack_size.map_err(|e| encode_err!(e))?;
        Ok(pdu::ClientAudioOutputPdu::TrainingConfirm(pdu::TrainingConfirmPdu {
            timestamp: pdu.timestamp,
            pack_size,
        }))
    }

    fn wave_confirm(timestamp: u16, block_no: u8) -> pdu::ClientAudioOutputPdu {
        pdu::ClientAudioOutputPdu::WaveConfirm(pdu::WaveConfirmPdu { timestamp, block_no })
    }

    fn play_wave(&mut self, format_no: u16, ts: u32, data: Cow<'_, [u8]>) {
        let Self {
            handler,
            client_formats,
            ..
        } = self;
        match client_formats.get(usize::from(format_no)) {
            Some(format) => handler.wave(format, ts, data),
            None => {
                warn!(
                    format_no,
                    n_formats = client_formats.len(),
                    "Ignoring wave with out-of-range format_no"
                );
            }
        }
    }

    fn begin_format_negotiation(&mut self, af: ServerAudioFormatPdu) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
        self.handler.close();
        self.server_format = Some(af);
        self.client_formats.clear();
        self.pending_wave = None;
        self.state = RdpsndState::WaitingForTraining;
        let mut msgs = vec![self.client_formats()?];
        if self.version()? >= pdu::Version::V6 {
            msgs.push(self.quality_mode());
        }
        Ok(msgs)
    }

    /// Complete a pre-v8 Wave transfer from WaveInfo + bare Wave payload bytes.
    fn finish_pending_wave(&mut self, wave_payload: &[u8]) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
        let Some(pending) = self.pending_wave.take() else {
            warn!("Received Wave payload without a pending WaveInfo");
            self.state = RdpsndState::Ready;
            return Ok(vec![]);
        };

        self.state = RdpsndState::Ready;

        // Wave payload is bPad[4] + remaining audio after the WaveInfo Data prefix.
        // Total wire length equals WaveInfo audio_length (`n`).
        let expected_len = usize::from(pending.audio_length);
        if wave_payload.len() >= expected_len {
            let remaining = &wave_payload[4..expected_len];
            let mut data = Vec::with_capacity(expected_len);
            data.extend_from_slice(&pending.data_prefix);
            data.extend_from_slice(remaining);

            let ts = u32::from(pending.timestamp);
            self.play_wave(pending.format_no, ts, data.into());
        } else {
            // MS-RDPEA §3.2.5.2.1.6: still confirm so servers can advance latency accounting.
            warn!(
                got = wave_payload.len(),
                expected = expected_len,
                "Wave payload shorter than WaveInfo audio length; confirming without playback"
            );
        }

        Ok(vec![Self::wave_confirm(pending.timestamp, pending.block_no)])
    }

    /// Drive the MS-RDPEA client state machine with one server SVC/DVC payload.
    ///
    /// The replies are transport-neutral ([`pdu::ClientAudioOutputPdu`]) so the
    /// static `rdpsnd` channel and the `AUDIO_PLAYBACK_DVC` dynamic channel share
    /// one implementation.
    fn react(&mut self, payload: &[u8]) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
        // Pre-v8 Wave data has no RDPSND header (MS-RDPEA §2.2.3.4).
        if self.state == RdpsndState::ExpectingWave {
            debug!(len = payload.len(), "Completing pending WaveInfo with Wave payload");
            return self.finish_pending_wave(payload);
        }

        let pdu = match pdu::ServerAudioOutputPdu::decode(&mut ReadCursor::new(payload)) {
            Ok(pdu) => pdu,
            Err(error) => {
                error!(?error, "Ignoring malformed RDPSND PDU");
                return Ok(vec![]);
            }
        };

        debug!(?pdu, ?self.state);
        let msg = match self.state {
            RdpsndState::Start => {
                let pdu::ServerAudioOutputPdu::AudioFormat(af) = pdu else {
                    error!("Invalid pdu");
                    self.state = RdpsndState::Stop;
                    return Ok(vec![]);
                };
                self.begin_format_negotiation(af)?
            }
            RdpsndState::WaitingForTraining => {
                let pdu::ServerAudioOutputPdu::Training(pdu) = pdu else {
                    error!("Invalid PDU");
                    self.state = RdpsndState::Stop;
                    return Ok(vec![]);
                };
                self.state = RdpsndState::Ready;
                vec![Self::training_confirm(&pdu)?]
            }
            RdpsndState::Ready => {
                match pdu {
                    pdu::ServerAudioOutputPdu::Wave(pdu) => {
                        // MS-RDPEA §2.2.3.3: WaveInfo only; next SVC message is bare Wave (§2.2.3.4).
                        // Some test stacks may concatenate Wave after WaveInfo in one buffer —
                        // finish immediately when trailing bytes are present.
                        self.pending_wave = Some(PendingWave {
                            timestamp: pdu.timestamp,
                            format_no: pdu.format_no,
                            block_no: pdu.block_no,
                            data_prefix: pdu.data_prefix,
                            audio_length: pdu.audio_length,
                        });
                        self.state = RdpsndState::ExpectingWave;

                        // SNDPROLOG (4) + WaveInfo body; trailing bytes mean a concatenated Wave payload.
                        let header_and_info = 4usize.checked_add(pdu.size()).expect("header_and_info never overflows");
                        if payload.len() > header_and_info {
                            return self.finish_pending_wave(&payload[header_and_info..]);
                        }
                        return Ok(vec![]);
                    }
                    pdu::ServerAudioOutputPdu::Wave2(pdu) => {
                        self.play_wave(pdu.format_no, pdu.audio_timestamp, pdu.data);
                        return Ok(vec![Self::wave_confirm(pdu.timestamp, pdu.block_no)]);
                    }
                    pdu::ServerAudioOutputPdu::Volume(pdu) => {
                        self.handler.set_volume(pdu);
                    }
                    pdu::ServerAudioOutputPdu::Pitch(pdu) => {
                        self.handler.set_pitch(pdu);
                    }
                    pdu::ServerAudioOutputPdu::Close => {
                        self.handler.close();
                    }
                    pdu::ServerAudioOutputPdu::Training(pdu) => return Ok(vec![Self::training_confirm(&pdu)?]),
                    pdu::ServerAudioOutputPdu::AudioFormat(af) => {
                        return self.begin_format_negotiation(af);
                    }
                    // Unsupported optional PDUs: keep the channel alive.
                    pdu::ServerAudioOutputPdu::CryptKey(_) | pdu::ServerAudioOutputPdu::WaveEncrypt(_) => {
                        warn!(?pdu, "Ignoring unsupported RDPSND PDU");
                    }
                }
                vec![]
            }
            state => {
                error!(?state, "Invalid state");
                vec![]
            }
        };

        Ok(msg)
    }
}

impl_as_any!(Rdpsnd);

impl SvcProcessor for Rdpsnd {
    fn channel_name(&self) -> ChannelName {
        Self::NAME
    }

    fn compression_condition(&self) -> CompressionCondition {
        CompressionCondition::Never
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        Ok(self.react(payload)?.into_iter().map(Into::into).collect())
    }
}

/// The audio output client on its dynamic virtual channel (`AUDIO_PLAYBACK_DVC`).
///
/// Modern Windows servers stream audio over this DVC whenever the client
/// supports `drdynvc`, leaving the static `rdpsnd` channel silent, so a client
/// that wants audio should register both transports.
pub struct RdpsndDvcClient {
    inner: Rdpsnd,
}

impl RdpsndDvcClient {
    pub const CHANNEL_NAME: &'static str = "AUDIO_PLAYBACK_DVC";

    pub fn new(handler: Box<dyn RdpsndClientHandler>) -> Self {
        Self {
            inner: Rdpsnd::new(handler),
        }
    }
}

impl_as_any!(RdpsndDvcClient);

impl DvcProcessor for RdpsndDvcClient {
    fn channel_name(&self) -> &str {
        Self::CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // The server initiates by sending its audio formats.
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        Ok(self
            .inner
            .react(payload)?
            .into_iter()
            .map(|pdu| -> DvcMessage { Box::new(pdu) })
            .collect())
    }

    fn close(&mut self, _channel_id: u32) {
        self.inner.handler.close();
    }
}

impl DvcClientProcessor for RdpsndDvcClient {}

impl DvcEncode for pdu::ClientAudioOutputPdu {}

/// Listener for `AUDIO_PLAYBACK_DVC` that accepts every (re)creation of the
/// channel with a fresh state machine.
///
/// Windows routinely closes and reopens the audio DVC within one session
/// (e.g. around logon), so a one-shot registration is not enough: after the
/// first close, further create requests would be refused and audio would stay
/// silent for the rest of the session.
pub struct RdpsndDvcListener {
    handler_factory: Box<dyn Fn() -> Box<dyn RdpsndClientHandler> + Send>,
}

impl RdpsndDvcListener {
    pub fn new(handler_factory: impl Fn() -> Box<dyn RdpsndClientHandler> + Send + 'static) -> Self {
        Self {
            handler_factory: Box::new(handler_factory),
        }
    }
}

impl DvcChannelListener for RdpsndDvcListener {
    fn channel_name(&self) -> &str {
        RdpsndDvcClient::CHANNEL_NAME
    }

    fn create(&mut self, _channel_id: DynamicChannelId) -> Option<Box<dyn DvcClientProcessor>> {
        Some(Box::new(RdpsndDvcClient::new((self.handler_factory)())))
    }
}

impl Drop for Rdpsnd {
    fn drop(&mut self) {
        self.handler.close();
    }
}

impl SvcClientProcessor for Rdpsnd {}
