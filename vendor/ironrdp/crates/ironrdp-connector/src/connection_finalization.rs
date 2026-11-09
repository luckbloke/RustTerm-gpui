use std::borrow::Cow;
use core::mem;

use ironrdp_core::{WriteBuf, encode_vec};
use ironrdp_pdu::PduHint;
use ironrdp_pdu::rdp::capability_sets::SERVER_CHANNEL_ID;
use ironrdp_pdu::rdp::headers::{BasicSecurityHeaderFlags, ShareDataPdu};
use ironrdp_pdu::rdp::standard_security::RdpSecurity;
use ironrdp_pdu::rdp::{finalization_messages, server_error_info};
use tracing::{debug, warn};

use crate::standard_security::{encode_send_data_request_raw, seal_pdu, unseal_pdu};
use crate::{
    ConnectorError, ConnectorErrorExt as _, ConnectorResult, MonotonicInstant, Sequence, State, Written, general_err,
    reason_err,
};

#[derive(Default, Debug, Copy, Clone)]
#[non_exhaustive]
pub enum ConnectionFinalizationState {
    #[default]
    Consumed,

    SendSynchronize,
    SendControlCooperate,
    SendRequestControl,
    SendFontList,

    WaitForResponse,

    Finished,
}

impl State for ConnectionFinalizationState {
    fn name(&self) -> &'static str {
        match self {
            Self::Consumed => "Consumed",
            Self::SendSynchronize => "SendSynchronize",
            Self::SendControlCooperate => "SendControlCooperate",
            Self::SendRequestControl => "SendRequestControl",
            Self::SendFontList => "SendFontList",
            Self::WaitForResponse => "WaitForResponse",
            Self::Finished => "Finished",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionFinalizationSequence {
    pub state: ConnectionFinalizationState,
    pub io_channel_id: u16,
    pub user_channel_id: u16,
    pub share_id: u32,
    pub monitor_layout: Option<finalization_messages::MonitorLayoutPdu>,
    /// Standard RDP Security state, when the connection was established with
    /// the legacy RC4 layer (local addition; see `NYATERM.md`).
    pub security: crate::standard_security::SharedSecurity,
}

impl ConnectionFinalizationSequence {
    pub fn new(io_channel_id: u16, user_channel_id: u16, share_id: u32) -> Self {
        Self {
            state: ConnectionFinalizationState::SendSynchronize,
            io_channel_id,
            user_channel_id,
            share_id,
            monitor_layout: None,
            security: None,
        }
    }

    /// Carries the negotiated Standard RDP Security state into this sequence.
    pub fn with_security(mut self, security: crate::standard_security::SharedSecurity) -> Self {
        self.security = security;
        self
    }

    /// Encodes one outbound Share Data PDU, sealed when Standard RDP Security
    /// is active ([MS-RDPBCGR] 5.3).
    fn encode_share_data_sealed(&mut self, message: ShareDataPdu, output: &mut WriteBuf) -> ConnectorResult<usize> {
        match self.security.as_ref().map(|s| s.lock().expect("standard security mutex")) {
            Some(mut security) => {
                let body = encode_vec(&ironrdp_pdu::rdp::headers::ShareControlHeader {
                    share_control_pdu: ironrdp_pdu::rdp::headers::ShareControlPdu::Data(
                        ironrdp_pdu::rdp::headers::ShareDataHeader {
                            share_data_pdu: message,
                            stream_priority: ironrdp_pdu::rdp::headers::StreamPriority::Medium,
                            compression_flags: ironrdp_pdu::rdp::headers::CompressionFlags::empty(),
                            compression_type: ironrdp_pdu::rdp::client_info::CompressionType::K8,
                        },
                    ),
                    pdu_source: self.user_channel_id,
                    share_id: self.share_id,
                })
                .map_err(ConnectorError::encode)?;
                let user_data = seal_pdu(&mut security, BasicSecurityHeaderFlags::empty(), &body)?;
                encode_send_data_request_raw(self.user_channel_id, self.io_channel_id, user_data, output)
            }
            None => ironrdp_pdu::rdp::headers::encode_share_data(
                self.user_channel_id,
                self.io_channel_id,
                self.share_id,
                message,
                output,
            )
            .map_err(ConnectorError::encode),
        }
    }
}

impl Sequence for ConnectionFinalizationSequence {
    fn next_pdu_hint(&self) -> Option<&dyn PduHint> {
        match self.state {
            ConnectionFinalizationState::Consumed => None,
            ConnectionFinalizationState::SendSynchronize => None,
            ConnectionFinalizationState::SendControlCooperate => None,
            ConnectionFinalizationState::SendRequestControl => None,
            ConnectionFinalizationState::SendFontList => None,
            ConnectionFinalizationState::WaitForResponse => Some(&ironrdp_pdu::X224_HINT),
            ConnectionFinalizationState::Finished => None,
        }
    }

    fn state(&self) -> &dyn State {
        &self.state
    }

    fn step(
        &mut self,
        input: &[u8],
        _received_at: Option<MonotonicInstant>,
        output: &mut WriteBuf,
    ) -> ConnectorResult<Written> {
        let (written, next_state) = match mem::take(&mut self.state) {
            ConnectionFinalizationState::Consumed => {
                return Err(general_err!(
                    "connection finalization sequence state is consumed (this is a bug)",
                ));
            }

            ConnectionFinalizationState::SendSynchronize => {
                let message = ShareDataPdu::Synchronize(finalization_messages::SynchronizePdu {
                    target_user_id: self.user_channel_id,
                });

                debug!(?message, "Send");

                let written = self.encode_share_data_sealed(message, output)?;

                (
                    Written::from_size(written)?,
                    ConnectionFinalizationState::SendControlCooperate,
                )
            }

            ConnectionFinalizationState::SendControlCooperate => {
                let message = ShareDataPdu::Control(finalization_messages::ControlPdu {
                    action: finalization_messages::ControlAction::Cooperate,
                    grant_id: 0,
                    control_id: 0,
                });

                debug!(?message, "Send");

                let written = self.encode_share_data_sealed(message, output)?;

                (
                    Written::from_size(written)?,
                    ConnectionFinalizationState::SendRequestControl,
                )
            }

            ConnectionFinalizationState::SendRequestControl => {
                let message = ShareDataPdu::Control(finalization_messages::ControlPdu {
                    action: finalization_messages::ControlAction::RequestControl,
                    grant_id: 0,
                    control_id: 0,
                });

                debug!(?message, "Send");

                let written = self.encode_share_data_sealed(message, output)?;

                (Written::from_size(written)?, ConnectionFinalizationState::SendFontList)
            }

            ConnectionFinalizationState::SendFontList => {
                let message = ShareDataPdu::FontList(finalization_messages::FontPdu::default());

                debug!(?message, "Send");

                let written = self.encode_share_data_sealed(message, output)?;

                (
                    Written::from_size(written)?,
                    ConnectionFinalizationState::WaitForResponse,
                )
            }

            ConnectionFinalizationState::WaitForResponse => {
                let raw_ctx = ironrdp_pdu::mcs::decode_send_data_indication(input).map_err(ConnectorError::decode)?;

                // Under Standard RDP Security every inbound I/O-channel PDU carries a
                // BasicSecurityHeader; strip (and decrypt) it when present.
                let user_data: Cow<'_, [u8]> = match self.security.as_ref().map(|s| s.lock().expect("standard security mutex")) {
                    Some(mut security) => unseal_pdu(&mut security, raw_ctx.user_data)?,
                    None => Cow::Borrowed(raw_ctx.user_data),
                };
                let ctx = ironrdp_pdu::rdp::headers::decode_share_data(ironrdp_pdu::mcs::SendDataIndicationCtx {
                    initiator_id: raw_ctx.initiator_id,
                    channel_id: raw_ctx.channel_id,
                    user_data: user_data.as_ref(),
                })
                .map_err(ConnectorError::decode)?;

                debug!(message = ?ctx.pdu, "Received");

                let next_state = match ctx.pdu {
                    ShareDataPdu::Synchronize(_) => {
                        debug!("Server Synchronize");
                        ConnectionFinalizationState::WaitForResponse
                    }
                    ShareDataPdu::Control(control_pdu) => match control_pdu.action {
                        finalization_messages::ControlAction::Cooperate => {
                            if control_pdu.grant_id == 0 && control_pdu.control_id == 0 {
                                debug!("Server Control (Cooperate)");
                            } else {
                                warn!(
                                    control_pdu.grant_id,
                                    control_pdu.control_id,
                                    user_channel_id = self.user_channel_id,
                                    "Server Control (Cooperate) has non-zero grant_id or control_id",
                                );
                            }
                            ConnectionFinalizationState::WaitForResponse
                        }
                        finalization_messages::ControlAction::GrantedControl => {
                            debug!(
                                control_pdu.grant_id,
                                control_pdu.control_id,
                                user_channel_id = self.user_channel_id,
                                SERVER_CHANNEL_ID
                            );

                            if control_pdu.grant_id != self.user_channel_id {
                                warn!(
                                    "Server Control (Granted Control) had invalid grant_id, expected {}, but got {}",
                                    self.user_channel_id, control_pdu.grant_id
                                );
                            }

                            if control_pdu.control_id != u32::from(SERVER_CHANNEL_ID) {
                                warn!(
                                    "Server Control (Granted Control) had invalid control_id, expected {}, but got {}",
                                    SERVER_CHANNEL_ID, control_pdu.control_id
                                );
                            }

                            ConnectionFinalizationState::WaitForResponse
                        }
                        _ => return Err(general_err!("unexpected control action")),
                    },
                    ShareDataPdu::ServerSetErrorInfo(server_error_info::ServerSetErrorInfoPdu(error_info)) => {
                        match error_info {
                            server_error_info::ErrorInfo::ProtocolIndependentCode(
                                server_error_info::ProtocolIndependentCode::None,
                            ) => ConnectionFinalizationState::WaitForResponse,
                            _ => {
                                return Err(reason_err!(
                                    "ServerSetErrorInfo",
                                    "server returned error info: {}",
                                    error_info.description()
                                ));
                            }
                        }
                    }
                    ShareDataPdu::MonitorLayout(monitor_layout) => {
                        self.monitor_layout = Some(monitor_layout);
                        ConnectionFinalizationState::WaitForResponse
                    }
                    ShareDataPdu::FontMap(_) => {
                        // https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/023f1e69-cfe8-4ee6-9ee0-7e759fb4e4ee
                        //
                        // Once the client has sent the Confirm Active PDU, it can start
                        // sending mouse and keyboard input to the server, and upon receipt
                        // of the Font List PDU the server can start sending graphics
                        // output to the client.

                        ConnectionFinalizationState::Finished
                    }
                    _ => return Err(general_err!("unexpected server message")),
                };

                (Written::Nothing, next_state)
            }

            ConnectionFinalizationState::Finished => return Err(general_err!("finalization already finished")),
        };

        self.state = next_state;

        Ok(written)
    }
}
