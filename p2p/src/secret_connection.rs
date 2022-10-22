//! `SecretConnection`: Transport layer encryption for Tendermint P2P connections.

use std::{
    cmp,
    convert::{TryFrom, TryInto},
    io::{self, Read, Write},
    marker::{Send, Sync},
    slice,
};

use chacha20poly1305::{
    aead::{generic_array::GenericArray, AeadInPlace},
    ChaCha20Poly1305, KeyInit,
};
use ed25519_dalek::{self as ed25519, Signer, Verifier};
use eyre::{eyre, Result, WrapErr};
use merlin::Transcript;
use rand_core::OsRng;
use subtle::ConstantTimeEq;
use x25519_dalek::{EphemeralSecret, PublicKey as EphemeralPublic};

use tendermint_proto as proto;

pub use self::{kdf::Kdf, nonce::Nonce, protocol::Version, public_key::PublicKey};
use crate::error::Error;

#[cfg(feature = "amino")]
mod amino_types;

mod kdf;
mod nonce;
mod protocol;
mod public_key;

/// Size of the MAC tag
pub const TAG_SIZE: usize = 16;

/// Maximum size of a message
pub const DATA_MAX_SIZE: usize = 1024;

/// 4 + 1024 == 1028 total frame size
const DATA_LEN_SIZE: usize = 4;
const TOTAL_FRAME_SIZE: usize = DATA_MAX_SIZE + DATA_LEN_SIZE;

/// Handshake is a process of establishing the SecretConnection between two peers.
/// Specification: https://github.com/tendermint/spec/blob/master/spec/p2p/peer.md#authenticated-encryption-handshake
struct Handshake<S> {
    protocol_version: Version,
    state: S,
}

/// Handshake states

/// AwaitingEphKey means we're waiting for the remote ephemeral pubkey.
struct AwaitingEphKey {
    local_privkey: ed25519::Keypair,
    local_eph_privkey: Option<EphemeralSecret>,
}

/// AwaitingAuthSig means we're waiting for the remote authenticated signature.
struct AwaitingAuthSig {
    sc_mac: [u8; 32],
    kdf: Kdf,
    recv_cipher: ChaCha20Poly1305,
    send_cipher: ChaCha20Poly1305,
    local_signature: ed25519::Signature,
}

impl Handshake<AwaitingEphKey> {
    /// Initiate a handshake.
    pub fn new(
        local_privkey: ed25519::Keypair,
        protocol_version: Version,
    ) -> (Self, EphemeralPublic) {
        // Generate an ephemeral key for perfect forward secrecy.
        let local_eph_privkey = EphemeralSecret::new(&mut OsRng);
        let local_eph_pubkey = EphemeralPublic::from(&local_eph_privkey);

        (
            Handshake {
                protocol_version,
                state: AwaitingEphKey {
                    local_privkey,
                    local_eph_privkey: Some(local_eph_privkey),
                },
            },
            local_eph_pubkey,
        )
    }

