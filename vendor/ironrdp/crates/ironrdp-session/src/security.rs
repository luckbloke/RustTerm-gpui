//! Standard RDP Security (legacy RC4 layer) session glue.
//!
//! Local addition documented in `NYATERM.md`. When the connector negotiated
//! Standard RDP Security with a non-NONE encryption level, the session layer
//! must seal every outbound PDU and unseal every inbound one
//! ([MS-RDPBCGR] 5.3). The [`RdpSecurity`] state (keys + continuous RC4
//! streams) is shared between the X.224 slow-path processor and the Fast-Path
//! processor through [`SharedSecurity`], because both directions of both
//! paths advance the same per-direction keystreams.

use std::sync::{Mutex, MutexGuard};
use std::sync::Arc;

use ironrdp_core::{Decode as _, WriteBuf, encode_vec};
use ironrdp_pdu::fast_path::EncryptionFlags;
use ironrdp_pdu::input::fast_path::FastPathInputHeader;
use ironrdp_pdu::mcs::{McsMessage, SendDataRequest};
use ironrdp_pdu::rdp::headers::{BasicSecurityHeader, BasicSecurityHeaderFlags, ShareControlHeader, ShareControlPdu, ShareDataHeader};
use ironrdp_pdu::rdp::standard_security::RdpSecurity;
use ironrdp_pdu::rdp::client_info::CompressionType;
use ironrdp_pdu::rdp::headers::StreamPriority;
use ironrdp_pdu::x224::X224;

use crate::{SessionError, SessionErrorExt as _, SessionResult};

/// Shared, interior-mutable Standard RDP Security state.
///
/// `RefCell` keeps the sealing/unsealing calls possible from `&self` encode
/// paths; the session drives all traffic from a single thread.
pub type SharedSecurity = Option<Arc<Mutex<RdpSecurity>>>;

/// Wraps `security` into the shared handle used by the session processors.
pub fn shared(security: RdpSecurity) -> SharedSecurity {
    Some(Arc::new(Mutex::new(security)))
}

/// Seals one outbound PDU body: `BasicSecurityHeader(flags | SEC_ENCRYPT)` +
/// MAC + RC4 ciphertext.
pub(crate) fn seal_pdu(
    security: &mut RdpSecurity,
    flags: BasicSecurityHeaderFlags,
    body: &[u8],
) -> SessionResult<Vec<u8>> {
    let sealed = security
        .seal(body, true)
        .map_err(|error| crate::reason_err!("standard security", "sealing failed: {error}"))?;

    // The salted (use-count) MAC variant is always selected outbound,
    // matching the SEC_SECURE_CHECKSUM flag set here ([MS-RDPBCGR] 5.3.6).
    let header = encode_vec(&BasicSecurityHeader {
        flags: flags | BasicSecurityHeaderFlags::ENCRYPT | BasicSecurityHeaderFlags::SECURE_CHECKSUM,
    })
    .map_err(SessionError::encode)?;

    let mut user_data = header;
    user_data.extend_from_slice(&sealed);
    Ok(user_data)
}

/// Seals one outbound fast-path input frame body (no `BasicSecurityHeader`):
/// returns MAC + RC4 ciphertext; the caller sets the
/// `FASTPATH_INPUT_ENCRYPTED` flag bit in the leading flags byte.
pub(crate) fn seal_fastpath(security: &mut RdpSecurity, body: &[u8]) -> SessionResult<Vec<u8>> {
    // The salted (use-count) MAC variant matches the
    // FASTPATH_INPUT_SECURE_CHECKSUM bit the caller sets, exactly like
    // FreeRDP's `fastpath_send_multiple_input_pdu`.
    let sealed = security
        .seal(body, true)
        .map_err(|error| crate::reason_err!("standard security", "sealing failed: {error}"))?;
    Ok(sealed)
}

/// Seals one outbound fast-path input PDU (`[MS-RDPBCGR]` 2.2.8.1.2 and
/// 5.3.6, matching FreeRDP's `fastpath_send_multiple_input_pdu`).
///
/// Unlike a slow-path PDU, only the *event data* is sealed: the fast-path
/// header (flags byte + PER length [+ numEvents]) stays in the clear, the
/// 8-byte MAC and the RC4 ciphertext follow it, and the PER length is
/// rewritten to cover the whole PDU. Sealing the header as well, or dropping
/// the length field, desynchronizes the server parser.
pub(crate) fn seal_fastpath_input(security: &SharedSecurity, frame: &[u8]) -> SessionResult<Vec<u8>> {
    let Some(security) = security.as_ref() else {
        return Ok(frame.to_vec());
    };

    let mut cursor = ironrdp_core::ReadCursor::new(frame);
    let header = FastPathInputHeader::decode(&mut cursor).map_err(SessionError::decode)?;
    let data_offset = frame.len() - header.data_length;

    let sealed = {
        let mut guard = security.lock().expect("standard security mutex");
        seal_fastpath(&mut guard, &frame[data_offset..])?
    };

    let encrypted_header = FastPathInputHeader {
        // FASTPATH_INPUT_SECURE_CHECKSUM is set because `seal_fastpath` uses
        // the salted (use-count) MAC variant.
        flags: EncryptionFlags::ENCRYPTED | EncryptionFlags::SECURE_CHECKSUM,
        data_length: sealed.len(),
        num_events: header.num_events,
    };
    let mut encrypted = encode_vec(&encrypted_header).map_err(SessionError::encode)?;
    encrypted.extend_from_slice(&sealed);
    Ok(encrypted)
}

