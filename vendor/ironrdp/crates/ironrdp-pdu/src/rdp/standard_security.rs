//! Standard RDP Security (the legacy RC4-based security layer).
//!
//! Implements the primitives required by the client side of the Standard RDP
//! Security connection sequence:
//!
//! - Session key derivation ([MS-RDPBCGR] 5.3.4 and 5.3.5)
//! - MAC computation ([MS-RDPBCGR] 5.3.6)
//! - Continuous per-direction RC4 streams with the 4096-packet key update rule
//!   ([MS-RDPBCGR] 5.3.5, "RC4 keys MUST be updated after 4096 packets")
//! - The CS_SECURITY_EXCHANGE PDU ([MS-RDPBCGR] 2.2.1.4.3.1.2)
//! - Decoding of the proprietary server certificate that carries the RSA
//!   public key used to protect the client random ([MS-RDPBCGR] 2.2.1.4.3.1.1)
//!
//! The hash constructions follow the reference implementation in FreeRDP
//! (`libfreerdp/core/security.c`), which is known to interoperate with
//! Windows servers; [MS-RDPBCGR] itself only describes the operations
//! textually.
//!
//! [MS-RDPBCGR]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/517a5529-9a99-42c8-83b5-8e5caa74eb6d

use core::fmt;

use ironrdp_core::{
    Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, cast_length, ensure_fixed_part_size,
    ensure_size, invalid_field_err, read_padding,
};

use crate::crypto::rc4::Rc4;
use crate::gcc::EncryptionMethod;

use des::cipher::{Block, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use des::TdesEde3;

const PAD1: [u8; 40] = [0x36; 40]; // MS-RDPBCGR 5.3.6 pad1
const PAD2: [u8; 48] = [0x5C; 48]; // MS-RDPBCGR 5.3.6 pad2

/// Salt prefix applied to the first bytes of each key with reduced-strength
/// encryption levels ([MS-RDPBCGR] 5.3.5: "SALT".
/// FreeRDP `security.c` uses `{ 0xD1, 0x26, 0x9E }`).
const SALT_40BIT: [u8; 3] = [0xD1, 0x26, 0x9E];

/// RC4 keys MUST be re-derived after this many packets per direction
/// ([MS-RDPBCGR] 5.3.5, Key Update).
const KEY_UPDATE_PACKET_THRESHOLD: u32 = 4096;

pub const CLIENT_RANDOM_SIZE: usize = 32;
const SERVER_RANDOM_SIZE: usize = 32;
const PRE_MASTER_SECRET_SIZE: usize = 48;
const MASTER_SECRET_SIZE: usize = 48;
const SESSION_KEY_BLOB_SIZE: usize = 48;
const MAC_SIZE: usize = 8;

const SIGNATURE_ALG_RSA: u32 = 0x0000_0001;
const KEY_EXCHANGE_ALG_RSA: u32 = 0x0000_0001;
const BB_RSA_KEY_BLOB: u16 = 0x0006;
const BB_RSA_SIGNATURE_BLOB: u16 = 0x0008;

const RSA_MAGIC: [u8; 4] = *b"RSA1";
const RSA_KEY_TRAILING_PADDING: usize = 8;
const RSA_SIGNATURE_BLOB_LEN: u16 = 72;

/// An error produced by Standard RDP Security key derivation or MAC handling.
#[derive(Debug)]
pub struct StandardSecurityError {
    description: String,
}

impl StandardSecurityError {
    fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
        }
    }
}

impl fmt::Display for StandardSecurityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description)
    }
}

impl core::error::Error for StandardSecurityError {}

/// SaltedHash(Salt, Input, Salt1, Salt2) = MD5(Salt + SHA1(Input + Salt + Salt1 + Salt2))
///
/// [MS-RDPBCGR] 5.3.4. The reference implementation always feeds a 48-byte
/// `Salt` here.
fn salted_hash(salt: &[u8; 48], input: &[u8], salt1: &[u8], salt2: &[u8]) -> [u8; 16] {
    use md5::Digest as _;
    use sha1::Digest as _;

    let mut sha1 = sha1::Sha1::new();
    sha1.update(input);
    sha1.update(salt);
    sha1.update(salt1);
    sha1.update(salt2);
    let sha1_digest = sha1.finalize();

    let mut md5 = md5::Md5::new();
    md5.update(salt);
    md5.update(sha1_digest);
    md5.finalize().into()
}

/// PremasterHash(i) = SaltedHash(PreMasterSecret, i, ClientRandom, ServerRandom)
///
/// Also used for the `MasterHash` steps of the key derivation, whose salt order
/// is *not* swapped: FreeRDP's live path (`security_X` in
/// `security_establish_keys`) passes the master secret as this function's salt
/// with the same `(ClientRandom, ServerRandom)` order.
fn premaster_hash(
    pre_master_secret: &[u8; 48],
    input: &[u8],
    client_random: &[u8],
    server_random: &[u8],
) -> [u8; 16] {
    salted_hash(pre_master_secret, input, client_random, server_random)
}

/// MACSignature = First64Bits(MD5(MacKeyN + pad2 + SHA1(MacKeyN + pad1 + length + data)))
///
/// [MS-RDPBCGR] 5.3.6. `mac_key` is truncated to the negotiated RC4 key length
/// by the caller (FreeRDP feeds exactly `rc4_key_len` bytes).
pub(crate) fn mac_signature(mac_key: &[u8], data: &[u8]) -> [u8; MAC_SIZE] {
    use md5::Digest as _;
    use sha1::Digest as _;

    let length = u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes();

    let mut sha1 = sha1::Sha1::new();
    sha1.update(mac_key);
    sha1.update(PAD1);
    sha1.update(length);
    sha1.update(data);
    let sha1_digest = sha1.finalize();

    let mut md5 = md5::Md5::new();
    md5.update(mac_key);
    md5.update(PAD2);
    md5.update(sha1_digest);

    let digest: [u8; 16] = md5.finalize().into();
    let mut signature = [0u8; MAC_SIZE];
    signature.copy_from_slice(&digest[..MAC_SIZE]);
    signature
}

/// Salted variant of the MAC ([MS-RDPBCGR] 5.3.6, "Salted MAC signature";
/// FreeRDP `security_salted_mac_signature`): the 32-bit packet use count is
/// appended to the SHA-1 input.
///
/// For outbound PDUs the encrypt use count is fed as-is; for inbound PDUs the
/// decrypt use count **minus one** is fed (the MAC covers the plaintext, and
/// the count has already been incremented by the time the MAC is checked).
pub(crate) fn salted_mac_signature(mac_key: &[u8], data: &[u8], use_count: u32) -> [u8; MAC_SIZE] {
    use md5::Digest as _;
    use sha1::Digest as _;

    let length = u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes();
    let use_count = use_count.to_le_bytes();

    let mut sha1 = sha1::Sha1::new();
    sha1.update(mac_key);
    sha1.update(PAD1);
    sha1.update(length);
    sha1.update(data);
    sha1.update(use_count);
    let sha1_digest = sha1.finalize();

    let mut md5 = md5::Md5::new();
    md5.update(mac_key);
    md5.update(PAD2);
    md5.update(sha1_digest);

    let digest: [u8; 16] = md5.finalize().into();
    let mut signature = [0u8; MAC_SIZE];
    signature.copy_from_slice(&digest[..MAC_SIZE]);
    signature
}