    /// Performs a Diffie-Hellman key agreement and creates a local signature.
    /// Transitions Handshake into AwaitingAuthSig state.
    pub fn got_key(
        &mut self,
        remote_eph_pubkey: EphemeralPublic,
    ) -> Result<Handshake<AwaitingAuthSig>> {
        let local_eph_privkey = match self.state.local_eph_privkey.take() {
            Some(key) => key,
            None => return Err(eyre!("forgot to call Handshake::new?")),
        };
        let local_eph_pubkey = EphemeralPublic::from(&local_eph_privkey);

        // Compute common shared secret.
        let shared_secret = EphemeralSecret::diffie_hellman(local_eph_privkey, &remote_eph_pubkey);

        let mut transcript = Transcript::new(b"TENDERMINT_SECRET_CONNECTION_TRANSCRIPT_HASH");

        // Reject all-zero outputs from X25519 (i.e. from low-order points)
        //
        // See the following for information on potential attacks this check
        // aids in mitigating:
        //
        // - https://github.com/tendermint/kms/issues/142
        // - https://eprint.iacr.org/2019/526.pdf
        if shared_secret.as_bytes().ct_eq(&[0x00; 32]).unwrap_u8() == 1 {
            return Err(Error::InvalidKey)
                .wrap_err("low-order points found (potential MitM attack!)");
        }

        // Sort by lexical order.
        let local_eph_pubkey_bytes = *local_eph_pubkey.as_bytes();
        let (low_eph_pubkey_bytes, high_eph_pubkey_bytes) =
            sort32(local_eph_pubkey_bytes, *remote_eph_pubkey.as_bytes());

        transcript.append_message(b"EPHEMERAL_LOWER_PUBLIC_KEY", &low_eph_pubkey_bytes);
        transcript.append_message(b"EPHEMERAL_UPPER_PUBLIC_KEY", &high_eph_pubkey_bytes);
        transcript.append_message(b"DH_SECRET", shared_secret.as_bytes());

        // Check if the local ephemeral public key was the least, lexicographically sorted.
        let loc_is_least = local_eph_pubkey_bytes == low_eph_pubkey_bytes;

        let kdf = Kdf::derive_secrets_and_challenge(shared_secret.as_bytes(), loc_is_least);

        let mut sc_mac: [u8; 32] = [0; 32];

        transcript.challenge_bytes(b"SECRET_CONNECTION_MAC", &mut sc_mac);

        // Sign the challenge bytes for authentication.
        let local_signature = if self.protocol_version.has_transcript() {
            sign_challenge(&sc_mac, &self.state.local_privkey)?
        } else {
            sign_challenge(&kdf.challenge, &self.state.local_privkey)?
        };

        Ok(Handshake {
            protocol_version: self.protocol_version,
            state: AwaitingAuthSig {
                sc_mac,
                recv_cipher: ChaCha20Poly1305::new(&kdf.recv_secret.into()),
                send_cipher: ChaCha20Poly1305::new(&kdf.send_secret.into()),
                kdf,
                local_signature,
            },
        })
    }
}

impl Handshake<AwaitingAuthSig> {
    /// Returns a verified pubkey of the remote peer.
    pub fn got_signature(&mut self, auth_sig_msg: proto::p2p::AuthSigMessage) -> Result<PublicKey> {
        let remote_pubkey = auth_sig_msg
            .pub_key
            .and_then(|pk| match pk.sum? {
                proto::crypto::public_key::Sum::Ed25519(ref bytes) => {
                    ed25519::PublicKey::from_bytes(bytes).ok()
                }
                proto::crypto::public_key::Sum::Secp256k1(_) => None,
            })
            .ok_or(Error::CryptoError)?;

        let remote_sig = ed25519::Signature::try_from(auth_sig_msg.sig.as_slice())
            .map_err(|_| Error::CryptoError)?;

        if self.protocol_version.has_transcript() {
            remote_pubkey
                .verify(&self.state.sc_mac, &remote_sig)
                .map_err(|_| Error::CryptoError)?;
        } else {
            remote_pubkey
                .verify(&self.state.kdf.challenge, &remote_sig)
                .map_err(|_| Error::CryptoError)?;
        }

        // We've authorized.
        Ok(remote_pubkey.into())
    }
}

/// Encrypted connection between peers in a Tendermint network.
pub struct SecretConnection<IoHandler: Read + Write + Send + Sync> {
    io_handler: IoHandler,
    protocol_version: Version,
    recv_nonce: Nonce,
    send_nonce: Nonce,
    recv_cipher: ChaCha20Poly1305,
    send_cipher: ChaCha20Poly1305,
    remote_pubkey: Option<PublicKey>,
    recv_buffer: Vec<u8>,
}

impl<IoHandler: Read + Write + Send + Sync> SecretConnection<IoHandler> {
    /// Returns the remote pubkey. Panics if there's no key.
    pub fn remote_pubkey(&self) -> PublicKey {
        self.remote_pubkey.expect("remote_pubkey uninitialized")
    }