/// Unwraps an inbound fast-path output frame: splits the 8-byte MAC, decrypts
/// the payload and MAC-checks it ([MS-RDPBCGR] 5.3.6).
///
/// `secure_checksum` must reflect the `FASTPATH_OUTPUT_SECURE_CHECKSUM` bit of
/// the received header, since the sender chooses the MAC variant.
pub(crate) fn unseal_fastpath(
    security: &mut RdpSecurity,
    payload: &[u8],
    secure_checksum: bool,
) -> SessionResult<Vec<u8>> {
    let unsealed = security
        .unseal(payload, secure_checksum)
        .map_err(|error| crate::reason_err!("standard security", "unsealing failed: {error}"))?;
    if !unsealed.mac_valid {
        tracing::warn!("Standard RDP Security MAC mismatch on inbound fast-path PDU; continuing");
    }
    Ok(unsealed.plaintext)
}

/// Unwraps an inbound I/O-channel (Share Control) PDU: strips the
/// `BasicSecurityHeader`, decrypts and MAC-checks the body when
/// `SEC_ENCRYPT` is set, and returns the plaintext Share Control stream.
///
/// A MAC mismatch is logged and otherwise tolerated (the decrypted data is
/// still returned), matching FreeRDP's tolerance of broken Standard RDP
/// Security signatures.
pub(crate) fn unseal_io_pdu<'a>(
    security: &mut RdpSecurity,
    user_data: &'a [u8],
) -> SessionResult<std::borrow::Cow<'a, [u8]>> {
    let mut cursor = ironrdp_core::ReadCursor::new(user_data);
    let header = BasicSecurityHeader::decode(&mut cursor).map_err(SessionError::decode)?;
    let rest = cursor.remaining();

    if !header.flags.contains(BasicSecurityHeaderFlags::ENCRYPT) {
        return Ok(std::borrow::Cow::Borrowed(rest));
    }

    let unsealed = security
        .unseal(rest, header.flags.contains(BasicSecurityHeaderFlags::SECURE_CHECKSUM))
        .map_err(|error| crate::reason_err!("standard security", "unsealing failed: {error}"))?;
    if !unsealed.mac_valid {
        tracing::warn!("Standard RDP Security MAC mismatch on inbound PDU; continuing (FreeRDP-compatible tolerance)");
        // NyaTerm diagnostic: file log (a windowed GUI launch has no stderr).
        if let Some(path) = std::env::var_os("NYATERM_RDPDR_LOG") {
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                use std::io::Write as _;
                let _ = writeln!(file, "[session] io PDU MAC mismatch, body_len={}", unsealed.plaintext.len());
            }
        }
    }

    Ok(std::borrow::Cow::Owned(unsealed.plaintext))
}

/// Unwraps an inbound message-channel PDU (Heartbeat / Auto-Detect /
/// Multitransport). These PDUs embed the `BasicSecurityHeader` in their own
/// wire structure, so the returned buffer keeps the header bytes and only
/// replaces the encrypted payload with its plaintext.
pub(crate) fn unseal_message_pdu<'a>(
    security: &mut RdpSecurity,
    user_data: &'a [u8],
) -> SessionResult<std::borrow::Cow<'a, [u8]>> {
    let mut cursor = ironrdp_core::ReadCursor::new(user_data);
    let header = BasicSecurityHeader::decode(&mut cursor).map_err(SessionError::decode)?;

    if !header.flags.contains(BasicSecurityHeaderFlags::ENCRYPT) {
        return Ok(std::borrow::Cow::Borrowed(user_data));
    }

    let header_len = user_data.len() - cursor.remaining().len();
    let rest = cursor.remaining();
    let unsealed = security
        .unseal(rest, header.flags.contains(BasicSecurityHeaderFlags::SECURE_CHECKSUM))
        .map_err(|error| crate::reason_err!("standard security", "unsealing failed: {error}"))?;
    if !unsealed.mac_valid {
        tracing::warn!("Standard RDP Security MAC mismatch on inbound PDU; continuing (FreeRDP-compatible tolerance)");
    }

    let mut out = Vec::with_capacity(header_len + unsealed.plaintext.len());
    out.extend_from_slice(&user_data[..header_len]);
    out.extend_from_slice(&unsealed.plaintext);
    Ok(std::borrow::Cow::Owned(out))
}