/// FIPS-mode MAC ([MS-RDPBCGR] 5.3.6): first 8 bytes of
/// HMAC-SHA1(FipsSignKey, data + use_count_le32).
///
/// Hand-rolled HMAC (ipad/opad with the FIPS 20-byte sign key) to avoid a
/// `hmac` version mismatch with the workspace's `sha1`/`digest` versions.
fn hmac_sha1_fips(key: &[u8; 20], data: &[u8], use_count: u32) -> [u8; 20] {
    use sha1::Digest as _;

    const BLOCK_SIZE: usize = 64;

    let mut ipad = [0x36u8; BLOCK_SIZE];
    for (block, key_byte) in ipad.iter_mut().zip(key.iter()) {
        *block ^= key_byte;
    }
    let mut opad = [0x5Cu8; BLOCK_SIZE];
    for (block, key_byte) in opad.iter_mut().zip(key.iter()) {
        *block ^= key_byte;
    }

    let mut inner = sha1::Sha1::new();
    inner.update(ipad);
    inner.update(data);
    inner.update(use_count.to_le_bytes());
    let inner_digest = inner.finalize();

    let mut outer = sha1::Sha1::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// The FIPS-140-1 mode of Standard RDP Security ([MS-RDPBCGR] 5.3.5.1): HMAC
/// keyed with the FIPS sign key, 3DES-CBC (zero IV, continuous stream across
/// PDUs), data padded to the 8-byte block size by the sender.
#[derive(Debug, Clone)]
struct FipsState {
    sign_key: [u8; 20],
    encrypt_cipher: DesCbc,
    decrypt_cipher: DesCbc,
}

/// Continuous 3DES-CBC stream with a zero IV: the CBC chain state (the last
/// ciphertext block) carries across PDUs, exactly like FreeRDP's long-lived
/// `winpr_Cipher` contexts (`security_fips_encrypt` / `security_fips_decrypt`).
#[derive(Debug, Clone)]
struct DesCbc {
    cipher: TdesEde3,
    /// Encrypt direction: IV for the next call (last ciphertext block).
    /// Decrypt direction: last ciphertext block seen.
    chain: [u8; 8],
}

impl DesCbc {
    fn new(key: &[u8; 24]) -> Result<Self, StandardSecurityError> {
        let cipher = TdesEde3::new_from_slice(key)
            .map_err(|error| StandardSecurityError::new(format!("invalid 3DES key: {error}")))?;
        Ok(Self {
            cipher,
            chain: [0u8; 8], // zero IV
        })
    }

    fn encrypt(&mut self, data: &mut [u8]) {
        for block in data.chunks_exact_mut(8) {
            for (byte, chain_byte) in block.iter_mut().zip(self.chain.iter()) {
                *byte ^= chain_byte;
            }
            let mut block_array = Block::<TdesEde3>::default();
            block_array.copy_from_slice(block);
            self.cipher.encrypt_block(&mut block_array);
            block.copy_from_slice(&block_array);
            self.chain.copy_from_slice(block);
        }
    }

    fn decrypt(&mut self, data: &mut [u8]) {
        for block in data.chunks_exact_mut(8) {
            let mut block_array = Block::<TdesEde3>::default();
            block_array.copy_from_slice(block);
            self.cipher.decrypt_block(&mut block_array);
            for (byte, chain_byte) in block_array.iter_mut().zip(self.chain.iter()) {
                *byte ^= chain_byte;
            }
            self.chain.copy_from_slice(block);
            block.copy_from_slice(&block_array);
        }
    }
}
/// First256Bits(SessionKeyBlob) as the MAC key, per [MS-RDPBCGR] 5.3.5.
fn mac_salt_key(session_key_blob: &[u8; SESSION_KEY_BLOB_SIZE]) -> [u8; 16] {
    let mut key = [0u8; 16];
    key.copy_from_slice(&session_key_blob[..16]);
    key
}

/// Bit-reversal table used by `fips_expand_key_bits` (FreeRDP
/// `security.c::fips_reverse_table`).
const FIPS_REVERSE_TABLE: [u8; 256] = [
    0x00, 0x80, 0x40, 0xc0, 0x20, 0xa0, 0x60, 0xe0, 0x10, 0x90, 0x50, 0xd0, 0x30, 0xb0, 0x70, 0xf0, 0x08, 0x88, 0x48,
    0xc8, 0x28, 0xa8, 0x68, 0xe8, 0x18, 0x98, 0x58, 0xd8, 0x38, 0xb8, 0x78, 0xf8, 0x04, 0x84, 0x44, 0xc4, 0x24, 0xa4,
    0x64, 0xe4, 0x14, 0x94, 0x54, 0xd4, 0x34, 0xb4, 0x74, 0xf4, 0x0c, 0x8c, 0x4c, 0xcc, 0x2c, 0xac, 0x6c, 0xec, 0x1c,
    0x9c, 0x5c, 0xdc, 0x3c, 0xbc, 0x7c, 0xfc, 0x02, 0x82, 0x42, 0xc2, 0x22, 0xa2, 0x62, 0xe2, 0x12, 0x92, 0x52, 0xd2,
    0x32, 0xb2, 0x72, 0xf2, 0x0a, 0x8a, 0x4a, 0xca, 0x2a, 0xaa, 0x6a, 0xea, 0x1a, 0x9a, 0x5a, 0xda, 0x3a, 0xba, 0x7a,
    0xfa, 0x06, 0x86, 0x46, 0xc6, 0x26, 0xa6, 0x66, 0xe6, 0x16, 0x96, 0x56, 0xd6, 0x36, 0xb6, 0x76, 0xf6, 0x0e, 0x8e,
    0x4e, 0xce, 0x2e, 0xae, 0x6e, 0xee, 0x1e, 0x9e, 0x5e, 0xde, 0x3e, 0xbe, 0x7e, 0xfe, 0x01, 0x81, 0x41, 0xc1, 0x21,
    0xa1, 0x61, 0xe1, 0x11, 0x91, 0x51, 0xd1, 0x31, 0xb1, 0x71, 0xf1, 0x09, 0x89, 0x49, 0xc9, 0x29, 0xa9, 0x69, 0xe9,
    0x19, 0x99, 0x59, 0xd9, 0x39, 0xb9, 0x79, 0xf9, 0x05, 0x85, 0x45, 0xc5, 0x25, 0xa5, 0x65, 0xe5, 0x15, 0x95, 0x55,
    0xd5, 0x35, 0xb5, 0x75, 0xf5, 0x0d, 0x8d, 0x4d, 0xcd, 0x2d, 0xad, 0x6d, 0xed, 0x1d, 0x9d, 0x5d, 0xdd, 0x3d, 0xbd,
    0x7d, 0xfd, 0x03, 0x83, 0x43, 0xc3, 0x23, 0xa3, 0x63, 0xe3, 0x13, 0x93, 0x53, 0xd3, 0x33, 0xb3, 0x73, 0xf3, 0x0b,
    0x8b, 0x4b, 0xcb, 0x2b, 0xab, 0x6b, 0xeb, 0x1b, 0x9a, 0x5a, 0xda, 0x3a, 0xba, 0x7a, 0xfa, 0x07, 0x87, 0x47, 0xc7,
    0x27, 0xa7, 0x67, 0xe7, 0x17, 0x97, 0x57, 0xd7, 0x37, 0xb7, 0x77, 0xf7, 0x0f, 0x8f, 0x4f, 0xcf, 0x2f, 0xaf, 0x6f,
    0xef, 0x1f, 0x9f, 0x5f, 0xdf, 0x3f, 0xbf, 0x7f, 0xff,
];

/// Odd-parity adjustment table used by `fips_expand_key_bits` (FreeRDP
/// `security.c::fips_oddparity_table`).
const FIPS_ODDPARITY_TABLE: [u8; 256] = [
    0x01, 0x01, 0x02, 0x02, 0x04, 0x04, 0x07, 0x07, 0x08, 0x08, 0x0b, 0x0b, 0x0d, 0x0d, 0x0e, 0x0e, 0x10, 0x10, 0x13,
    0x13, 0x15, 0x15, 0x16, 0x16, 0x19, 0x19, 0x1a, 0x1a, 0x1c, 0x1c, 0x1f, 0x1f, 0x20, 0x20, 0x23, 0x23, 0x25, 0x25,
    0x26, 0x26, 0x29, 0x29, 0x2a, 0x2a, 0x2c, 0x2c, 0x2f, 0x2f, 0x31, 0x31, 0x32, 0x32, 0x34, 0x34, 0x37, 0x37, 0x38,
    0x38, 0x3b, 0x3b, 0x3d, 0x3d, 0x3e, 0x3e, 0x40, 0x40, 0x43, 0x43, 0x45, 0x45, 0x46, 0x46, 0x49, 0x49, 0x4a, 0x4a,
    0x4c, 0x4c, 0x4f, 0x4f, 0x51, 0x51, 0x52, 0x52, 0x54, 0x54, 0x57, 0x57, 0x58, 0x58, 0x5b, 0x5b, 0x5d, 0x5d, 0x5e,
    0x5e, 0x61, 0x61, 0x62, 0x62, 0x64, 0x64, 0x67, 0x67, 0x68, 0x68, 0x6b, 0x6b, 0x6d, 0x6d, 0x6e, 0x6e, 0x70, 0x70,
    0x73, 0x73, 0x75, 0x75, 0x76, 0x76, 0x79, 0x79, 0x7a, 0x7a, 0x7c, 0x7c, 0x7f, 0x7f, 0x80, 0x80, 0x83, 0x83, 0x85,
    0x85, 0x86, 0x86, 0x89, 0x89, 0x8a, 0x8a, 0x8c, 0x8c, 0x8f, 0x8f, 0x91, 0x91, 0x92, 0x92, 0x94, 0x94, 0x97, 0x97,
    0x98, 0x98, 0x9b, 0x9b, 0x9d, 0x9d, 0x9e, 0x9e, 0xa1, 0xa1, 0xa2, 0xa2, 0xa4, 0xa4, 0xa7, 0xa7, 0xa8, 0xa8, 0xab,
    0xab, 0xad, 0xad, 0xae, 0xae, 0xb0, 0xb0, 0xb3, 0xb3, 0xb5, 0xb5, 0xb6, 0xb6, 0xb9, 0xb9, 0xba, 0xba, 0xbc, 0xbc,
    0xbf, 0xbf, 0xc1, 0xc1, 0xc2, 0xc2, 0xc4, 0xc4, 0xc7, 0xc7, 0xc8, 0xc8, 0xcb, 0xcb, 0xcd, 0xcd, 0xce, 0xce, 0xd0,
    0xd0, 0xd3, 0xd3, 0xd5, 0xd5, 0xd6, 0xd6, 0xd9, 0xd9, 0xda, 0xda, 0xdc, 0xdc, 0xdf, 0xdf, 0xe0, 0xe0, 0xe3, 0xe3,
    0xe5, 0xe5, 0xe6, 0xe6, 0xe9, 0xe9, 0xea, 0xea, 0xec, 0xec, 0xef, 0xef, 0xf1, 0xf1, 0xf2, 0xf2, 0xf4, 0xf4, 0xf7,
    0xf7, 0xf8, 0xf8, 0xfb, 0xfb, 0xfd, 0xfd, 0xfe, 0xfe,
];

/// Expands a 21-byte SHA-1-derived buffer into a 24-byte odd-parity 3DES key
/// ([MS-RDPBCGR] 5.3.5.1; FreeRDP `fips_expand_key_bits`): reverse every byte,
/// insert a zero bit after every 7th bit while expanding to 24 bytes, then
/// restore byte order and force odd parity.
fn fips_expand_key_bits(input: &[u8; 21]) -> [u8; 24] {
    let mut buf = [0u8; 21];
    for (index, byte) in input.iter().enumerate() {
        buf[index] = FIPS_REVERSE_TABLE[*byte as usize];
    }

    let mut out = [0u8; 24];
    let mut bit = 0usize;
    for out_byte in out.iter_mut() {
        let position = bit / 8;
        let remainder = bit % 8;
        if remainder <= 1 {
            *out_byte = (buf[position] << remainder) & 0xfe;
        } else {
            let mut accumulator = (buf[position] << remainder) & 0xff;
            accumulator |= buf[position + 1] >> (8 - remainder);
            *out_byte = accumulator & 0xfe;
        }
        bit += 7;
    }

    for out_byte in out.iter_mut() {
        *out_byte = FIPS_ODDPARITY_TABLE[FIPS_REVERSE_TABLE[*out_byte as usize] as usize];
    }
    out
}

/// The whole Standard RDP Security state for one session: the three derived
/// keys plus the continuous RC4 stream for each direction.
///
/// Client-side key mapping ([MS-RDPBCGR] 5.3.5):
/// - the *client encrypt* key seals everything the client sends;
/// - the *client decrypt* key unseals everything the server sends.
#[derive(Debug, Clone)]
pub struct RdpSecurity {
    sign_key: [u8; 16],
    decrypt_key: [u8; 16],
    encrypt_key: [u8; 16],
    decrypt_update_key: [u8; 16],
    encrypt_update_key: [u8; 16],
    /// Effective RC4 key length: 16 for 128-bit, 8 for 40/56-bit.
    rc4_key_len: usize,
    rc4_encrypt: Rc4,
    rc4_decrypt: Rc4,
    encrypt_use_count: u32,
    decrypt_use_count: u32,
    /// FIPS-140-1 mode state; `None` for the RC4 modes. In FIPS mode the RC4
    /// fields and the 40/56-bit salt do not apply.
    fips: Option<FipsState>,
}

impl RdpSecurity {
    /// Derives the session keys from both peers' randoms and builds the
    /// initial RC4 streams ([MS-RDPBCGR] 5.3.4 and 5.3.5), or the FIPS-140-1
    /// 3DES/HMAC-SHA1 state for `ENCRYPTION_METHOD_FIPS` (5.3.5.1; FreeRDP
    /// `security_establish_keys` FIPS branch).
    ///
    /// `encryption_method` is the single method the server selected in its
    /// GCC security data.
    pub fn establish_keys(
        client_random: &[u8; CLIENT_RANDOM_SIZE],
        server_random: &[u8; SERVER_RANDOM_SIZE],
        encryption_method: EncryptionMethod,
    ) -> Result<Self, StandardSecurityError> {
        // PreMasterSecret = ClientRandom[0..24] + ServerRandom[0..24] (5.3.4.1)
        let mut pre_master_secret = [0u8; PRE_MASTER_SECRET_SIZE];
        pre_master_secret[..24].copy_from_slice(&client_random[..24]);
        pre_master_secret[24..].copy_from_slice(&server_random[..24]);

        // Common derivation: MasterSecret from 'A'/'BB'/'CCC' premaster
        // hashes, the MD5-expanded encrypt/decrypt keys, and a sign key from
        // the session key blob. In FIPS mode the RC4/MD5 keys are still
        // derived (FreeRDP does the same) but are not used for encryption.
        let mut master_secret = [0u8; MASTER_SECRET_SIZE];
        master_secret[..16].copy_from_slice(&premaster_hash(&pre_master_secret, b"A", client_random, server_random));
        master_secret[16..32].copy_from_slice(&premaster_hash(&pre_master_secret, b"BB", client_random, server_random));
        master_secret[32..].copy_from_slice(&premaster_hash(&pre_master_secret, b"CCC", client_random, server_random));

        // SessionKeyBlob = SaltedHash(MasterSecret, 'X'/'YY'/'ZZZ', ClientRandom,
        // ServerRandom) -- no salt-order swap. This is the *live* FreeRDP path
        // (`security_X` in `security_establish_keys`); the A/BB/CCC variant in
        // [MS-RDPBCGR] 5.3.5's wording lives in `security_session_key_blob` /
        // `security_master_hash`, which FreeRDP never calls and which does not
        // interoperate. Using it here derives a wrong MacKey and wrong RC4 keys,
        // so every server PDU decrypts to garbage.
        let mut session_key_blob = [0u8; SESSION_KEY_BLOB_SIZE];
        session_key_blob[..16].copy_from_slice(&premaster_hash(&master_secret, b"X", client_random, server_random));
        session_key_blob[16..32].copy_from_slice(&premaster_hash(&master_secret, b"YY", client_random, server_random));
        session_key_blob[32..].copy_from_slice(&premaster_hash(&master_secret, b"ZZZ", client_random, server_random));

        let (rc4_key_len, short_key_prefix, fips) =
            if encryption_method.contains(EncryptionMethod::FIPS) {
            // FIPS-140-1: 3DES/HMAC-SHA1 (5.3.5.1). The RC4-level salts do not
            // apply (rc4_key_len is set to 16 to keep the "full key" meaning).
            // client_encrypt_key_t = SHA1(ClientRandom[16..32] + ServerRandom[16..32])
            // client_decrypt_key_t = SHA1(ClientRandom[0..16] + ServerRandom[0..16])
            // each 20-byte digest gets digest[0] appended as byte 21.
            let mut encrypt_key_t = [0u8; 21];
            {
                use sha1::Digest as _;
                let mut sha1 = sha1::Sha1::new();
                sha1.update(&client_random[16..32]);
                sha1.update(&server_random[16..32]);
                let digest: [u8; 20] = sha1.finalize().into();
                encrypt_key_t[..20].copy_from_slice(&digest);
                encrypt_key_t[20] = digest[0];
            }
            let mut decrypt_key_t = [0u8; 21];
            {
                use sha1::Digest as _;
                let mut sha1 = sha1::Sha1::new();
                sha1.update(&client_random[0..16]);
                sha1.update(&server_random[0..16]);
                let digest: [u8; 20] = sha1.finalize().into();
                decrypt_key_t[..20].copy_from_slice(&digest);
                decrypt_key_t[20] = digest[0];
            }

            // fips_hmac_key = SHA1(decrypt_key_t[0..20] + encrypt_key_t[0..20])
            // (20 bytes; distinct from the 16-byte MacSaltKey sign_key above)
            let mut fips_hmac_key = [0u8; 20];
            {
                use sha1::Digest as _;
                let mut sha1 = sha1::Sha1::new();
                sha1.update(&decrypt_key_t[..20]);
                sha1.update(&encrypt_key_t[..20]);
                fips_hmac_key.copy_from_slice(&sha1.finalize());
            }

            // Client mode: encrypt key from client_encrypt_key_t, decrypt key
            // from client_decrypt_key_t (server mode swaps them).
            let fips_encrypt_key = fips_expand_key_bits(&encrypt_key_t);
            let fips_decrypt_key = fips_expand_key_bits(&decrypt_key_t);

            let fips = FipsState {
                sign_key: fips_hmac_key,
                encrypt_cipher: DesCbc::new(&fips_encrypt_key)?,
                decrypt_cipher: DesCbc::new(&fips_decrypt_key)?,
            };

            (16, &SALT_40BIT[0..0], Some(fips))
        } else if encryption_method.contains(EncryptionMethod::BIT_128) {
            (16, &SALT_40BIT[0..0], None)
        } else if encryption_method.contains(EncryptionMethod::BIT_56) {
            (8, &SALT_40BIT[0..1], None)
        } else if encryption_method.contains(EncryptionMethod::BIT_40) {
            (8, &SALT_40BIT[0..3], None)
        } else {
            return Err(StandardSecurityError::new("no encryption method was negotiated"));
        };

        // 5.3.5: MacKey = first 128 bits of the SessionKeyBlob; the client
        // decrypt key comes from blob[16..32] and the client encrypt key from
        // blob[32..48], each expanded through MD5(blob-portion + both randoms).
        // FIPS mode keeps the same MacSaltKey (FreeRDP copies
        // `session_key_blob[0..16]` into `sign_key` for every mode, L716); its
        // 20-byte HMAC key lives in the FipsState and is independent.
        let mut sign_key = mac_salt_key(&session_key_blob);
        let mut decrypt_key = md5_16_32_32(&session_key_blob[16..32], client_random, server_random);
        let mut encrypt_key = md5_16_32_32(&session_key_blob[32..48], client_random, server_random);

        // Reduced-strength levels overwrite the first bytes of every key with
        // the SALT constant (FreeRDP `security_establish_keys`).
        sign_key[..short_key_prefix.len()].copy_from_slice(short_key_prefix);
        decrypt_key[..short_key_prefix.len()].copy_from_slice(short_key_prefix);
        encrypt_key[..short_key_prefix.len()].copy_from_slice(short_key_prefix);

        Ok(Self {
            rc4_encrypt: Rc4::new(&encrypt_key[..rc4_key_len]),
            rc4_decrypt: Rc4::new(&decrypt_key[..rc4_key_len]),
            decrypt_update_key: decrypt_key,
            encrypt_update_key: encrypt_key,
            sign_key,
            decrypt_key,
            encrypt_key,
            rc4_key_len,
            encrypt_use_count: 0,
            decrypt_use_count: 0,
            fips,
        })
    }

    /// Effective RC4 key length in bytes (16 in FIPS mode).
    pub fn rc4_key_len(&self) -> usize {
        self.rc4_key_len
    }

    /// Whether the FIPS-140-1 mode (3DES + HMAC-SHA1) is active.
    pub fn is_fips(&self) -> bool {
        self.fips.is_some()
    }

    /// NyaTerm diagnostic: current per-direction packet use counts (the RC4
    /// re-key happens at 4096 in each direction, so divergent counts between
    /// the peers show up here first).
    pub fn use_counts(&self) -> (u32, u32) {
        (self.encrypt_use_count, self.decrypt_use_count)
    }
    /// Seals one outbound PDU body ([MS-RDPBCGR] 5.3.5/5.3.6) and returns the
    /// bytes that follow the `BasicSecurityHeader` carrying `SEC_ENCRYPT`:
    ///
    /// - RC4 modes: `MAC(8) || ciphertext` 闂?the MAC travels in the clear
    ///   (FreeRDP `rdp_security_stream_out`);
    /// - FIPS mode: `length(0x10) || version(0x01) || pad || MAC(8) ||
    ///   3DES-CBC(data || zero-padding)` 闂?the body is padded to the 8-byte
    ///   block size by the sender (FreeRDP FIPS branch).
    ///
    /// `secure_checksum` selects the salted MAC variant (append the packet
    /// use count to the digest); outbound frames should set it together with
    /// the `SEC_SECURE_CHECKSUM` header flag.
    ///
    /// The keystream/cipher chain is continuous across PDUs and re-keyed per
    /// 5.3.5 once 4096 packets have been sent in this direction (RC4 modes).
    pub fn seal(&mut self, plaintext: &[u8], secure_checksum: bool) -> Result<Vec<u8>, StandardSecurityError> {
        let use_count = self.encrypt_use_count;

        if let Some(fips) = self.fips.as_mut() {
            // FIPS: pad to the block size, HMAC over the *unpadded* data.
            let pad = (8 - (plaintext.len() % 8)) % 8;
            let mut padded = Vec::with_capacity(plaintext.len() + pad);
            padded.extend_from_slice(plaintext);
            padded.resize(plaintext.len() + pad, 0);

            let mac = hmac_sha1_fips(&fips.sign_key, plaintext, use_count)[..MAC_SIZE].to_vec();
            fips.encrypt_cipher.encrypt(&mut padded);

            let mut out = Vec::with_capacity(4 + MAC_SIZE + padded.len());
            out.extend_from_slice(&0x10u16.to_le_bytes()); // TSFIPS_VERSION length
            out.push(0x01); // TSFIPS_VERSION 1
            out.push(pad as u8);
            out.extend_from_slice(&mac);
            out.extend_from_slice(&padded);
            self.encrypt_use_count += 1;
            return Ok(out);
        }

        if self.encrypt_use_count >= KEY_UPDATE_PACKET_THRESHOLD {
            self.encrypt_key = update_key(&self.encrypt_key, &self.encrypt_update_key, self.rc4_key_len)?;
            self.rc4_encrypt = Rc4::new(&self.encrypt_key[..self.rc4_key_len]);
            self.encrypt_use_count = 0;
        }

        let mac = if secure_checksum {
            salted_mac_signature(&self.sign_key[..self.rc4_key_len], plaintext, use_count)
        } else {
            mac_signature(&self.sign_key[..self.rc4_key_len], plaintext)
        };
        let ciphertext = self.rc4_encrypt.process(plaintext);
        self.encrypt_use_count += 1;

        let mut out = Vec::with_capacity(MAC_SIZE + ciphertext.len());
        out.extend_from_slice(&mac);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Unseals one inbound PDU: `payload` is everything that follows the
    /// `BasicSecurityHeader` (both RC4 and FIPS layouts are handled). Returns
    /// the plaintext together with whether the MAC matched.
    ///
    /// `secure_checksum` must reflect the `SEC_SECURE_CHECKSUM` bit of the
    /// received header (the inbound MAC variant is chosen by the *sender*).
    ///
    /// Like FreeRDP (`rdp_decrypt`), a MAC mismatch does not stop the
    /// decryption: Standard RDP Security is cryptographically broken by design
    /// (same rationale as FreeRDP's comment), so the caller may keep the
    /// session alive and only surface the mismatch. The returned data is
    /// always the decrypted body; the stream state always advances so the
    /// connection stays synchronized.
    pub fn unseal(&mut self, payload: &[u8], secure_checksum: bool) -> Result<Unsealed, StandardSecurityError> {
        if let Some(fips) = self.fips.as_mut() {
            // FIPS layout: length(2) + version(1) + pad(1) + MAC(8) + cipher.
            if payload.len() < 12 {
                return Err(StandardSecurityError::new(format!(
                    "FIPS payload of {} bytes is shorter than the fixed 12-byte prefix",
                    payload.len(),
                )));
            }
            let pad = payload[3] as usize;
            let mac = &payload[4..12];
            let ciphertext = &payload[12..];
            if ciphertext.len() % 8 != 0 || ciphertext.len() < pad {
                return Err(StandardSecurityError::new(format!(
                    "invalid FIPS ciphertext length {} (pad {pad})",
                    ciphertext.len(),
                )));
            }

            let mut padded = ciphertext.to_vec();
            fips.decrypt_cipher.decrypt(&mut padded);
            let length = payload.len() - 12 - pad;
            padded.truncate(length);
            self.decrypt_use_count += 1;

            let expected = hmac_sha1_fips(&fips.sign_key, &padded, self.decrypt_use_count - 1)[..MAC_SIZE].to_vec();
            let mac_valid = mac == expected;

            return Ok(Unsealed {
                plaintext: padded,
                mac_valid,
            });
        }

        if payload.len() < MAC_SIZE {
            return Err(StandardSecurityError::new(format!(
                "sealed payload of {} bytes is shorter than the {MAC_SIZE}-byte MAC",
                payload.len(),
            )));
        }
        let (mac, ciphertext) = payload.split_at(MAC_SIZE);

        if self.decrypt_use_count >= KEY_UPDATE_PACKET_THRESHOLD {
            self.decrypt_key = update_key(&self.decrypt_key, &self.decrypt_update_key, self.rc4_key_len)?;
            self.rc4_decrypt = Rc4::new(&self.decrypt_key[..self.rc4_key_len]);
            self.decrypt_use_count = 0;
        }

        let plaintext = self.rc4_decrypt.process(ciphertext);
        self.decrypt_use_count += 1;

        let expected = if secure_checksum {
            // The MAC covers the plaintext, and the decrypt use count has
            // already been incremented above (FreeRDP decrements its separate
            // checksum counter; ours is the same counter).
            salted_mac_signature(&self.sign_key[..self.rc4_key_len], &plaintext, self.decrypt_use_count - 1)
        } else {
            mac_signature(&self.sign_key[..self.rc4_key_len], &plaintext)
        };
        let mac_valid = mac == expected;

        Ok(Unsealed { plaintext, mac_valid })
    }
}

/// MD5(in0[0..16] + in1[0..32] + in2[0..32]) -- FreeRDP `security_md5_16_32_32`.
fn md5_16_32_32(in0: &[u8], in1: &[u8], in2: &[u8]) -> [u8; 16] {
    use md5::Digest as _;

    let mut md5 = md5::Md5::new();
    md5.update(&in0[..16]);
    md5.update(&in1[..32]);
    md5.update(&in2[..32]);
    md5.finalize().into()
}

/// The result of [`RdpSecurity::unseal`].
#[derive(Debug, Clone)]
pub struct Unsealed {
    /// The decrypted PDU body.
    pub plaintext: Vec<u8>,
    /// Whether the received MAC matched the computed one. `false` means the
    /// PDU was tampered with (or the keys desynchronized); callers may keep
    /// going, matching FreeRDP's tolerance of broken signatures.
    pub mac_valid: bool,
}

/// Per-direction key update after 4096 packets ([MS-RDPBCGR] 5.3.5, Key
/// Update; FreeRDP `security_key_update`).
///
/// `rc4` must be a *fresh* RC4 keyed with the current key so the call is
/// side-effect free w.r.t. the traffic stream; the caller reinitializes its
/// traffic stream afterwards.
fn update_key(key: &[u8; 16], update_key: &[u8; 16], key_len: usize) -> Result<[u8; 16], StandardSecurityError> {
    use md5::Digest as _;
    use sha1::Digest as _;

    // Level salt: 40-bit restores the 3-byte prefix, 56-bit the 1-byte one.
    let salt_len = if key_len == 8 {
        // Distinguish 40 vs 56 by the current prefix content.
        if key[..3] == SALT_40BIT {
            3
        } else {
            1
        }
    } else {
        0
    };

    let mut sha1 = sha1::Sha1::new();
    sha1.update(&update_key[..key_len]);
    sha1.update(PAD1);
    sha1.update(&key[..key_len]);
    let sha1_digest = sha1.finalize();

    let mut md5 = md5::Md5::new();
    md5.update(&update_key[..key_len]);
    md5.update(PAD2);
    md5.update(sha1_digest);
    let mut new_key: [u8; 16] = md5.finalize().into();

    // RC4-encrypt the key with itself.
    let ciphered = Rc4::new(&new_key[..key_len]).process(&new_key[..key_len]);
    new_key[..key_len].copy_from_slice(&ciphered);

    if salt_len > 0 {
        new_key[..salt_len].copy_from_slice(&SALT_40BIT[..salt_len]);
    }

    Ok(new_key)
}

/// CS_SECURITY_EXCHANGE PDU ([MS-RDPBCGR] 2.2.1.4.3.1.2).
///
/// The client sends this right after the MCS channel join phase when the
/// negotiated security protocol is PROTOCOL_RDP with a non-NONE encryption
/// level. The encrypted client random is produced by RSA-encrypting the
/// 32-byte client random with the server certificate's public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityExchangePdu {
    /// `lengthOfSecurityExchange` bytes: the RSA-encrypted client random
    /// followed by 8 bytes of zero padding (per [MS-RDPBCGR] 2.2.1.4.3.1.2).
    pub encrypted_client_random: Vec<u8>,
}

impl SecurityExchangePdu {
    const NAME: &'static str = "SecurityExchangePdu";

    const FIXED_PART_SIZE: usize = 4; // lengthOfSecurityExchange
}

impl Encode for SecurityExchangePdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: self.size());

        dst.write_u32(cast_length!(
            "length_of_security_exchange",
            self.encrypted_client_random.len(), in: dst)?);
        dst.write_slice(&self.encrypted_client_random);

        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE + self.encrypted_client_random.len()
    }
}

