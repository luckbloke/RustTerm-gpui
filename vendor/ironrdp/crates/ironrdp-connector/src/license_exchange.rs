use std::sync::Mutex;
use std::borrow::Cow;
use core::fmt::Debug;
use core::panic::RefUnwindSafe;
use core::{fmt, mem};
use std::str;
use std::sync::Arc;

use ironrdp_core::{WriteBuf, encode_vec};
use ironrdp_pdu::PduHint;
use ironrdp_pdu::rdp::headers::BasicSecurityHeaderFlags;
use ironrdp_pdu::rdp::standard_security::RdpSecurity;
use ironrdp_pdu::rdp::server_license::{self, LicenseInformation, LicensePdu, ServerLicenseError};
use rand::RngCore as _;
use tracing::{debug, error, info, trace, warn};

use super::{ConnectorError, ConnectorErrorExt as _, custom_err, general_err, reason_err};
use crate::standard_security::{SharedSecurity, encode_send_data_request_raw, seal_pdu, unseal_pdu_with_flags};
use crate::{
    ConnectorResult, ConnectorResultExt as _, MonotonicInstant, Sequence, State, Written, encode_send_data_request,
};

#[derive(Default, Debug)]
#[non_exhaustive]
pub enum LicenseExchangeState {
    #[default]
    Consumed,

    NewLicenseRequest,
    PlatformChallenge {
        encryption_data: server_license::LicenseEncryptionData,
    },
    UpgradeLicense {
        encryption_data: server_license::LicenseEncryptionData,
    },
    LicenseExchanged,
}

impl State for LicenseExchangeState {
    fn name(&self) -> &'static str {
        match self {
            Self::Consumed => "Consumed",
            Self::NewLicenseRequest => "NewLicenseRequest",
            Self::PlatformChallenge { .. } => "PlatformChallenge",
            Self::UpgradeLicense { .. } => "UpgradeLicense",
            Self::LicenseExchanged => "LicenseExchanged",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::LicenseExchanged)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// Client licensing sequence
///
/// Implements the state machine described in MS-RDPELE, section [3.1.5.3.1] Client State Transition.
///
/// [3.1.5.3.1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpele/8f9b860a-3687-401d-b3bc-7e9f5d4f7528
#[derive(Debug)]
pub struct LicenseExchangeSequence {
    pub state: LicenseExchangeState,
    pub io_channel_id: u16,
    pub username: String,
    pub domain: Option<String>,
    pub hardware_id: [u32; 4],
    pub license_cache: Arc<dyn LicenseCache>,
    /// Standard RDP Security state, when the connection was established with
    /// the legacy RC4 layer. Inbound licensing PDUs are unsealed and outbound
    /// ones sealed with it (local addition; see `NYATERM.md`).
    pub security: SharedSecurity,
    /// The MCS message channel announced in the GCC server data: licensing
    /// PDUs travel on it (FreeRDP `rdp_recv_message_channel_pdu`). `None`
    /// means the server did not announce one (then any channel is accepted,
    /// as before).
    pub message_channel_id: Option<u16>,
    /// NyaTerm: how many consecutive inbound PDUs did not decode as licensing
    /// traffic. After a few of these the server is clearly speaking a different
    /// protocol than we expect (typically a key-derivation mismatch garbling
    /// the RC4 stream), so the exchange fails with the diagnostic embedded in
    /// the error instead of hanging on `read frame by hint` forever.
    ignored_pdus: usize,
}

// Use RefUnwindSafe so that types that embed LicenseCache remain UnwindSafe
pub trait LicenseCache: Sync + Send + Debug + RefUnwindSafe {
    fn get_license(&self, license_info: LicenseInformation) -> ConnectorResult<Option<Vec<u8>>>;
    fn store_license(&self, license_info: LicenseInformation) -> ConnectorResult<()>;
}

#[derive(Debug)]
pub(crate) struct NoopLicenseCache;

impl LicenseCache for NoopLicenseCache {
    fn get_license(&self, _license_info: LicenseInformation) -> ConnectorResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn store_license(&self, _license_info: LicenseInformation) -> ConnectorResult<()> {
        Ok(())
    }
}

impl LicenseExchangeSequence {
    pub fn new(
        io_channel_id: u16,
        username: String,
        domain: Option<String>,
        hardware_id: [u32; 4],
        license_cache: Arc<dyn LicenseCache>,
        message_channel_id: Option<u16>,
    ) -> Self {
        Self {
            state: LicenseExchangeState::NewLicenseRequest,
            io_channel_id,
            username,
            domain,
            hardware_id,
            license_cache,
            security: None,
            message_channel_id,
            ignored_pdus: 0,
        }
    }