    /// Performs a handshake and returns a new SecretConnection.
    pub fn new(
        mut io_handler: IoHandler,
        local_privkey: ed25519::Keypair,
        protocol_version: Version,
    ) -> Result<SecretConnection<IoHandler>> {
        // Start a handshake process.
        let local_pubkey = PublicKey::from(&local_privkey);
        let (mut h, local_eph_pubkey) = Handshake::new(local_privkey, protocol_version);

        // Write local ephemeral pubkey and receive one too.
        let remote_eph_pubkey =
            share_eph_pubkey(&mut io_handler, &local_eph_pubkey, protocol_version)?;

        // Compute a local signature (also recv_cipher & send_cipher)
        let mut h = h.got_key(remote_eph_pubkey)?;

        let mut sc = SecretConnection {
            io_handler,
            protocol_version,
            recv_buffer: vec![],
            recv_nonce: Nonce::default(),
            send_nonce: Nonce::default(),
            recv_cipher: h.state.recv_cipher.clone(),
            send_cipher: h.state.send_cipher.clone(),
            remote_pubkey: None,
        };

        // Share each other's pubkey & challenge signature.
        // NOTE: the data must be encrypted/decrypted using ciphers.
        let auth_sig_msg = match local_pubkey {
            PublicKey::Ed25519(ref pk) => {
                share_auth_signature(&mut sc, pk, &h.state.local_signature)?
            }
        };

        // Authenticate remote pubkey.
        let remote_pubkey = h.got_signature(auth_sig_msg)?;

        // All good!
        sc.remote_pubkey = Some(remote_pubkey);
        Ok(sc)
    }

    /// Encrypt AEAD authenticated data
    fn encrypt(
        &self,
        chunk: &[u8],
        sealed_frame: &mut [u8; TAG_SIZE + TOTAL_FRAME_SIZE],
    ) -> Result<()> {
        debug_assert!(!chunk.is_empty(), "chunk is empty");
        debug_assert!(
            chunk.len() <= TOTAL_FRAME_SIZE - DATA_LEN_SIZE,
            "chunk is too big: {}! max: {}",
            chunk.len(),
            DATA_MAX_SIZE,
        );
        sealed_frame[..DATA_LEN_SIZE].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
        sealed_frame[DATA_LEN_SIZE..DATA_LEN_SIZE + chunk.len()].copy_from_slice(chunk);

        let tag = self
            .send_cipher
            .encrypt_in_place_detached(
                GenericArray::from_slice(self.send_nonce.to_bytes()),
                b"",
                &mut sealed_frame[..TOTAL_FRAME_SIZE],
            )
            .map_err(|_| Error::CryptoError)?;

        sealed_frame[TOTAL_FRAME_SIZE..].copy_from_slice(tag.as_slice());

        Ok(())
    }

    /// Decrypt AEAD authenticated data
    fn decrypt(&self, ciphertext: &[u8], out: &mut [u8]) -> Result<usize> {
        if ciphertext.len() < TAG_SIZE {
            return Err(Error::CryptoError).wrap_err_with(|| {
                format!(
                    "ciphertext must be at least as long as a MAC tag {}",
                    TAG_SIZE
                )
            });
        }

        // Split ChaCha20 ciphertext from the Poly1305 tag
        let (ct, tag) = ciphertext.split_at(ciphertext.len() - TAG_SIZE);

        if out.len() < ct.len() {
            return Err(Error::CryptoError).wrap_err("output buffer is too small");
        }

        let in_out = &mut out[..ct.len()];
        in_out.copy_from_slice(ct);

        self.recv_cipher
            .decrypt_in_place_detached(
                GenericArray::from_slice(self.recv_nonce.to_bytes()),
                b"",
                in_out,
                tag.into(),
            )
            .map_err(|_| Error::CryptoError)?;

        Ok(in_out.len())
    }
}

impl<IoHandler> Read for SecretConnection<IoHandler>
where
    IoHandler: Read + Write + Send + Sync,
{
    // CONTRACT: data smaller than DATA_MAX_SIZE is read atomically.
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        if !self.recv_buffer.is_empty() {
            let n = cmp::min(data.len(), self.recv_buffer.len());
            data.copy_from_slice(&self.recv_buffer[..n]);
            let mut leftover_portion = vec![0; self.recv_buffer.len().checked_sub(n).unwrap()];
            leftover_portion.clone_from_slice(&self.recv_buffer[n..]);
            self.recv_buffer = leftover_portion;

            return Ok(n);
        }

        let mut sealed_frame = [0u8; TAG_SIZE + TOTAL_FRAME_SIZE];
        self.io_handler.read_exact(&mut sealed_frame)?;

        // decrypt the frame
        let mut frame = [0u8; TOTAL_FRAME_SIZE];
        let res = self.decrypt(&sealed_frame, &mut frame);

        if res.is_err() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                res.err().unwrap().to_string(),
            ));
        }

        self.recv_nonce.increment();
        // end decryption

        let chunk_length = u32::from_le_bytes(frame[..4].try_into().unwrap());

        if chunk_length as usize > DATA_MAX_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("chunk is too big: {}! max: {}", chunk_length, DATA_MAX_SIZE),
            ));
        }

        let mut chunk = vec![0; chunk_length as usize];
        chunk.clone_from_slice(
            &frame[DATA_LEN_SIZE..(DATA_LEN_SIZE.checked_add(chunk_length as usize).unwrap())],
        );

        let n = cmp::min(data.len(), chunk.len());
        data[..n].copy_from_slice(&chunk[..n]);
        self.recv_buffer.copy_from_slice(&chunk[n..]);

        Ok(n)
    }
}