/// Encodes a Share Data PDU as a complete (sealed when active) MCS Send Data
/// Request frame.
pub(crate) fn encode_share_data(
    security: &SharedSecurity,
    user_channel_id: u16,
    io_channel_id: u16,
    share_id: u32,
    pdu: ironrdp_pdu::rdp::headers::ShareDataPdu,
    buf: &mut WriteBuf,
) -> SessionResult<usize> {
    let user_data = match security {
        Some(security) => {
            let mut security = security.lock().expect("standard security mutex");
            let body = encode_vec(&ShareControlHeader {
                share_control_pdu: ShareControlPdu::Data(ShareDataHeader {
                    share_data_pdu: pdu,
                    stream_priority: StreamPriority::Medium,
                    compression_flags: ironrdp_pdu::rdp::headers::CompressionFlags::empty(),
                    compression_type: CompressionType::K8,
                }),
                pdu_source: user_channel_id,
                share_id,
            })
            .map_err(SessionError::encode)?;
            seal_pdu(&mut security, BasicSecurityHeaderFlags::empty(), &body)?
        }
        None => {
            // Unencrypted path: reuse the pdu-layer encoder (it wraps the PDU
            // in a Share Control header and an MCS Send Data Request).
            let mut frame = WriteBuf::new();
            ironrdp_pdu::rdp::headers::encode_share_data(user_channel_id, io_channel_id, share_id, pdu, &mut frame)
                .map_err(SessionError::encode)?;
            // The frame is complete; copy through `buf` to keep the caller's
            // contract (returns the written size, appends to `buf`).
            let written = frame.filled().len();
            buf.write_slice(frame.filled());
            return Ok(written);
        }
    };

    let pdu = SendDataRequest {
        initiator_id: user_channel_id,
        channel_id: io_channel_id,
        user_data: std::borrow::Cow::Owned(user_data),
    };

    let written = encode_vec(&X224(McsMessage::SendDataRequest(pdu))).map_err(SessionError::encode)?;
    let size = written.len();
    buf.write_slice(&written);
    Ok(size)
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{ReadCursor, encode_vec};
    use ironrdp_pdu::gcc::EncryptionMethod;
    use ironrdp_pdu::input::MousePdu;
    use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent, KeyboardFlags};
    use ironrdp_pdu::rdp::standard_security::{CLIENT_RANDOM_SIZE, RdpSecurity};

    use super::*;

    fn security() -> RdpSecurity {
        let client_random = [0x11u8; CLIENT_RANDOM_SIZE];
        let server_random = [0x22u8; 32];

        RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::BIT_128)
            .expect("128-bit key derivation")
    }

    /// The wire layout is the contract with the server: clear fast-path header,
    /// then `MAC || ciphertext` over the event data only, with the PER length
    /// covering the whole PDU. (Regression test: sealing `frame[1..]` used to
    /// bury the length field inside the ciphertext.)
    #[test]
    fn fastpath_input_sealing_keeps_the_header_in_the_clear() {
        let input = FastPathInput::new(vec![
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x1E),
            FastPathInputEvent::MouseEvent(MousePdu {
                flags: ironrdp_pdu::input::mouse::PointerFlags::MOVE,
                x_position: 100,
                y_position: 200,
                number_of_wheel_rotation_units: 0,
            }),
        ])
        .expect("two input events");
        let plaintext = encode_vec(&input).expect("encode fast-path input");

        let shared = shared(security());
        let sealed = seal_fastpath_input(&shared, &plaintext).expect("seal fast-path input");

        // The `FastPathInputHeader` decoder rejects a non-empty flags field, so
        // the sealed header is taken apart by hand: flags byte, then the PER
        // length (a set top bit selects the two-byte form).
        let flags = sealed[0];
        assert_eq!(flags & 0xC0, 0xC0, "FASTPATH_INPUT_ENCRYPTED and SECURE_CHECKSUM");
        assert_eq!(flags & 0x3F, plaintext[0] & 0x3F, "action and numEvents stay unchanged");

        let (declared_length, header_len) = if sealed[1] & 0x80 != 0 {
            (usize::from(sealed[1] & 0x7F) << 8 | usize::from(sealed[2]), 3)
        } else {
            (usize::from(sealed[1]), 2)
        };
        assert_eq!(declared_length, sealed.len(), "the PER length covers the ciphertext");

        let mut plain_cursor = ReadCursor::new(&plaintext);
        let plain_header = FastPathInputHeader::decode(&mut plain_cursor).expect("decode header");
        let plain_header_len = plaintext.len() - plain_cursor.remaining().len();
        let plain_events_len = plain_header.data_length;

        assert_eq!(header_len, plain_header_len, "the fast-path header size is unchanged");
        assert_eq!(sealed.len(), header_len + 8 + plain_events_len, "MAC + ciphertext of the events");
        // The event data really is encrypted: the ciphertext after the MAC does
        // not match the plaintext event bytes.
        assert_ne!(
            &sealed[header_len + 8..],
            &plaintext[plain_header_len..],
            "event data must be RC4-encrypted"
        );

        // One outbound packet was consumed from the RC4 stream.
        let shared_guard = shared.as_ref().expect("security state");
        let guard = shared_guard.lock().expect("standard security mutex");
        assert_eq!(guard.use_counts().0, 1);
    }
}