impl<'de> Decode<'de> for SecurityExchangePdu {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);

        let length = cast_length!("length_of_security_exchange", src.read_u32(), in: src)?;
        ensure_size!(in: src, size: length);

        let encrypted_client_random = src.read_slice(length).to_vec();

        Ok(Self {
            encrypted_client_random,
        })
    }
}

/// RSA public key carried by a proprietary server certificate
/// ([MS-RDPBCGR] 2.2.1.4.3.1.1.1, RSA_PUBLIC_KEY).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsaPublicKey {
    /// The RSA modulus, big-endian, without leading zero byte.
    pub modulus: Vec<u8>,
    /// The RSA public exponent (in practice always 65537).
    pub exponent: u32,
}

impl RsaPublicKey {
    /// RSA modulus length in bytes (the RSA ciphertext length).
    pub fn modulus_len(&self) -> usize {
        self.modulus.len()
    }
}

/// A decoded proprietary server certificate ([MS-RDPBCGR] 2.2.1.4.3.1.1).
///
/// The 72-byte RSA signature the server embeds in the certificate is parsed
/// and skipped, not verified: validating it requires the well-known Terminal
/// Server Signing Key and only authenticates the certificate chain, not the
/// session (Standard RDP Security has no server authentication on the wire
/// anyway -- the security exchange is done over an unencrypted transport).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProprietaryServerCertificate {
    pub version: u32,
    pub sig_alg_id: u32,
    pub key_alg_id: u32,
    pub public_key: RsaPublicKey,
}

