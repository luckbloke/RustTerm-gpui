//! Standard RDP Security (legacy RC4 layer) framing helpers.
//!
//! Local addition documented in `NYATERM.md`: wires the crypto primitives from
//! `ironrdp_pdu::rdp::standard_security` into the connector's PDU framing so
//! every sealed inbound / outbound frame goes through one place.
//!
//! [MS-RDPBCGR] 5.3: when Standard RDP Security with a non-NONE encryption
//! level is negotiated, every PDU on the I/O and message channels carries a
//! `BasicSecurityHeader`; with `SEC_ENCRYPT` the header is followed by an
//! 8-byte MAC and the RC4-encrypted body (the MAC itself travels in the
//! clear).

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use ironrdp_core::{Decode as _, ReadCursor, WriteBuf, encode_vec};
use ironrdp_pdu::mcs::{McsMessage, SendDataRequest};
use ironrdp_pdu::rdp::headers::{BasicSecurityHeader, BasicSecurityHeaderFlags};
use ironrdp_pdu::rdp::standard_security::RdpSecurity;
use ironrdp_pdu::x224::X224;

use crate::{ConnectorError, ConnectorErrorExt as _, ConnectorResult, custom_err};

/// NyaTerm: a Standard RDP Security state shared through `Arc<Mutex>` so the
/// connector slot and every sub-sequence (licensing / activation /
/// finalization) observe ONE mutable state. Passing bare `RdpSecurity` values
/// around forced each holder to clone and eventually dropped somebody's
/// keystream advances, desynchronizing the RC4 streams (captured repeatedly on
/// a real Server 2008 R2). The connector side needs `Send` (the sequences live
/// inside the `Send`-bounded `ClientConnectorState`), hence `Arc<Mutex>`; the
/// session converts this into its own `Rc<RefCell>` handle at the
/// `ConnectionResult` boundary, where the connector is done with the state.
/// See `NYATERM.md`.
pub type SharedSecurity = Option<Arc<Mutex<RdpSecurity>>>;

/// Encodes a sealed PDU body: `BasicSecurityHeader(flags | SEC_ENCRYPT |
/// SEC_SECURE_CHECKSUM)` + MAC + RC4 ciphertext.
///
/// The salted (use-count) MAC variant is always selected on outbound frames,
/// matching the `SEC_SECURE_CHECKSUM` flag set here (FreeRDP only enables it
/// with the `SaltedChecksum` setting; Windows honors the client's choice per
/// [MS-RDPBCGR] 5.3.6).
pub(crate) fn seal_pdu(
    security: &mut RdpSecurity,
    flags: BasicSecurityHeaderFlags,
    body: &[u8],
) -> ConnectorResult<Vec<u8>> {
    let sealed = security
        .seal(body, true)
        .map_err(|error| custom_err!("standard security", error))?;

    let header = encode_vec(&BasicSecurityHeader {
        flags: flags | BasicSecurityHeaderFlags::ENCRYPT | BasicSecurityHeaderFlags::SECURE_CHECKSUM,
    })
    .map_err(ConnectorError::encode)?;

    let mut user_data = header;
    user_data.extend_from_slice(&sealed);
    Ok(user_data)
}

/// Unwraps an inbound PDU body under Standard RDP Security.
///
/// Every I/O / message channel PDU carries a `BasicSecurityHeader` once the
/// security exchange has happened ([MS-RDPBCGR] 5.3); the header is always
/// stripped, and with `SEC_ENCRYPT` the following MAC + ciphertext are
/// decrypted and MAC-checked. The MAC variant (plain vs salted) is selected
/// by the *sender's* `SEC_SECURE_CHECKSUM` header bit. A MAC mismatch is
/// surfaced as a warning but the decrypted data is still returned, matching
/// FreeRDP's tolerance (`rdp_decrypt` in `libfreerdp/core/rdp.c`).
pub(crate) fn unseal_pdu<'a>(
    security: &mut RdpSecurity,
    user_data: &'a [u8],
) -> ConnectorResult<Cow<'a, [u8]>> {
    unseal_pdu_with_flags(security, user_data).map(|(_, _, body)| body)
}

/// Like [`unseal_pdu`], but also returns the inbound `BasicSecurityHeader`
/// flags (for flag-based dispatch) and whether the MAC verified -- which
/// distinguishes a wrong-key derivation (MAC fails) from a wrong RC4 stream or
/// direction (MAC passes but the plaintext is garbage).
///
/// NyaTerm local addition; see `NYATERM.md`.
pub(crate) fn unseal_pdu_with_flags<'a>(
    security: &mut RdpSecurity,
    user_data: &'a [u8],
) -> ConnectorResult<(BasicSecurityHeaderFlags, bool, Cow<'a, [u8]>)> {
    let mut cursor = ReadCursor::new(user_data);
    let header = BasicSecurityHeader::decode(&mut cursor).map_err(ConnectorError::decode)?;
    let rest = cursor.remaining();

    if !header.flags.contains(BasicSecurityHeaderFlags::ENCRYPT) {
        return Ok((header.flags, true, Cow::Borrowed(rest)));
    }

    let unsealed = security
        .unseal(rest, header.flags.contains(BasicSecurityHeaderFlags::SECURE_CHECKSUM))
        .map_err(|error| custom_err!("standard security", error))?;
    if !unsealed.mac_valid {
        // NyaTerm diagnostic: the same fact is surfaced to the file log by the
        // license exchange dispatcher (a windowed GUI launch has no stderr).
        tracing::warn!("Standard RDP Security MAC mismatch on inbound PDU; continuing (FreeRDP-compatible tolerance)");
    }

    Ok((header.flags, unsealed.mac_valid, Cow::Owned(unsealed.plaintext)))
}

/// Encodes an MCS Send Data Request carrying raw (already security-wrapped)
/// user data.
pub(crate) fn encode_send_data_request_raw(
    initiator_id: u16,
    channel_id: u16,
    user_data: Vec<u8>,
    buf: &mut WriteBuf,
) -> ConnectorResult<usize> {
    let pdu = SendDataRequest {
        initiator_id,
        channel_id,
        user_data: Cow::Owned(user_data),
    };

    ironrdp_core::encode_buf(&X224(McsMessage::SendDataRequest(pdu)), buf).map_err(ConnectorError::encode)
}