    /// Encodes and sends an outbound licensing PDU, sealing it when Standard
    /// RDP Security is active ([MS-RDPBCGR] 5.3: licensing PDUs on the I/O
    /// channel are encrypted once the security exchange has completed).
    fn encode_license_pdu(
        &mut self,
        initiator_id: u16,
        channel_id: u16,
        pdu: &LicensePdu,
        output: &mut WriteBuf,
    ) -> ConnectorResult<usize> {
        // Clone the Arc handle first: the lock guard then borrows the local
        // handle instead of `self`, keeping `&mut self` methods callable while
        // the guard is alive.
        let security_arc = self.security.clone();
        let mut security = security_arc.as_ref().map(|s| s.lock().expect("standard security mutex"));
        match security.as_deref_mut() {
            Some(mut security) => {
                let encoded = encode_vec(pdu).map_err(ConnectorError::encode)?;
                // The `LicensePdu` encoding embeds its own `BasicSecurityHeader`
                // (the first half of `LicenseHeader`). Under Standard RDP
                // Security that header is the PDU's *outer* framing, which
                // `seal_pdu` re-creates in the clear -- so strip the embedded
                // copy and seal only the preamble + body, the way FreeRDP
                // `license_send` frames the packet. (Same pattern as the
                // Client Info PDU.)
                let body = &encoded[ironrdp_pdu::rdp::headers::BASIC_SECURITY_HEADER_SIZE..];
                let user_data = seal_pdu(security, BasicSecurityHeaderFlags::LICENSE_PKT | BasicSecurityHeaderFlags::LICENSE_ENCRYPT_CS, body)?;
                encode_send_data_request_raw(initiator_id, channel_id, user_data, output)
            }
            None => encode_send_data_request::<LicensePdu>(initiator_id, channel_id, pdu, output),
        }
    }
}

impl Sequence for LicenseExchangeSequence {
    fn next_pdu_hint(&self) -> Option<&dyn PduHint> {
        match self.state {
            LicenseExchangeState::Consumed => None,
            LicenseExchangeState::NewLicenseRequest => Some(&ironrdp_pdu::X224_HINT),
            LicenseExchangeState::PlatformChallenge { .. } => Some(&ironrdp_pdu::X224_HINT),
            LicenseExchangeState::UpgradeLicense { .. } => Some(&ironrdp_pdu::X224_HINT),
            LicenseExchangeState::LicenseExchanged => None,
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
        let send_data_indication_ctx =
            ironrdp_pdu::mcs::decode_send_data_indication(input).map_err(ConnectorError::decode)?;

        // Licensing is not alone on the connection: the server interleaves
        // connect-time auto-detect, heartbeat, multitransport and share-data
        // PDUs with it, on both the I/O and the message channel. FreeRDP tells
        // them apart by the `BasicSecurityHeader` flags
        // (`rdp_recv_message_channel_pdu` dispatches on `SEC_LICENSE_PKT` /
        // `SEC_LICENSE_ENCRYPT_CS` / `SEC_LICENSE_ENCRYPT_SC`), but real
        // Windows Server 2008 R2 does NOT set any licensing flag on its
        // licensing PDUs -- captured on the wire: channel 1003, security flags
        // `SEC_ENCRYPT` only. Auto-detect and heartbeat PDUs, however, always
        // carry their own distinguishing flag, and never decode as an
        // MS-RDPELE PDU.
        //
        // So dispatch on the *content*: unseal the PDU -- every encrypted PDU
        // must pass through the continuous RC4 stream to keep it in sync, even
        // the ones ignored here -- and treat it as licensing traffic only when
        // it decodes as a licensing PDU. Anything else is ignored with the
        // state left untouched.
        let security_arc = self.security.clone();
        let mut security = security_arc.as_ref().map(|s| s.lock().expect("standard security mutex"));
        let (mut last_mac_valid, user_data): (bool, Cow<'_, [u8]>) = match security.as_deref_mut() {
            Some(mut security) => {
                let (flags, mac_valid, body) =
                    unseal_pdu_with_flags(security, send_data_indication_ctx.user_data)?;

                // The decrypted body starts at the license preamble, but
                // `LicensePdu` expects its `LicenseHeader`'s 4-byte security
                // header in front of it -- re-assemble it from the received
                // clear-text header (flagsHi is always zero on the wire).
                let mut framed = Vec::with_capacity(4 + body.len());
                framed.extend_from_slice(&flags.bits().to_le_bytes());
                framed.extend_from_slice(&0u16.to_le_bytes()); // flagsHi, unused
                framed.extend_from_slice(&body);

                // NyaTerm diagnostic: MAC validity separates "wrong key
                // derivation" (mac fails) from "key ok but RC4 stream/direction
                // wrong" (mac passes, plaintext garbage) at a glance.
                if let Some(path) = std::env::var_os("NYATERM_RDPDR_LOG") {
                    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                        use std::io::Write as _;
                        let _ = writeln!(
                            file,
                            "[license] chan={} mac_valid={} body_len={}",
                            send_data_indication_ctx.channel_id,
                            mac_valid,
                            body.len()
                        );
                    }
                }

                (mac_valid, Cow::Owned(framed))
            }
            None => (true, Cow::Borrowed(send_data_indication_ctx.user_data)),
        };

        let license_pdu: LicensePdu = match ironrdp_pdu::decode::<LicensePdu>(user_data.as_ref()) {
            Ok(license_pdu) => {
                self.ignored_pdus = 0;
                license_pdu
            }
            Err(error) => {
                // NyaTerm diagnostic: surface the failure in the connection
                // error itself (a windowed GUI launch has no stderr, and the
                // file log may not make it back to the developer). `mac_valid`
                // separates wrong-key-derivation (mac fails) from a wrong RC4
                // stream/direction (mac passes but the plaintext is garbage).
                drop(security);
                self.ignored_pdus += 1;
                if self.ignored_pdus >= 3 {
                    let mut head = String::new();
                    for byte in user_data.iter().take(user_data.len().min(16)) {
                        head.push_str(&format!("{byte:02X} "));
                    }
                    let _ = &last_mac_valid;
                    return Err(reason_err!(
                        "license exchange",
                        "received {} non-licensing PDUs in a row (the server's Standard RDP Security stream does not decode as MS-RDPELE); last PDU: chan={}, mac_valid={}, body={head}",
                        self.ignored_pdus,
                        send_data_indication_ctx.channel_id,
                        last_mac_valid,
                    ));
                }

                if let Some(path) = std::env::var_os("NYATERM_RDPDR_LOG") {
                    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                        use std::io::Write as _;
                        let mut head = String::new();
                        for byte in user_data.iter().take(user_data.len().min(24)) {
                            head.push_str(&format!("{byte:02X} "));
                        }
                        let _ = writeln!(
                            file,
                            "[license] chan={} not a licensing PDU ({error}) body_len={} head: {head}",
                            send_data_indication_ctx.channel_id,
                            user_data.len()
                        );
                    }
                }

                return Ok(Written::Nothing);
            }
        };