/// CERT_CHAIN_VERSION_1: proprietary certificate ([MS-RDPBCGR] 2.2.1.4.3.1).
const CERT_CHAIN_VERSION_1: u32 = 0x0000_0001;
/// CERT_CHAIN_VERSION_MASK ([MS-RDPBCGR] 2.2.1.4.3.1).
const CERT_CHAIN_VERSION_MASK: u32 = 0x0000_0007;

impl<'de> Decode<'de> for ProprietaryServerCertificate {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        // [MS-RDPBCGR] 2.2.1.4.3.1: the blob carries NO length prefix -- the
        // GCC ServerSecurityData block already bounds it -- and starts directly
        // with dwVersion (4 bytes), whose low bits select the format.
        //
        // NyaTerm: an earlier revision of this decoder read a phantom leading
        // length field followed by a phantom dwVersionLen; real servers start
        // the blob at dwVersion (0x00000001 for Server 2008 R2), so the first
        // four bytes were consumed as a length and the version block check
        // failed with "received 1 bytes, expected 16 bytes".
        let version = src.read_u32();
        if version & CERT_CHAIN_VERSION_MASK != CERT_CHAIN_VERSION_1 {
            return Err(invalid_field_err!(
                "dwVersion",
                "unsupported certificate chain version",
                in: src
            ));
        }
        let sig_alg_id = src.read_u32();
        let key_alg_id = src.read_u32();