impl<IoHandler> Write for SecretConnection<IoHandler>
where
    IoHandler: Read + Write + Send + Sync,
{
    // Writes encrypted frames of `TAG_SIZE` + `TOTAL_FRAME_SIZE`
    // CONTRACT: data smaller than DATA_MAX_SIZE is read atomically.
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut n = 0usize;
        let mut data_copy = data;
        while !data_copy.is_empty() {
            let chunk: &[u8];
            if DATA_MAX_SIZE < data.len() {
                chunk = &data[..DATA_MAX_SIZE];
                data_copy = &data_copy[DATA_MAX_SIZE..];
            } else {
                chunk = data_copy;
                data_copy = &[0u8; 0];
            }
            let sealed_frame = &mut [0u8; TAG_SIZE + TOTAL_FRAME_SIZE];
            let res = self.encrypt(chunk, sealed_frame);
            if res.is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    res.err().unwrap().to_string(),
                ));
            }
            self.send_nonce.increment();
            // end encryption

            self.io_handler.write_all(&sealed_frame[..])?;
            n = n.checked_add(chunk.len()).unwrap();
        }

        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io_handler.flush()
    }
}

/// Returns remote_eph_pubkey
fn share_eph_pubkey<IoHandler: Read + Write + Send + Sync>(
    handler: &mut IoHandler,
    local_eph_pubkey: &EphemeralPublic,
    protocol_version: Version,
) -> Result<EphemeralPublic> {
    // Send our pubkey and receive theirs in tandem.
    // TODO(ismail): on the go side this is done in parallel, here we do send and receive after
    // each other. thread::spawn would require a static lifetime.
    // Should still work though.
    handler.write_all(&protocol_version.encode_initial_handshake(&local_eph_pubkey))?;

    let mut response_len = 0u8;
    handler.read_exact(slice::from_mut(&mut response_len))?;

    let mut buf = vec![0; response_len as usize];
    handler.read_exact(&mut buf)?;
    protocol_version.decode_initial_handshake(&buf)
}

/// Return is of the form lo, hi
fn sort32(first: [u8; 32], second: [u8; 32]) -> ([u8; 32], [u8; 32]) {
    if second > first {
        (first, second)
    } else {
        (second, first)
    }
}

/// Sign the challenge with the local private key
fn sign_challenge(
    challenge: &[u8; 32],
    local_privkey: &dyn Signer<ed25519::Signature>,
) -> Result<ed25519::Signature> {
    local_privkey
        .try_sign(challenge)
        .map_err(|_| Error::CryptoError.into())
}

// TODO(ismail): change from DecodeError to something more generic
// this can also fail while writing / sending
fn share_auth_signature<IoHandler: Read + Write + Send + Sync>(
    sc: &mut SecretConnection<IoHandler>,
    pubkey: &ed25519::PublicKey,
    local_signature: &ed25519::Signature,
) -> Result<proto::p2p::AuthSigMessage> {
    let buf = sc
        .protocol_version
        .encode_auth_signature(pubkey, &local_signature);

    sc.write_all(&buf)?;

    let mut buf = vec![0; sc.protocol_version.auth_sig_msg_response_len()];
    sc.read_exact(&mut buf)?;
    sc.protocol_version.decode_auth_signature(&buf)
}

#[cfg(tests)]
mod tests {
    use super::*;

