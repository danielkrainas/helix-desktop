//! gcr's `sx-aes-1` secret exchange (helix-desktop-spec
//! `org.gnome.keyring.internal.Prompter.md`, "Secret exchange"): DH in the
//! RFC 3526 1536-bit group, HKDF-SHA256, AES-128-CBC with PKCS#7.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::Engine;
use num_bigint::BigUint;

const GROUP: &str = "sx-aes-1";
/// RFC 3526, 1536-bit MODP group.
const PRIME_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA237327FFFFFFFFFFFFFFFF";
const PRIME_BYTES: usize = 192;

type Enc = cbc::Encryptor<aes::Aes128>;
type Dec = cbc::Decryptor<aes::Aes128>;

fn prime() -> BigUint {
    BigUint::parse_bytes(PRIME_HEX.as_bytes(), 16).expect("constant prime")
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The `[sx-aes-1]` group's values.
fn parse(text: &str) -> Option<std::collections::HashMap<String, Vec<u8>>> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    if lines.next()? != format!("[{GROUP}]") {
        return None;
    }
    let mut out = std::collections::HashMap::new();
    for line in lines {
        if line.starts_with('[') {
            break;
        }
        let (k, v) = line.split_once('=')?;
        out.insert(k.trim().to_owned(), base64::engine::general_purpose::STANDARD.decode(v.trim()).ok()?);
    }
    Some(out)
}

/// One side of an exchange.
pub struct Exchange {
    private: BigUint,
    public: Vec<u8>,
    key: Option<[u8; 16]>,
}

impl Exchange {
    pub fn new() -> Self {
        let mut random = [0u8; PRIME_BYTES];
        getrandom::fill(&mut random).expect("random bytes");
        let p = prime();
        // In [2, p - 2].
        let private = BigUint::from_bytes_be(&random) % (&p - 3u32) + 2u32;
        let public = BigUint::from(2u32).modpow(&private, &p).to_bytes_be();
        random.iter_mut().for_each(|b| *b = 0);
        Self { private, public, key: None }
    }

    /// Our public key only.
    pub fn begin(&self) -> String {
        format!("[{GROUP}]\npublic={}\n", b64(&self.public))
    }

    /// Take the peer's public key (and derive the key); the secret it may
    /// carry, decrypted.
    pub fn receive(&mut self, text: &str) -> Result<Option<Vec<u8>>, String> {
        let values = parse(text).ok_or("not an sx-aes-1 exchange")?;
        let peer = values.get("public").ok_or("no public key")?;
        let p = prime();
        let peer = BigUint::from_bytes_be(peer);
        if peer < BigUint::from(2u32) || peer >= &p - 1u32 {
            return Err("bad public key".into());
        }
        let shared = peer.modpow(&self.private, &p).to_bytes_be();
        let mut ikm = vec![0u8; PRIME_BYTES - shared.len()];
        ikm.extend_from_slice(&shared);
        let mut key = [0u8; 16];
        hkdf::Hkdf::<sha2::Sha256>::new(None, &ikm).expand(&[], &mut key).map_err(|e| e.to_string())?;
        ikm.iter_mut().for_each(|b| *b = 0);
        self.key = Some(key);
        match (values.get("secret"), values.get("iv")) {
            (Some(secret), Some(iv)) if iv.len() == 16 => {
                let mut buf = secret.clone();
                let plain = Dec::new(&key.into(), iv.as_slice().into()).decrypt_padded_mut::<Pkcs7>(&mut buf).map_err(|_| "cannot decrypt")?.to_vec();
                buf.iter_mut().for_each(|b| *b = 0);
                Ok(Some(plain))
            }
            _ => Ok(None),
        }
    }

    /// Our public key and, if given, `secret` encrypted (needs
    /// [`Exchange::receive`] first).
    pub fn send(&self, secret: Option<&[u8]>) -> Result<String, String> {
        let key = self.key.ok_or("no peer key yet")?;
        let mut out = format!("[{GROUP}]\npublic={}\n", b64(&self.public));
        if let Some(secret) = secret {
            let mut iv = [0u8; 16];
            getrandom::fill(&mut iv).map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; (secret.len() / 16 + 1) * 16];
            buf[..secret.len()].copy_from_slice(secret);
            let cipher = Enc::new(&key.into(), &iv.into()).encrypt_padded_mut::<Pkcs7>(&mut buf, secret.len()).map_err(|_| "cannot encrypt")?.to_vec();
            buf.iter_mut().for_each(|b| *b = 0);
            out.push_str(&format!("secret={}\niv={}\n", b64(&cipher), b64(&iv)));
        }
        Ok(out)
    }
}

impl Default for Exchange {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        if let Some(key) = self.key.as_mut() {
            key.iter_mut().for_each(|b| *b = 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let (mut a, mut b) = (Exchange::new(), Exchange::new());
        // a begins; b receives a's key and answers with a secret.
        assert_eq!(b.receive(&a.begin()).unwrap(), None);
        let sent = b.send(Some(b"hunter2")).unwrap();
        assert!(sent.starts_with("[sx-aes-1]\npublic="));
        assert_eq!(a.receive(&sent).unwrap().as_deref(), Some(&b"hunter2"[..]));
        // Without a secret.
        assert_eq!(a.receive(&b.send(None).unwrap()).unwrap(), None);
        assert!(a.receive("[other]\npublic=AA==").is_err());
        assert!(Exchange::new().send(None).is_err(), "no peer yet");
    }
}