        if sig_alg_id != SIGNATURE_ALG_RSA || key_alg_id != KEY_EXCHANGE_ALG_RSA {
            return Err(invalid_field_err!("dwKeyAlgId", "unsupported certificate algorithms", in: src));
        }

        let public_key_blob_type = src.read_u16();
        if public_key_blob_type != BB_RSA_KEY_BLOB {
            return Err(invalid_field_err!("wPublicKeyBlobType", "unsupported public key blob type", in: src));
        }
        let public_key_blob_len: usize =
            cast_length!("wPublicKeyBlobLen", src.read_u16(), in: src)?;
        ensure_size!(in: src, size: public_key_blob_len);
        let mut key_src = ReadCursor::new(src.read_slice(public_key_blob_len));

        // RSA_PUBLIC_KEY ([MS-RDPBCGR] 2.2.1.4.3.1.1.1)
        let magic: [u8; 4] = key_src.read_array();
        if magic != RSA_MAGIC {
            return Err(invalid_field_err!("magic", "invalid RSA public key magic", in: key_src));
        }
        let _key_len = key_src.read_u32(); // bitlen/8 + 8
        let bit_len = key_src.read_u32();
        let _data_len = key_src.read_u32(); // bitlen/8 - 1
        let exponent = key_src.read_u32();
        let modulus_len = usize::try_from(bit_len / 8).map_err(|_| invalid_field_err!("bitLen", "invalid RSA modulus bit length", in: key_src))?;
        ensure_size!(in: key_src, size: modulus_len);
        let modulus = key_src.read_slice(modulus_len).to_vec();
        read_padding!(&mut key_src, RSA_KEY_TRAILING_PADDING);