        let (written, next_state) = match mem::take(&mut self.state) {
            LicenseExchangeState::Consumed => {
                return Err(general_err!(
                    "license exchange sequence state is consumed (this is a bug)",
                ));
            }

            LicenseExchangeState::NewLicenseRequest => {
                match license_pdu {
                    LicensePdu::ServerLicenseRequest(license_request) => {
                        let mut rng = rand::rng();
                        let mut client_random = [0u8; server_license::RANDOM_NUMBER_SIZE];
                        rng.fill_bytes(&mut client_random);

                        let mut premaster_secret = [0u8; server_license::PREMASTER_SECRET_SIZE];
                        rng.fill_bytes(&mut premaster_secret);

                        let license_info = license_request
                            .scope_list
                            .iter()
                            .filter_map(|scope| {
                                self.license_cache
                                    .get_license(LicenseInformation {
                                        version: license_request.product_info.version,
                                        scope: scope.0.clone(),
                                        company_name: license_request.product_info.company_name.clone(),
                                        product_id: license_request.product_info.product_id.clone(),
                                        license_info: vec![],
                                    })
                                    .transpose()
                            })
                            .next()
                            .transpose()?;

                        if let Some(info) = license_info {
                            match server_license::ClientLicenseInfo::from_server_license_request(
                                &license_request,
                                &client_random,
                                &premaster_secret,
                                self.hardware_id,
                                info,
                            ) {
                                Ok((client_license_info, encryption_data)) => {
                                    trace!(?encryption_data, "Successfully generated Client License Info");
                                    trace!(message = ?client_license_info, "Send");

                                    let written = self.encode_license_pdu(
                                        send_data_indication_ctx.initiator_id,
                                        send_data_indication_ctx.channel_id,
                                        &client_license_info.into(),
                                        output,
                                    )?;

                                    trace!(?written, "Written ClientLicenseInfo");

                                    (
                                        Written::from_size(written)?,
                                        LicenseExchangeState::PlatformChallenge { encryption_data },
                                    )
                                }
                                Err(err) => {
                                    return Err(custom_err!("ClientNewLicenseRequest", err));
                                }
                            }
                        } else {
                            let hwid = self.hardware_id;
                            match server_license::ClientNewLicenseRequest::from_server_license_request(
                                &license_request,
                                &client_random,
                                &premaster_secret,
                                &self.username,
                                &format!("{:X}-{:X}-{:X}-{:X}", hwid[0], hwid[1], hwid[2], hwid[3]),
                            ) {
                                Ok((new_license_request, encryption_data)) => {
                                    trace!(?encryption_data, "Successfully generated Client New License Request");
                                    trace!(message = ?new_license_request, "Send");

                                    let written = self.encode_license_pdu(
                                        send_data_indication_ctx.initiator_id,
                                        send_data_indication_ctx.channel_id,
                                        &new_license_request.into(),
                                        output,
                                    )?;

                                    (
                                        Written::from_size(written)?,
                                        LicenseExchangeState::PlatformChallenge { encryption_data },
                                    )
                                }
                                Err(error) => {
                                    if let ServerLicenseError::InvalidX509Certificate {
                                        source: error,
                                        cert_der,
                                    } = &error
                                    {
                                        struct BytesHexFormatter<'a>(&'a [u8]);

                                        impl fmt::Display for BytesHexFormatter<'_> {
                                            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                                                write!(f, "0x")?;
                                                self.0.iter().try_for_each(|byte| write!(f, "{byte:02X}"))
                                            }
                                        }

                                        error!(
                                            %error,
                                            cert_der = %BytesHexFormatter(cert_der),
                                            "Unsupported or invalid X509 certificate received during license exchange step"
                                        );
                                    }

                                    return Err(custom_err!("ClientNewLicenseRequest", error));
                                }
                            }
                        }
                    }
                    LicensePdu::LicensingErrorMessage(error_message) => {
                        // FreeRDP `license_client_recv` error-alert semantics:
                        // STATUS_VALID_CLIENT or ST_NO_TRANSITION completes the
                        // exchange, ST_RESEND_LAST_MESSAGE keeps the state (the
                        // server asks us to retry), only ST_TOTAL_ABORT (or an
                        // unknown transition) fails the connection.
                        use server_license::LicensingStateTransition;
                        match error_message.state_transition {
                            LicensingStateTransition::NoTransition
                                if error_message.error_code == server_license::LicenseErrorCode::StatusValidClient =>
                            {
                                info!("Server did not initiate license exchange");
                                (Written::Nothing, LicenseExchangeState::LicenseExchanged)
                            }
                            LicensingStateTransition::NoTransition => {
                                info!("Licensing exchange completed via server error alert");
                                (Written::Nothing, LicenseExchangeState::LicenseExchanged)
                            }
                            LicensingStateTransition::ResendLastMessage => {
                                warn!(code = ?error_message.error_code, "Server asked to resend the last licensing PDU");
                                (Written::Nothing, LicenseExchangeState::NewLicenseRequest)
                            }
                            transition => {
                                let _ = transition;
                                return Err(custom_err!(
                                    "LicensingErrorMessage",
                                    ServerLicenseError::from(error_message)
                                ));
                            }
                        }
                    }
                    _ => {
                        return Err(general_err!(
                            "unexpected PDU received during LicenseExchangeState::NewLicenseRequest"
                        ));
                    }
                }
            }

            LicenseExchangeState::PlatformChallenge { encryption_data } => {
                match license_pdu {
                    LicensePdu::ServerPlatformChallenge(challenge) => {
                        debug!(message = ?challenge, "Received");

                        let challenge_response =
                            server_license::ClientPlatformChallengeResponse::from_server_platform_challenge(
                                &challenge,
                                self.hardware_id,
                                &encryption_data,
                            )
                            .map_err(|e| custom_err!("ClientPlatformChallengeResponse", e))?;

                        debug!(message = ?challenge_response, "Send");

                        let written = self.encode_license_pdu(
                            send_data_indication_ctx.initiator_id,
                            send_data_indication_ctx.channel_id,
                            &challenge_response.into(),
                            output,
                        )?;

                        (
                            Written::from_size(written)?,
                            LicenseExchangeState::UpgradeLicense { encryption_data },
                        )
                    }
                    LicensePdu::LicensingErrorMessage(error_message) => {
                        // Same error-alert semantics as in `NewLicenseRequest`.
                        use server_license::LicensingStateTransition;
                        match error_message.state_transition {
                            LicensingStateTransition::ResendLastMessage => {
                                warn!(code = ?error_message.error_code, "Server asked to resend the last licensing PDU");
                                (
                                    Written::Nothing,
                                    LicenseExchangeState::PlatformChallenge { encryption_data },
                                )
                            }
                            LicensingStateTransition::TotalAbort => {
                                return Err(custom_err!(
                                    "LicensingErrorMessage",
                                    ServerLicenseError::from(error_message)
                                ));
                            }
                            _ => {
                                info!("Client licensing completed via server error alert");
                                (Written::Nothing, LicenseExchangeState::LicenseExchanged)
                            }
                        }
                    }
                    _ => {
                        return Err(general_err!(
                            "unexpected PDU received during LicenseExchangeState::PlatformChallenge"
                        ));
                    }
                }
            }

            LicenseExchangeState::UpgradeLicense { encryption_data } => {
                match license_pdu {
                    LicensePdu::ServerUpgradeLicense(upgrade_license) => {
                        debug!(message = ?upgrade_license, "Received");

                        upgrade_license
                            .verify_server_license(&encryption_data)
                            .map_err(|e| custom_err!("license verification", e))?;

                        debug!("License verified with success");

                        let license_info = upgrade_license
                            .new_license_info(&encryption_data)
                            .map_err(ConnectorError::decode)?;

                        self.license_cache.store_license(license_info)?;

                        info!("Client licensing completed");
                        (Written::Nothing, LicenseExchangeState::LicenseExchanged)
                    }
                    LicensePdu::LicensingErrorMessage(error_message) => {
                        // Same error-alert semantics as in `NewLicenseRequest`.
                        use server_license::LicensingStateTransition;
                        let stay = match error_message.state_transition {
                            LicensingStateTransition::ResendLastMessage => {
                                warn!(code = ?error_message.error_code, "Server asked to resend the last licensing PDU");
                                Some(LicenseExchangeState::UpgradeLicense { encryption_data })
                            }
                            LicensingStateTransition::TotalAbort => {
                                return Err(custom_err!(
                                    "LicensingErrorMessage",
                                    ServerLicenseError::from(error_message)
                                ));
                            }
                            _ => {
                                info!("Client licensing completed via server error alert");
                                None
                            }
                        };

                        match stay {
                            Some(next_state) => (Written::Nothing, next_state),
                            None => (Written::Nothing, LicenseExchangeState::LicenseExchanged),
                        }
                    }
                    _ => {
                        return Err(general_err!(
                            "unexpected PDU received during LicenseExchangeState::UpgradeLicense"
                        ));
                    }
                }
            }

            LicenseExchangeState::LicenseExchanged => return Err(general_err!("license already exchanged")),
        };

        self.state = next_state;

        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_core::encode_vec;
    use ironrdp_pdu::mcs::{McsMessage, SendDataIndication};
    use ironrdp_pdu::x224::X224;

    fn sequence_with_message_channel() -> LicenseExchangeSequence {
        let mut sequence = LicenseExchangeSequence::new(
            1007,
            "user".to_owned(),
            None,
            [0; 4],
            Arc::new(NoopLicenseCache),
            Some(5),
        );
        // Standard RDP Security active: the flag-based dispatch under test only
        // happens on that path. The key material is irrelevant here (the test
        // PDUs are not encrypted).
        sequence.security = Some(Arc::new(Mutex::new(
            RdpSecurity::establish_keys(
                &[0x11; 32],
                &[0x22; 32],
                ironrdp_pdu::gcc::EncryptionMethod::BIT_128,
            )
            .expect("128-bit RC4 keys should derive"),
        )));
        sequence
    }

    fn send_data_indication(channel_id: u16, payload: &[u8]) -> Vec<u8> {
        encode_vec(&X224(McsMessage::SendDataIndication(SendDataIndication {
            initiator_id: 1002,
            channel_id,
            user_data: Cow::Borrowed(payload),
        })))
        .expect("statically valid MCS SendDataIndication should encode")
    }

    /// A `BasicSecurityHeader` carrying `flags` (the header is 4 bytes: flags
    /// plus the unused flagsHi half) followed by `body`.
    fn security_header(flags: BasicSecurityHeaderFlags, body: &[u8]) -> Vec<u8> {
        let mut framed = Vec::with_capacity(4 + body.len());
        framed.extend_from_slice(&flags.bits().to_le_bytes());
        framed.extend_from_slice(&0u16.to_le_bytes()); // flagsHi, unused
        framed.extend_from_slice(body);
        framed
    }

    /// Traffic that is not an MS-RDPELE PDU (auto-detect, heartbeat,
    /// multitransport, share data) is ignored with no reply and no state
    /// change, whatever channel it arrives on and whatever flags it carries.
    #[test]
    fn non_license_pdus_are_ignored() {
        let mut sequence = sequence_with_message_channel();

        // A heartbeat PDU on the message channel and an auto-detect request on
        // the I/O channel: neither is licensing traffic.
        for (channel_id, flags) in [
            (5, BasicSecurityHeaderFlags::HEARTBEAT),
            (1007, BasicSecurityHeaderFlags::AUTODETECT_REQ),
        ] {
            let pdu = send_data_indication(channel_id, &security_header(flags, &[0xAA, 0xBB]));

            let written = sequence
                .step(&pdu, None, &mut WriteBuf::new())
                .expect("a non-licensing PDU should be ignored, not fatal");

            assert_eq!(written.size(), None, "nothing should be written");
            assert!(matches!(sequence.state, LicenseExchangeState::NewLicenseRequest));
        }
    }

    /// After three consecutive non-licensing PDUs the exchange fails with a
    /// diagnostic error instead of hanging forever on the read loop.
    #[test]
    fn repeated_garbage_fails_with_diagnostic() {
        let mut sequence = sequence_with_message_channel();

        for _ in 0..2 {
            let pdu = send_data_indication(1003, &security_header(BasicSecurityHeaderFlags::LICENSE_PKT, &[1, 2, 3]));
            sequence
                .step(&pdu, None, &mut WriteBuf::new())
                .expect("the first two garbage PDUs are ignored");
        }

        let error = sequence
            .step(
                &send_data_indication(1003, &security_header(BasicSecurityHeaderFlags::LICENSE_PKT, &[1, 2, 3])),
                None,
                &mut WriteBuf::new(),
            )
            .expect_err("the third garbage PDU must fail the exchange with a diagnostic");

        let text = error.to_string();
        assert!(text.contains("mac_valid="), "diagnostic should carry mac_valid: {text}");
        assert!(text.contains("chan=1003"), "diagnostic should carry the channel: {text}");
    }

    /// A real licensing PDU is processed even though Windows Server 2008 R2
    /// does not set a licensing security-header flag on it (captured wire data:
    /// channel 1003, `SEC_ENCRYPT` only).
    #[test]
    fn licensing_pdu_is_processed() {
        let mut sequence = sequence_with_message_channel();

        let message = server_license::LicensingErrorMessage::new_valid_client().expect("valid client alert");
        let encoded = encode_vec(&LicensePdu::LicensingErrorMessage(message)).expect("encode license pdu");
        // On the wire the decrypted body starts at the license preamble: the
        // PDU's embedded `BasicSecurityHeader` is the packet's outer framing.
        let body = &encoded[ironrdp_pdu::rdp::headers::BASIC_SECURITY_HEADER_SIZE..];
        // The body is left unencrypted so the test can carry real plaintext;
        // the dispatch under test does not look at the flags.
        let pdu = send_data_indication(1003, &security_header(BasicSecurityHeaderFlags::LICENSE_PKT, body));

        let written = sequence
            .step(&pdu, None, &mut WriteBuf::new())
            .expect("a licensing PDU should be processed");

        assert_eq!(written.size(), None, "the valid-client alert writes nothing");
        assert!(
            matches!(sequence.state, LicenseExchangeState::LicenseExchanged),
            "the server-not-initiating alert ends the exchange"
        );
    }

    /// Regression fixture: the first inbound PDU of the captured Server 2008 R2
    /// Standard RDP Security session. It pins the MCS routing (1003, the I/O
    /// global channel) and the fact that Windows sets no licensing flag here --
    /// which is why dispatch is content-based rather than flag-based.
    #[test]
    fn captured_2008_r2_pdu_header_layout() {
        let mut bytes: Vec<u8> = vec![
            0x03, 0x00, 0x00, 0x20, 0x02, 0xF0, 0x80, 0x68, 0x00, 0x01, 0x03, 0xEB, 0x30, 0x12, 0x08, 0x00,
            0x00, 0x00, 0x89, 0x81, 0xF4, 0x6B, 0x79, 0xA3, 0xB8, 0xCE, 0xF7, 0x9F, 0x4B, 0xDE, 0x51, 0x60,
        ];
        bytes[3] = 0x20; // TPKT length patched to the captured size
        bytes[13] = 0x12; // MCS userData length patched likewise

        let ctx = ironrdp_pdu::mcs::decode_send_data_indication(&bytes).expect("mcs decode");
        let user_data = ctx.user_data;

        assert_eq!(ctx.channel_id, 1003, "licensing traffic arrives on the I/O channel");
        assert_eq!(
            u16::from_le_bytes([user_data[0], user_data[1]]),
            BasicSecurityHeaderFlags::ENCRYPT.bits(),
            "no licensing flag is set on this server's licensing PDUs"
        );
        assert_eq!(u16::from_le_bytes([user_data[2], user_data[3]]), 0, "flagsHi is unused");
    }
}