    #[test]
    fn test_sort() {
        // sanity check
        let t1 = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ];
        let t2 = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 1,
        ];
        let (ref t3, ref t4) = sort32(t1, t2);
        assert_eq!(t1, *t3);
        assert_eq!(t2, *t4);
    }

    #[test]
    fn test_dh_compatibility() {
        let local_priv = &[
            15, 54, 189, 54, 63, 255, 158, 244, 56, 168, 155, 63, 246, 79, 208, 192, 35, 194, 39,
            232, 170, 187, 179, 36, 65, 36, 237, 12, 225, 176, 201, 54,
        ];
        let remote_pub = &[
            193, 34, 183, 46, 148, 99, 179, 185, 242, 148, 38, 40, 37, 150, 76, 251, 25, 51, 46,
            143, 189, 201, 169, 218, 37, 136, 51, 144, 88, 196, 10, 20,
        ];

        // generated using computeDHSecret in go
        let expected_dh = &[
            92, 56, 205, 118, 191, 208, 49, 3, 226, 150, 30, 205, 230, 157, 163, 7, 36, 28, 223,
            84, 165, 43, 78, 38, 126, 200, 40, 217, 29, 36, 43, 37,
        ];
        let got_dh = diffie_hellman(local_priv, remote_pub);

        assert_eq!(expected_dh, &got_dh);
    }
}

#[cfg(test)]
mod test {
    use std::thread;

    use super::*;

    #[test]
    fn test_handshake() {
        let (pipe1, pipe2) = pipe::bipipe_buffered();

        let peer1 = thread::spawn(|| {
            let mut csprng = OsRng {};
            let privkey1: ed25519::Keypair = ed25519::Keypair::generate(&mut csprng);
            let conn1 = SecretConnection::new(pipe2, privkey1, Version::V0_34);
            assert_eq!(conn1.is_ok(), true);
        });

        let peer2 = thread::spawn(|| {
            let mut csprng = OsRng {};
            let privkey2: ed25519::Keypair = ed25519::Keypair::generate(&mut csprng);
            let conn2 = SecretConnection::new(pipe1, privkey2, Version::V0_34);
            assert_eq!(conn2.is_ok(), true);
        });

        peer1.join().expect("peer1 thread has panicked");
        peer2.join().expect("peer2 thread has panicked");
    }

    #[test]
    fn test_read_write_single_message() {
        let (pipe1, pipe2) = pipe::bipipe_buffered();

        const MESSAGE: &str = "The Queen's Gambit";

        let sender = thread::spawn(move || {
            let mut csprng = OsRng {};
            let privkey1: ed25519::Keypair = ed25519::Keypair::generate(&mut csprng);
            let mut conn1 = SecretConnection::new(pipe2, privkey1, Version::V0_34)
                .expect("handshake to succeed");

            conn1
                .write_all(MESSAGE.as_bytes())
                .expect("expected to write message");
        });

        let receiver = thread::spawn(move || {
            let mut csprng = OsRng {};
            let privkey2: ed25519::Keypair = ed25519::Keypair::generate(&mut csprng);
            let mut conn2 = SecretConnection::new(pipe1, privkey2, Version::V0_34)
                .expect("handshake to succeed");

            let mut buf = [0; MESSAGE.len()];
            conn2
                .read_exact(&mut buf)
                .expect("expected to read message");
            assert_eq!(MESSAGE.as_bytes(), &buf);
        });

        sender.join().expect("sender thread has panicked");
        receiver.join().expect("receiver thread has panicked");
    }

    #[test]
    fn test_evil_peer_shares_invalid_eph_key() {
        let mut csprng = OsRng {};
        let local_privkey: ed25519::Keypair = ed25519::Keypair::generate(&mut csprng);
        let (mut h, _) = Handshake::new(local_privkey, Version::V0_34);
        let bytes: [u8; 32] = [0; 32];
        let res = h.got_key(EphemeralPublic::from(bytes));
        assert_eq!(res.is_err(), true);
    }

    #[test]
    fn test_evil_peer_shares_invalid_auth_sig() {
        let mut csprng = OsRng {};
        let local_privkey: ed25519::Keypair = ed25519::Keypair::generate(&mut csprng);
        let (mut h, _) = Handshake::new(local_privkey, Version::V0_34);
        let res = h.got_key(EphemeralPublic::from(x25519_dalek::X25519_BASEPOINT_BYTES));
        assert_eq!(res.is_err(), false);

        let mut h = res.unwrap();
        let res = h.got_signature(proto::p2p::AuthSigMessage {
            pub_key: None,
            sig: vec![],
        });
        assert_eq!(res.is_err(), true);
    }
}