        let signature_blob_type = src.read_u16();
        if signature_blob_type != BB_RSA_SIGNATURE_BLOB {
            return Err(invalid_field_err!("wSignatureBlobType", "unsupported signature blob type", in: src));
        }
        let signature_blob_len: usize = cast_length!("wSignatureBlobLen", src.read_u16(), in: src)?;
        ensure_size!(in: src, size: signature_blob_len);
        src.read_slice(signature_blob_len); // RSA signature, not verified (see type docs)

        Ok(Self {
            version: version & CERT_CHAIN_VERSION_MASK,
            sig_alg_id,
            key_alg_id,
            public_key: RsaPublicKey { modulus, exponent },
        })
    }
}

/// RSA-encrypts `message` with a raw (non-DER) public key, as required for the
/// Standard RDP Security client random ([MS-RDPBCGR] 2.2.1.4.3.1.2).
///
/// NyaTerm: RDP's legacy RSA is NOT PKCS#1 -- FreeRDP `crypto_rsa_common`
/// reverses the modulus, the exponent and the input before the modular
/// exponentiation and reverses the output back, which is equivalent to
/// evaluating everything in little-endian byte order, with no padding scheme
/// at all (the 32-byte client random simply is smaller than the modulus).
/// An earlier revision used big-endian PKCS#1 v1.5 type 2: the server then
/// decrypted a different "client random", every derived session key differed,
/// and all inbound MACs failed.
///
/// The result is exactly `modulus_len` bytes (little-endian ciphertext) and
/// does NOT include the 8 trailing zero bytes of the PDU -- append those at
/// the PDU layer.
pub fn rsa_public_encrypt_le(
    message: &[u8],
    public_key: &RsaPublicKey,
) -> Result<Vec<u8>, StandardSecurityError> {
    use num_bigint::BigUint;

    let modulus_len = public_key.modulus_len();
    if message.len() > modulus_len {
        return Err(StandardSecurityError::new(format!(
            "RSA message of {len} bytes does not fit a {modulus_len}-byte modulus",
            len = message.len(),
        )));
    }

    let modulus = BigUint::from_bytes_le(&public_key.modulus);
    let exponent = BigUint::from(public_key.exponent);
    let m = BigUint::from_bytes_le(message);
    let ciphertext = m.modpow(&exponent, &modulus);

    let mut result = ciphertext.to_bytes_le();
    result.resize(modulus_len, 0);

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcc::EncryptionMethod;
    use ironrdp_core::decode_cursor;

    // Vectors computed independently from the MS-RDPBCGR 5.3.4/5.3.5/5.3.6
    // formulas with a second implementation (Python/hashlib).
    fn test_randoms() -> ([u8; 32], [u8; 32]) {
        let mut client_random = [0u8; 32];
        for (i, byte) in client_random.iter_mut().enumerate() {
            *byte = i as u8;
        }
        let mut server_random = [0u8; 32];
        for (i, byte) in server_random.iter_mut().enumerate() {
            *byte = ((i * 3 + 1) % 256) as u8;
        }
        (client_random, server_random)
    }

    fn test_plaintext() -> Vec<u8> {
        (0..64).map(|i| (i as u8).wrapping_mul(7)).collect()
    }

    /// Builds the peer (server-side) view of the same session: the encrypt and
    /// decrypt roles are swapped relative to the client view, so what the
    /// client seals the peer can unseal and vice versa.
    fn peer_of(security: &RdpSecurity) -> RdpSecurity {
        let mut peer = security.clone();
        core::mem::swap(&mut peer.encrypt_key, &mut peer.decrypt_key);
        core::mem::swap(&mut peer.encrypt_update_key, &mut peer.decrypt_update_key);
        core::mem::swap(&mut peer.rc4_encrypt, &mut peer.rc4_decrypt);
        if let Some(fips) = peer.fips.as_mut() {
            core::mem::swap(&mut fips.encrypt_cipher, &mut fips.decrypt_cipher);
        }
        peer
    }

    #[test]
    fn premaster_hash_matches_reference_vector() {
        let (client_random, server_random) = test_randoms();
        let mut pre_master_secret = [0u8; PRE_MASTER_SECRET_SIZE];
        pre_master_secret[..24].copy_from_slice(&client_random[..24]);
        pre_master_secret[24..].copy_from_slice(&server_random[..24]);

        let hash = premaster_hash(&pre_master_secret, b"A", &client_random, &server_random);
        assert_eq!(
            hash,
            [
                0x1a, 0xf3, 0xc8, 0xc8, 0xb6, 0x84, 0x15, 0x67, 0x2c, 0x32, 0xf1, 0x27, 0xb4, 0x8b, 0xa0, 0x2e,
            ],
        );
    }

    /// Key derivation vector, pinned to FreeRDP's live `security_establish_keys`
    /// path (`security_A` for the master secret, then `security_X` -- the
    /// `'X'`/`'YY'`/`'ZZZ'` salted hashes over the master secret -- for the
    /// session key blob, followed by the two MD5 expansions). An earlier revision
    /// used `'A'`/`'BB'`/`'CCC'` master hashes with a swapped salt order, the
    /// dead `security_session_key_blob` variant, which real servers reject.
    #[test]
    fn key_derivation_matches_reference_vectors() {
        let (client_random, server_random) = test_randoms();
        let security = RdpSecurity::establish_keys(
            &client_random,
            &server_random,
            EncryptionMethod::BIT_128,
        )
        .expect("128-bit key derivation");

        assert_eq!(security.rc4_key_len, 16);
        assert_eq!(
            security.sign_key,
            [
                0xcd, 0x83, 0x0b, 0x99, 0x5a, 0x13, 0x14, 0xb1, 0x08, 0xe9, 0x1a, 0x80, 0x84, 0x44, 0x4c, 0x6d,
            ],
        );
        assert_eq!(
            security.decrypt_key,
            [
                0x4d, 0xbd, 0xf5, 0xb1, 0x1f, 0xf6, 0xe3, 0x81, 0x35, 0x83, 0x36, 0x17, 0x66, 0xf9, 0xac, 0x83,
            ],
        );
        assert_eq!(
            security.encrypt_key,
            [
                0x02, 0xaf, 0x71, 0xb5, 0x0e, 0x2f, 0x03, 0xec, 0xef, 0xbc, 0x21, 0x97, 0x98, 0xf7, 0xd7, 0xab,
            ],
        );
    }

    #[test]
    fn mac_computation_matches_reference_vector() {
        let (client_random, server_random) = test_randoms();
        let security =
            RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::BIT_128)
                .expect("128-bit key derivation");

        let mac = mac_signature(&security.sign_key, &test_plaintext());
        assert_eq!(mac, [0x80, 0x5d, 0x99, 0x46, 0xbf, 0xb1, 0x41, 0x4f]);
    }

    #[test]
    fn mac_40bit_applies_salt_prefix() {
        let (client_random, server_random) = test_randoms();
        let security =
            RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::BIT_40)
                .expect("40-bit key derivation");

        assert_eq!(security.rc4_key_len, 8);
        assert_eq!(
            security.sign_key[..3],
            [0xD1, 0x26, 0x9E],
            "40-bit level overwrites the first three key bytes with SALT",
        );
        let mac = mac_signature(&security.sign_key[..8], &test_plaintext());
        assert_eq!(mac, [0x4b, 0xc0, 0x38, 0x22, 0xbf, 0x5b, 0xcc, 0xa5]);
    }

    #[test]
    fn seal_unseal_round_trip_between_peers() {
        let (client_random, server_random) = test_randoms();
        let mut client =
            RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::BIT_128)
                .expect("128-bit key derivation");
        let mut server = peer_of(&client);

        let plaintext = test_plaintext();
        let sealed = client.seal(&plaintext, false).expect("seal");
        assert_eq!(sealed.len(), 8 + plaintext.len());

        // The MAC travels in the clear and the ciphertext differs from the plaintext.
        assert_ne!(&sealed[8..], &plaintext[..]);

        let unsealed = server.unseal(&sealed, false).expect("unseal");
        assert!(unsealed.mac_valid);
        assert_eq!(unsealed.plaintext, plaintext);
    }

    #[test]
    fn rc4_stream_is_continuous_across_pdus() {
        let (client_random, server_random) = test_randoms();
        let mut client =
            RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::BIT_128)
                .expect("128-bit key derivation");
        let mut server = peer_of(&client);

        // Two consecutive seals in the same direction must consume the RC4
        // stream: encrypting the same plaintext twice produces different
        // ciphertext, and the receiver sees a continuous keystream too.
        let plaintext = [0xABu8; 32];
        let first = client.seal(&plaintext, false).expect("seal 1");
        let second = client.seal(&plaintext, false).expect("seal 2");
        assert_ne!(first[8..], second[8..]);

        let first_unsealed = server.unseal(&first, false).expect("unseal 1");
        assert!(first_unsealed.mac_valid);
        assert_eq!(first_unsealed.plaintext, plaintext);
        let second_unsealed = server.unseal(&second, false).expect("unseal 2");
        assert!(second_unsealed.mac_valid);
        assert_eq!(second_unsealed.plaintext, plaintext);
    }

    #[test]
    fn unseal_detects_mac_mismatch() {
        let (client_random, server_random) = test_randoms();
        let mut security =
            RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::BIT_128)
                .expect("128-bit key derivation");

        // The peer must be cloned before the seal so its decrypt stream is at
        // the same RC4 state the client started from.
        let mut peer = peer_of(&security);

        let sealed = security.seal(&[1u8; 16], false).expect("seal");
        let mut tampered = sealed.clone();
        tampered[0] ^= 0xFF;

        // The plaintext is still returned (FreeRDP-style tolerance), but the
        // MAC check must report the tampering.
        let unsealed = peer
            .unseal(&tampered, false)
            .expect("decryption proceeds despite MAC mismatch");
        assert!(!unsealed.mac_valid);
        assert_eq!(unsealed.plaintext, [1u8; 16]);
    }

    #[test]
    fn unsupported_methods_are_rejected() {
        let (client_random, server_random) = test_randoms();
        assert!(RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::empty()).is_err());
    }

    #[test]
    fn salted_mac_round_trip_between_peers() {
        let (client_random, server_random) = test_randoms();
        let mut client = RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::BIT_128)
            .expect("128-bit key derivation");
        let mut server = peer_of(&client);

        let plaintext = test_plaintext();
        let sealed = client.seal(&plaintext, true).expect("seal salted");
        let unsealed = server.unseal(&sealed, true).expect("unseal salted");
        assert!(unsealed.mac_valid);
        assert_eq!(unsealed.plaintext, plaintext);
    }

    #[test]
    fn salted_mac_differs_from_plain_mac() {
        let (client_random, server_random) = test_randoms();
        let mut security = RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::BIT_128)
            .expect("128-bit key derivation");

        let plaintext = test_plaintext();
        let plain = security.seal(&plaintext, false).expect("plain");
        let salted = security.seal(&plaintext, true).expect("salted");
        assert_ne!(plain[..8], salted[..8], "use count must change the MAC");
    }

    #[test]
    fn fips_round_trip_between_peers() {
        let (client_random, server_random) = test_randoms();
        let mut client = RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::FIPS)
            .expect("FIPS key derivation");
        assert!(client.is_fips());
        let mut server = peer_of(&client);

        let plaintext = test_plaintext();
        let sealed = client.seal(&plaintext, false).expect("seal fips");
        let unsealed = server.unseal(&sealed, false).expect("unseal fips");
        assert!(unsealed.mac_valid);
        assert_eq!(unsealed.plaintext, plaintext);

        // Non-block-multiple length exercises the sender-side padding.
        let odd = vec![0x5Au8; 37];
        let sealed_odd = client.seal(&odd, false).expect("seal fips odd");
        let unsealed_odd = server.unseal(&sealed_odd, false).expect("unseal fips odd");
        assert!(unsealed_odd.mac_valid);
        assert_eq!(unsealed_odd.plaintext, odd);
    }

    #[test]
    fn fips_stream_is_continuous_across_pdus() {
        let (client_random, server_random) = test_randoms();
        let mut client = RdpSecurity::establish_keys(&client_random, &server_random, EncryptionMethod::FIPS)
            .expect("FIPS key derivation");
        let mut server = peer_of(&client);

        let plaintext = [0x42u8; 16];
        let first = client.seal(&plaintext, false).expect("seal 1");
        let second = client.seal(&plaintext, false).expect("seal 2");
        assert_ne!(first[12..], second[12..], "CBC chain must advance across PDUs");

        let first_unsealed = server.unseal(&first, false).expect("unseal 1");
        let second_unsealed = server.unseal(&second, false).expect("unseal 2");
        assert_eq!(first_unsealed.plaintext, plaintext);
        assert_eq!(second_unsealed.plaintext, plaintext);
    }

    #[test]
    fn hmac_sha1_matches_rfc2202_vector_with_use_count() {
        // RFC 2202 test case 1 construction (key = 0x0b x20, data = "Hi There")
        // with the FIPS MAC use-count suffix (32-bit LE zero) appended to the
        // message. Vector computed with an independent implementation.
        let key = [0x0bu8; 20];
        let digest = hmac_sha1_fips(&key, b"Hi There", 0);
        assert_eq!(
            digest,
            [
                0x40, 0x7c, 0x59, 0xa1, 0xfe, 0xfc, 0xdf, 0xd1, 0xc4, 0xc0, 0x7f, 0x9d, 0x81, 0xfd, 0x1c, 0xd4, 0x3a,
                0x05, 0xa9, 0xef,
            ],
        );
    }

    #[test]
    fn fips_expand_key_bits_produces_odd_parity_24_bytes() {
        let input = [0x0au8; 21];
        let expanded = fips_expand_key_bits(&input);
        assert_eq!(expanded.len(), 24);
        for byte in expanded {
            assert_eq!(byte.count_ones() % 2, 1, "DES keys must have odd parity");
        }
    }

    #[test]
    fn security_exchange_pdu_round_trip() {
        let pdu = SecurityExchangePdu {
            encrypted_client_random: vec![0x42; 72],
        };

        let encoded = ironrdp_core::encode_vec(&pdu).expect("encode");
        assert_eq!(&encoded[..4], &72u32.to_le_bytes());

        let decoded = decode_cursor::<SecurityExchangePdu>(&mut ReadCursor::new(&encoded)).expect("decode");
        assert_eq!(decoded, pdu);
    }

    #[test]
    fn proprietary_server_certificate_round_trip() {
        let modulus = (0..64u8).collect::<Vec<u8>>();
        let mut cert = Vec::new();
        cert.extend_from_slice(&CERT_CHAIN_VERSION_1.to_le_bytes()); // dwVersion
        cert.extend_from_slice(&SIGNATURE_ALG_RSA.to_le_bytes()); // dwSigAlgId
        cert.extend_from_slice(&KEY_EXCHANGE_ALG_RSA.to_le_bytes()); // dwKeyAlgId
        cert.extend_from_slice(&BB_RSA_KEY_BLOB.to_le_bytes()); // wPublicKeyBlobType
        // wPublicKeyBlobLen covers only the RSA_PUBLIC_KEY data that follows
        // the 2-byte type + 2-byte length header (FreeRDP semantics: the
        // process function consumes magic + keylen + bitlen + datalen + pubexp
        // + modulus + the 8-byte padding = exactly this length).
        let key_blob_len =
            (4 + 4 + 4 + 4 + 4 + modulus.len() + RSA_KEY_TRAILING_PADDING) as u16;
        cert.extend_from_slice(&key_blob_len.to_le_bytes()); // wPublicKeyBlobLen
        cert.extend_from_slice(&RSA_MAGIC); // magic
        cert.extend_from_slice(&((modulus.len() + 8) as u32).to_le_bytes()); // keylen
        cert.extend_from_slice(&((modulus.len() * 8) as u32).to_le_bytes()); // bitlen
        cert.extend_from_slice(&((modulus.len() * 8 - 1) as u32).to_le_bytes()); // datalen
        cert.extend_from_slice(&65537u32.to_le_bytes()); // pubExp
        cert.extend_from_slice(&modulus); // modulus
        cert.extend_from_slice(&[0u8; RSA_KEY_TRAILING_PADDING]);
        cert.extend_from_slice(&BB_RSA_SIGNATURE_BLOB.to_le_bytes()); // wSignatureBlobType
        cert.extend_from_slice(&RSA_SIGNATURE_BLOB_LEN.to_le_bytes()); // wSignatureBlobLen
        cert.extend_from_slice(&[0xAB; 72]); // signature

        let decoded = decode_cursor::<ProprietaryServerCertificate>(&mut ReadCursor::new(&cert)).expect("decode");
        assert_eq!(decoded.version, CERT_CHAIN_VERSION_1);
        assert_eq!(decoded.public_key.exponent, 65537);
        assert_eq!(decoded.public_key.modulus, modulus);
    }

    /// RDP's legacy RSA round trip: little-endian (byte-reversed) evaluation,
    /// no padding. Decrypts with the private exponent through the same
    /// little-endian convention and checks the message comes back intact.
    #[test]
    fn rsa_le_round_trip() {
        use num_bigint::BigUint;

        // Small RSA keypair: p = 1009, q = 1013, n = 1022117, phi = 1020096,
        // e = 13, d = 78469 (precomputed). The modulus bytes are LITTLE-ENDIAN
        // (the same convention `rsa_public_encrypt_le` evaluates).
        let public_key = RsaPublicKey {
            modulus: vec![0xA5, 0x98, 0x0F], // 1022117
            exponent: 13,
        };
        let d = BigUint::from(78469u64);

        let message: [u8; 2] = [0x41, 0x42];

        let ciphertext = rsa_public_encrypt_le(&message, &public_key).expect("encrypt");
        assert_eq!(ciphertext.len(), public_key.modulus_len());

        // Decrypt through the same little-endian convention.
        let modulus = BigUint::from_bytes_le(&public_key.modulus);
        let m = BigUint::from_bytes_le(&ciphertext).modpow(&d, &modulus);
        let recovered = m.to_bytes_le();
        assert_eq!(recovered[..message.len()], message, "message must round trip");
        assert!(recovered.len() <= message.len() + 1);
    }
}
