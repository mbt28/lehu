//! SRP-6a, the accessory side, over the 3072-bit group of RFC 5054 with
//! SHA-512: CarPlay's pair-setup. The user is the literal "Pair-Setup", the
//! password the setup code "3939" (the phone never asks the driver for a
//! code; the SRP is a formality the long-term keys ride on). Once per
//! phone, so the big-number work is plain `num-bigint`.

use num_bigint::BigUint;
use num_traits::Zero;
use subtle::ConstantTimeEq;

use crate::crypto;

const N_BYTES: usize = 384;
const N_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74\
020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437\
4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED\
EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05\
98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB\
9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B\
E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718\
3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33\
A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7\
ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864\
D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2\
08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

fn n() -> BigUint {
    // The constant is well-formed hex; a parse failure is a build error.
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap_or_default()
}

fn g() -> BigUint {
    BigUint::from(5u32)
}

fn pad(x: &BigUint) -> Vec<u8> {
    let bytes = x.to_bytes_be();
    let mut out = vec![0u8; N_BYTES.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

pub struct Server {
    username: Vec<u8>,
    salt: [u8; 16],
    b: BigUint,
    v: BigUint,
    big_b: Vec<u8>,
}

impl Server {
    /// Begin: the salt and `b` are random.
    pub fn start(username: &str, password: &str) -> Option<Self> {
        let salt: [u8; 16] = crypto::random_array()?;
        let b: [u8; 32] = crypto::random_array()?;
        Some(Self::start_with(username, password, salt, &b))
    }

    /// Begin with given secrets, for the tests' known answers.
    pub fn start_with(username: &str, password: &str, salt: [u8; 16], b_bytes: &[u8]) -> Self {
        let n = n();
        let g = g();
        // k = H(N | PAD(g)); x = H(salt | H(I ":" p)); v = g^x.
        let k = BigUint::from_bytes_be(&crypto::sha512(&[&n.to_bytes_be(), &pad(&g)]));
        let inner = crypto::sha512(&[username.as_bytes(), b":", password.as_bytes()]);
        let x = BigUint::from_bytes_be(&crypto::sha512(&[&salt, &inner]));
        let v = g.modpow(&x, &n);
        let b = BigUint::from_bytes_be(b_bytes);
        // B = (k v + g^b) mod N.
        let big_b = (k * &v + g.modpow(&b, &n)) % &n;
        Self {
            username: username.as_bytes().to_vec(),
            salt,
            b,
            v,
            big_b: pad(&big_b),
        }
    }

    pub fn salt(&self) -> &[u8; 16] {
        &self.salt
    }

    /// B, padded to the group size.
    pub fn public(&self) -> &[u8] {
        &self.big_b
    }

    /// Check the phone's A and proof M1. The session key K and our proof M2
    /// on success; `None` on a bad A or a proof that does not match (the
    /// compare is constant-time).
    pub fn verify(&self, a_bytes: &[u8], m1: &[u8]) -> Option<([u8; 64], [u8; 64])> {
        let n = n();
        let a = BigUint::from_bytes_be(a_bytes);
        // A mod N == 0 would make S trivial.
        if (&a % &n).is_zero() {
            return None;
        }
        let a_pad = pad(&a);
        let u = BigUint::from_bytes_be(&crypto::sha512(&[&a_pad, &self.big_b]));
        // S = (A v^u)^b; K = H(S).
        let s = (&a * self.v.modpow(&u, &n) % &n).modpow(&self.b, &n);
        let k = crypto::sha512(&[&s.to_bytes_be()]);
        // M1 = H((H(N) xor H(g)) | H(I) | salt | PAD(A) | PAD(B) | K).
        let hn = crypto::sha512(&[&n.to_bytes_be()]);
        let hg = crypto::sha512(&[&g().to_bytes_be()]);
        let mut hx = [0u8; 64];
        for i in 0..64 {
            hx[i] = hn[i] ^ hg[i];
        }
        let hi = crypto::sha512(&[&self.username]);
        let expect = crypto::sha512(&[&hx, &hi, &self.salt, &a_pad, &self.big_b, &k]);
        if m1.len() != 64 || !bool::from(expect.ct_eq(m1)) {
            return None;
        }
        let m2 = crypto::sha512(&[&a_pad, m1, &k]);
        Some((k, m2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Computed independently in Python (hashlib + pow) with salt 00..0f,
    // b = 0x42 x 32, a = 0x17 x 32, user "Pair-Setup", password "3939".
    const B: &str = "88b81287380218ecefc04c258a63ce1b7739b1f5de9a3e8cdf8bb5a6d9c81a9b0d6cd8b3a05be9d9004306064fb234f2eedc8896044f85853caca508aa1a216cd5cdc351e37bd948bd728b1380b3ace8093502af58a3d4d16dd8b4eb9e095a16dcf194bbbe487d3fcaa46598318a7e65d72c8f2b5234b7a86ba2e4ba92e209f799da1f1ce1d0aa01816d4ca343b87f71dd1e9deb7645b9514f86badc51b60eac7af1292224d882afcfca1bcfe98392ada2c8c878e7dabb28d4815384232970800aa0d4b811e3714ae11624498f0f0634dfc6c1ac6eee5be814bc1bdc5d8fb4fb8c626cd85e6b5b136a4bd8e7f85d5e236cb43e79e71431017cd283b2ae27f377c26c19f9c2bf5fff88af176b5da74449f87f06844d3d820fd2f1c6a974f021dbc8b5043af78fc519cd6cc063ca1fee6fdbc5064c61ec78dd206c8321fac364c54ea79d0ecd235dc7fdcd52dc946a4054744d6d973979f9d85c07d7c9b116cec88b0f225f873003ef2867d4027fddacbf0ad13b964ef3433d050b21f23e74c211";
    const A: &str = "c591b2ef0cf8aaed71bf18c5e3aaf0f80bd71eabddd0e9313a665ca040881e309674f752afa83a881225d34c140974d43bfd57bc1d819c3169698e8a77ea5132ec8a174383f6d8210124efe4de2cc99e1d88f6528bd30ae0283cc49e028befff61c1ffafd5bc31541995b3c0382ff4ed83c859dc0d56e9047635213bcb1a9dc95bc91fd642e01e16a7a7df50467dbe15f3ecfaf48801f5126d3f365f84b68acf3f4daf1e3bd14efd31600f155a350093a899ac49336b92e7d029399c4e5df0873c7b2c2dfa27c0ac96b1e8a2d35578a162116a70520b365818b4414fe08891d36021576ae1557dc4dca73ea09d7f8fc7aa65dc3f10992782df12d1d0976992272e37782bde0615b596e845e3f2be6e1669669d96e41ed3968fc856d34bd469b2aefb97e7e13ddb253e4a910cf10e25b5fcf5bcaea6a5298ec983b010e375b092f0e600ebca0701ec36d6965916493cf9ba9007a894534b88655b36c50646cb824d82fb047452ebc0b5bbf6160b30ca00c1e0b821751c226016cf8b1fb53d888d";
    const K: &str = "b3801fef07c0b0fe193739493bb6bd37061c05f2df846141bd59805f390d1f283200872cbecbe64bfe94d259c01f5d74b0527e26c2f18ae729753ea0294c724f";
    const M1: &str = "58f8b677d1c0163227abb8b5bf0b71ed86a7eef20a817190285afc7856d364cbd3e09c7b698613ca3afb3139921f8f73329026083060f28a9d9cd2ec1631e7c5";
    const M2: &str = "ce6573a9a0eb98a98124c1d2ba7dc39a120b17fd7c89e738612600903fbe35789fb6b7c170eb0f55a5223944ec499b23357daac11bb78dfa5d5c6cbef62486e9";

    fn server() -> Server {
        let salt: [u8; 16] = core::array::from_fn(|i| i as u8);
        Server::start_with("Pair-Setup", "3939", salt, &[0x42; 32])
    }

    #[test]
    fn matches_the_independent_vector() {
        let s = server();
        assert_eq!(s.public(), hex(B));
        let (k, m2) = s.verify(&hex(A), &hex(M1)).expect("proof holds");
        assert_eq!(k.to_vec(), hex(K));
        assert_eq!(m2.to_vec(), hex(M2));
    }

    #[test]
    fn rejects_bad_proofs_and_a_zero_a() {
        let s = server();
        let mut m1 = hex(M1);
        m1[3] ^= 1;
        assert!(s.verify(&hex(A), &m1).is_none());
        assert!(s.verify(&hex(A), &hex(M1)[..63]).is_none());
        assert!(s.verify(&[0u8; 384], &hex(M1)).is_none());
        assert!(s.verify(&n().to_bytes_be(), &hex(M1)).is_none());
        assert!(Server::start("Pair-Setup", "3939").is_some());
    }
}
