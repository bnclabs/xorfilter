//! Library implements xor-filter.
//!
//! This is a port of its
//! [original implementation](https://github.com/FastFilter/xorfilter)
//! written in golang.

#[allow(unused_imports)]
use std::collections::hash_map::RandomState;
use std::{
    collections::hash_map::DefaultHasher,
    convert::TryInto,
    ffi, fs,
    hash::{self, BuildHasher, Hash, Hasher},
    io::{self, Error, ErrorKind, Read, Write},
};

use mkit::{
    self,
    cbor::{Cbor, FromCbor, IntoCbor},
    db::Bloom,
    Cborize,
};

fn murmur64(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

// returns random number, modifies the seed
fn splitmix64(seed: &mut u64) -> u64 {
    *seed = (*seed).wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn mixsplit(key: u64, seed: u64) -> u64 {
    murmur64(key.wrapping_add(seed))
}

fn reduce(hash: u32, n: u32) -> u32 {
    // http://lemire.me/blog/2016/06/27/a-fast-alternative-to-the-modulo-reduction/
    (((hash as u64) * (n as u64)) >> 32) as u32
}

fn fingerprint(hash: u64) -> u64 {
    hash ^ (hash >> 32)
}

#[derive(Clone, Default)]
struct XorSet {
    xor_mask: u64,
    count: u32,
}

#[derive(Default)]
struct Hashes {
    h: u64,
    h0: u32,
    h1: u32,
    h2: u32,
}

#[derive(Clone, Copy, Default)]
struct KeyIndex {
    hash: u64,
    index: u32,
}

/// Wrapper type for [std::hash::BuildHasherDefault].
pub struct BuildHasherDefault {
    hasher: hash::BuildHasherDefault<DefaultHasher>,
}

impl From<BuildHasherDefault> for Vec<u8> {
    fn from(_: BuildHasherDefault) -> Vec<u8> {
        vec![]
    }
}

impl From<Vec<u8>> for BuildHasherDefault {
    fn from(_: Vec<u8>) -> BuildHasherDefault {
        BuildHasherDefault {
            hasher: hash::BuildHasherDefault::<DefaultHasher>::default(),
        }
    }
}

impl BuildHasher for BuildHasherDefault {
    type Hasher = DefaultHasher;

    fn build_hasher(&self) -> Self::Hasher {
        self.hasher.build_hasher()
    }
}

impl Default for BuildHasherDefault {
    fn default() -> Self {
        BuildHasherDefault {
            hasher: hash::BuildHasherDefault::<DefaultHasher>::default(),
        }
    }
}

/// Type Xor8 is probabilistic data-structure to test membership of an
/// element in a set.
///
/// This implementation has a false positive rate of about 0.3%
/// and a memory usage of less than 9 bits per entry for sizeable sets.
/// Xor8 is parametrized over type `H` which is expected to implement
/// [BuildHasher] trait, like [RandomState] and [BuildHasherDefault].
/// When not supplied, `BuildHasherDefault` is used as the default
/// hash-builder. When applications want to serialize and de-serialize
/// Xor8, avoid using `RandomState`.
pub struct Xor8<H = BuildHasherDefault>
where
    H: BuildHasher,
{
    keys: Option<Vec<u64>>,
    hash_builder: H,
    seed: u64,
    block_length: u32,
    finger_prints: Vec<u8>,
}

impl<H> PartialEq for Xor8<H>
where
    H: BuildHasher,
{
    fn eq(&self, other: &Self) -> bool {
        self.seed == other.seed
            && self.block_length == other.block_length
            && self.finger_prints == other.finger_prints
    }
}

impl<H> Default for Xor8<H>
where
    H: BuildHasher + Default,
{
    fn default() -> Self {
        Xor8 {
            keys: Some(Vec::default()),
            hash_builder: H::default(),
            seed: u64::default(),
            block_length: u32::default(),
            finger_prints: Vec::default(),
        }
    }
}

impl<H> IntoCbor for Xor8<H>
where
    H: BuildHasher + Into<Vec<u8>>,
{
    fn into_cbor(self) -> mkit::Result<Cbor> {
        CborXor8::from(self).into_cbor()
    }
}

impl<H> FromCbor for Xor8<H>
where
    H: Default + BuildHasher + From<Vec<u8>>,
{
    fn from_cbor(val: Cbor) -> mkit::Result<Self> {
        Ok(CborXor8::from_cbor(val)?.into())
    }
}

impl<H> Xor8<H>
where
    H: Default + BuildHasher,
{
    /// New Xor8 instance initialized with `DefaultHasher`.
    pub fn new() -> Self {
        Default::default()
    }
}

impl<H> Xor8<H>
where
    H: BuildHasher,
{
    /// New Xor8 instance initialized with supplied `hasher`.
    pub fn with_hasher(hash_builder: H) -> Self {
        Xor8 {
            keys: Some(Default::default()),
            hash_builder,
            seed: Default::default(),
            block_length: Default::default(),
            finger_prints: Default::default(),
        }
    }

    /// Insert 64-bit digest of a single key. Digest for the key shall
    /// be generated using the default-hasher or via hasher supplied via
    /// [Xor8::with_hasher] method.
    pub fn insert<T: ?Sized + Hash>(&mut self, key: &T) {
        let hashed_key = {
            let mut hasher = self.hash_builder.build_hasher();
            key.hash(&mut hasher);
            hasher.finish()
        };
        self.keys.as_mut().unwrap().push(hashed_key);
    }

    /// Populate 64-bit digests for collection of keys. Digest for the key
    /// shall be generated using the default-hasher or via hasher supplied
    /// via [Xor8::with_hasher] method.
    pub fn populate<T: Hash>(&mut self, keys: &[T]) {
        keys.iter().for_each(|key| {
            let mut hasher = self.hash_builder.build_hasher();
            key.hash(&mut hasher);
            self.keys.as_mut().unwrap().push(hasher.finish());
        })
    }

    /// Populate pre-compute 64-bit digests for keys.
    pub fn populate_keys(&mut self, keys: &[u64]) {
        self.keys.as_mut().unwrap().extend_from_slice(keys)
    }

    /// Build bitmap for keys that are insert using [Xor8::insert] or
    /// [Xor8::populate] method.
    pub fn build(&mut self) {
        let keys = self.keys.take().unwrap();
        self.build_keys(&keys);
    }

    /// Build a bitmap for pre-computed 64-bit digests for keys. If any
    /// keys where inserted using [Xor8::insert], [Xor8::populate],
    /// [Xor8::populate_keys] method shall be ignored.
    pub fn build_keys(&mut self, keys: &[u64]) {
        let (size, mut rngcounter) = (keys.len(), 1_u64);
        let capacity = {
            let capacity = 32 + ((1.23 * (size as f64)).ceil() as u32);
            capacity / 3 * 3 // round it down to a multiple of 3
        };
        self.seed = splitmix64(&mut rngcounter);
        self.block_length = capacity / 3;
        self.finger_prints = vec![Default::default(); capacity as usize];

        let block_length = self.block_length as usize;
        let mut q0: Vec<KeyIndex> = Vec::with_capacity(block_length);
        let mut q1: Vec<KeyIndex> = Vec::with_capacity(block_length);
        let mut q2: Vec<KeyIndex> = Vec::with_capacity(block_length);
        let mut stack: Vec<KeyIndex> = Vec::with_capacity(size);
        let mut sets0: Vec<XorSet> = vec![Default::default(); block_length];
        let mut sets1: Vec<XorSet> = vec![Default::default(); block_length];
        let mut sets2: Vec<XorSet> = vec![Default::default(); block_length];

        loop {
            for key in keys.iter() {
                let hs = self.geth0h1h2(*key);
                sets0[hs.h0 as usize].xor_mask ^= hs.h;
                sets0[hs.h0 as usize].count += 1;
                sets1[hs.h1 as usize].xor_mask ^= hs.h;
                sets1[hs.h1 as usize].count += 1;
                sets2[hs.h2 as usize].xor_mask ^= hs.h;
                sets2[hs.h2 as usize].count += 1;
            }

            q0.clear();
            q1.clear();
            q2.clear();

            let iter = sets0.iter().enumerate().take(self.block_length as usize);
            for (i, item) in iter {
                if item.count == 1 {
                    q0.push(KeyIndex {
                        index: i as u32,
                        hash: item.xor_mask,
                    });
                }
            }
            let iter = sets1.iter().enumerate().take(self.block_length as usize);
            for (i, item) in iter {
                if item.count == 1 {
                    q1.push(KeyIndex {
                        index: i as u32,
                        hash: item.xor_mask,
                    });
                }
            }
            let iter = sets2.iter().enumerate().take(self.block_length as usize);
            for (i, item) in iter {
                if item.count == 1 {
                    q2.push(KeyIndex {
                        index: i as u32,
                        hash: item.xor_mask,
                    });
                }
            }

            stack.clear();

            while !q0.is_empty() || !q1.is_empty() || !q2.is_empty() {
                while let Some(keyindexvar) = q0.pop() {
                    if sets0[keyindexvar.index as usize].count == 0 {
                        // not actually possible after the initial scan.
                        continue;
                    }
                    let hash = keyindexvar.hash;
                    let h1 = self.geth1(hash);
                    let h2 = self.geth2(hash);
                    stack.push(keyindexvar);

                    let mut s = unsafe { sets1.get_unchecked_mut(h1 as usize) };
                    s.xor_mask ^= hash;
                    s.count -= 1;
                    if s.count == 1 {
                        q1.push(KeyIndex {
                            index: h1,
                            hash: s.xor_mask,
                        })
                    }

                    let mut s = unsafe { sets2.get_unchecked_mut(h2 as usize) };
                    s.xor_mask ^= hash;
                    s.count -= 1;
                    if s.count == 1 {
                        q2.push(KeyIndex {
                            index: h2,
                            hash: s.xor_mask,
                        })
                    }
                }
                while let Some(mut keyindexvar) = q1.pop() {
                    if sets1[keyindexvar.index as usize].count == 0 {
                        continue;
                    }
                    let hash = keyindexvar.hash;
                    let h0 = self.geth0(hash);
                    let h2 = self.geth2(hash);
                    keyindexvar.index += self.block_length;
                    stack.push(keyindexvar);

                    let mut s = unsafe { sets0.get_unchecked_mut(h0 as usize) };
                    s.xor_mask ^= hash;
                    s.count -= 1;
                    if s.count == 1 {
                        q0.push(KeyIndex {
                            index: h0,
                            hash: s.xor_mask,
                        })
                    }

                    let mut s = unsafe { sets2.get_unchecked_mut(h2 as usize) };
                    s.xor_mask ^= hash;
                    s.count -= 1;
                    if s.count == 1 {
                        q2.push(KeyIndex {
                            index: h2,
                            hash: s.xor_mask,
                        })
                    }
                }
                while let Some(mut keyindexvar) = q2.pop() {
                    if sets2[keyindexvar.index as usize].count == 0 {
                        continue;
                    }
                    let hash = keyindexvar.hash;
                    let h0 = self.geth0(hash);
                    let h1 = self.geth1(hash);
                    keyindexvar.index += 2 * self.block_length;
                    stack.push(keyindexvar);

                    let mut s = unsafe { sets0.get_unchecked_mut(h0 as usize) };
                    s.xor_mask ^= hash;
                    s.count -= 1;
                    if s.count == 1 {
                        q0.push(KeyIndex {
                            index: h0,
                            hash: s.xor_mask,
                        })
                    }
                    let mut s = unsafe { sets1.get_unchecked_mut(h1 as usize) };
                    s.xor_mask ^= hash;
                    s.count -= 1;
                    if s.count == 1 {
                        q1.push(KeyIndex {
                            index: h1,
                            hash: s.xor_mask,
                        })
                    }
                }
            }

            if stack.len() == size {
                break;
            }

            for item in sets0.iter_mut() {
                *item = Default::default();
            }
            for item in sets1.iter_mut() {
                *item = Default::default();
            }
            for item in sets2.iter_mut() {
                *item = Default::default();
            }
            self.seed = splitmix64(&mut rngcounter)
        }

        while let Some(ki) = stack.pop() {
            let mut val = fingerprint(ki.hash) as u8;
            if ki.index < self.block_length {
                let h1 = (self.geth1(ki.hash) + self.block_length) as usize;
                let h2 = (self.geth2(ki.hash) + 2 * self.block_length) as usize;
                val ^= self.finger_prints[h1] ^ self.finger_prints[h2];
            } else if ki.index < 2 * self.block_length {
                let h0 = self.geth0(ki.hash) as usize;
                let h2 = (self.geth2(ki.hash) + 2 * self.block_length) as usize;
                val ^= self.finger_prints[h0] ^ self.finger_prints[h2];
            } else {
                let h0 = self.geth0(ki.hash) as usize;
                let h1 = (self.geth1(ki.hash) + self.block_length) as usize;
                val ^= self.finger_prints[h0] ^ self.finger_prints[h1]
            }
            self.finger_prints[ki.index as usize] = val;
        }
    }

    /// Contains tell you whether the key is likely part of the set.
    pub fn contains<T: ?Sized + Hash>(&self, key: &T) -> bool {
        let hashed_key = {
            let mut hasher = self.hash_builder.build_hasher();
            key.hash(&mut hasher);
            hasher.finish()
        };
        self.contains_key(hashed_key)
    }

    pub fn contains_key(&self, key: u64) -> bool {
        let hash = mixsplit(key, self.seed);
        let f = fingerprint(hash) as u8;
        let r0 = hash as u32;
        let r1 = hash.rotate_left(21) as u32;
        let r2 = hash.rotate_left(42) as u32;
        let h0 = reduce(r0, self.block_length) as usize;
        let h1 = (reduce(r1, self.block_length) + self.block_length) as usize;
        let h2 = (reduce(r2, self.block_length) + 2 * self.block_length) as usize;
        f == (self.finger_prints[h0] ^ self.finger_prints[h1] ^ self.finger_prints[h2])
    }

    fn geth0h1h2(&self, k: u64) -> Hashes {
        let h = mixsplit(k, self.seed);
        Hashes {
            h,
            h0: reduce(h as u32, self.block_length),
            h1: reduce(h.rotate_left(21) as u32, self.block_length),
            h2: reduce(h.rotate_left(42) as u32, self.block_length),
        }
    }

    fn geth0(&self, hash: u64) -> u32 {
        let r0 = hash as u32;
        reduce(r0, self.block_length)
    }

    fn geth1(&self, hash: u64) -> u32 {
        let r1 = hash.rotate_left(21) as u32;
        reduce(r1, self.block_length)
    }

    fn geth2(&self, hash: u64) -> u32 {
        let r2 = hash.rotate_left(42) as u32;
        reduce(r2, self.block_length)
    }
}

impl<H> Xor8<H>
where
    H: BuildHasher,
{
    /// File signature write on first 4 bytes of file.
    /// ^ stands for xor
    /// TL stands for filter
    /// 1 stands for version 1
    const SIGNATURE_V1: [u8; 4] = [b'^', b'T', b'L', 1];

    /// METADATA_LENGTH is size that required to write size of all the
    /// metadata of the serialized filter.
    // signature length + seed length + block-length +
    //      fingerprint length + fingerprint size
    const METADATA_LENGTH: usize = 4 + 8 + 4 + 4;

    /// Write to file in binary format
    /// TODO Add chechsum of finger_prints into file headers
    pub fn write_file(&self, path: &ffi::OsStr) -> io::Result<usize> {
        let mut f = fs::File::create(path)?;
        let buf = self.to_bytes();
        f.write_all(&buf)?;
        Ok(buf.len())
    }

    /// Read from file in binary format
    pub fn read_file(path: &ffi::OsStr) -> io::Result<Self>
    where
        H: Default,
    {
        let mut f = fs::File::open(path)?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        Self::from_bytes(data)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let capacity = Self::METADATA_LENGTH + self.finger_prints.len();
        let mut buf: Vec<u8> = Vec::with_capacity(capacity);
        buf.extend_from_slice(&Xor8::<H>::SIGNATURE_V1);
        buf.extend_from_slice(&self.seed.to_be_bytes());
        buf.extend_from_slice(&self.block_length.to_be_bytes());
        buf.extend_from_slice(&(self.finger_prints.len() as u32).to_be_bytes());
        buf.extend_from_slice(&self.finger_prints);
        buf
    }

    pub fn from_bytes(buf: Vec<u8>) -> io::Result<Self>
    where
        H: Default,
    {
        // validate the buf first.
        if Self::METADATA_LENGTH > buf.len() {
            return Err(Error::new(ErrorKind::InvalidData, "invalid byte slice"));
        }
        if buf[..4] != Xor8::<H>::SIGNATURE_V1 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "File signature incorrect",
            ));
        }
        let fp_len = u32::from_be_bytes(buf[16..20].try_into().unwrap()) as usize;
        if buf[20..].len() < fp_len {
            return Err(Error::new(ErrorKind::InvalidData, "invalid byte slice"));
        }
        Ok(Xor8 {
            keys: Default::default(),
            hash_builder: H::default(),
            seed: u64::from_be_bytes(buf[4..12].try_into().unwrap()),
            block_length: u32::from_be_bytes(buf[12..16].try_into().unwrap()),
            finger_prints: buf[20..].to_vec(),
        })
    }
}

impl<H> Bloom for Xor8<H>
where
    H: Default + BuildHasher + From<Vec<u8>> + Into<Vec<u8>>,
{
    type Err = Error;

    fn add_key<Q: ?Sized + Hash>(&mut self, key: &Q) {
        self.insert(key)
    }

    fn add_digest32(&mut self, digest: u32) {
        self.populate_keys(&[u64::from(digest)])
    }

    fn build(&mut self) {
        let keys = self.keys.take().unwrap();
        self.build_keys(&keys);
    }

    fn contains<Q: ?Sized + Hash>(&self, element: &Q) -> bool {
        self.contains(element)
    }

    fn to_bytes(&self) -> Result<Vec<u8>, Self::Err> {
        let val = Xor8 {
            keys: None,
            hash_builder: H::default(),
            seed: self.seed,
            block_length: self.block_length,
            finger_prints: self.finger_prints.clone(),
        };

        let mut buf: Vec<u8> = vec![];

        match val.into_cbor() {
            Ok(val) => match val.encode(&mut buf) {
                Ok(_) => Ok(buf),
                Err(err) => Err(Error::new(ErrorKind::InvalidData, err)),
            },
            Err(err) => Err(Error::new(ErrorKind::InvalidData, err)),
        }
    }

    fn from_bytes(mut buf: &[u8]) -> Result<(Self, usize), Self::Err> {
        let (val, n) = match Cbor::decode(&mut buf) {
            Ok(val) => val,
            Err(err) => return Err(Error::new(ErrorKind::InvalidData, err)),
        };
        match Xor8::<H>::from_cbor(val) {
            Ok(val) => Ok((val, n)),
            Err(err) => Err(Error::new(ErrorKind::InvalidData, err)),
        }
    }

    fn or(&self, _other: &Self) -> Result<Self, Self::Err> {
        unimplemented!()
    }
}

// Intermediate type to serialize and de-serialized Xor8 into bytes using
// `mkit` macros.
#[derive(Cborize)]
struct CborXor8 {
    hash_builder: Vec<u8>,
    seed: u64,
    block_length: u32,
    finger_prints: Vec<u8>,
}

impl CborXor8 {
    const ID: &'static str = "xor8/0.0.1";
}

impl<H> From<Xor8<H>> for CborXor8
where
    H: BuildHasher + Into<Vec<u8>>,
{
    fn from(val: Xor8<H>) -> Self {
        CborXor8 {
            hash_builder: val.hash_builder.into(),
            seed: val.seed,
            block_length: val.block_length,
            finger_prints: val.finger_prints,
        }
    }
}

impl<H> From<CborXor8> for Xor8<H>
where
    H: Default + BuildHasher + From<Vec<u8>>,
{
    fn from(val: CborXor8) -> Self {
        Xor8 {
            keys: None,
            hash_builder: val.hash_builder.into(),
            seed: val.seed,
            block_length: val.block_length,
            finger_prints: val.finger_prints,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{prelude::random, rngs::SmallRng, Rng, SeedableRng};

    #[test]
    fn test_basic1() {
        let seed: u128 = random();
        println!("test_basic1 seed {}", seed);
        let mut rng = SmallRng::from_seed(seed.to_le_bytes());

        let testsize = 1_000_000;
        let mut keys: Vec<u64> = Vec::with_capacity(testsize);
        keys.resize(testsize, Default::default());
        for key in keys.iter_mut() {
            *key = rng.gen();
        }

        let filter = {
            let mut filter = Xor8::<RandomState>::new();
            filter.populate(&keys);
            filter.build();
            filter
        };

        for key in keys.iter() {
            assert!(filter.contains(key), "key {} not present", key);
        }

        let (falsesize, mut matches) = (10_000_000, 0_f64);
        let bpv = (filter.finger_prints.len() as f64) * 8.0 / (testsize as f64);
        println!("test_basic1 bits per entry {} bits", bpv);
        assert!(bpv < 10.0, "bpv({}) >= 10.0", bpv);

        for _ in 0..falsesize {
            if filter.contains(&rng.gen::<u64>()) {
                matches += 1_f64;
            }
        }
        let fpp = matches * 100.0 / (falsesize as f64);
        println!("test_basic1 false positive rate {}%", fpp);
        assert!(fpp < 0.40, "fpp({}) >= 0.40", fpp);
    }

    #[test]
    fn test_basic2() {
        let seed: u128 = random();
        println!("test_basic2 seed {}", seed);
        let mut rng = SmallRng::from_seed(seed.to_le_bytes());

        let testsize = 1_000_000;
        let mut keys: Vec<u64> = Vec::with_capacity(testsize);
        keys.resize(testsize, Default::default());
        for key in keys.iter_mut() {
            *key = rng.gen();
        }

        let filter = {
            let mut filter = Xor8::<RandomState>::new();
            filter.populate_keys(&keys);
            filter.build();
            filter
        };

        for key in keys.into_iter() {
            assert!(filter.contains_key(key), "key {} not present", key);
        }

        let (falsesize, mut matches) = (10_000_000, 0_f64);
        let bpv = (filter.finger_prints.len() as f64) * 8.0 / (testsize as f64);
        println!("test_basic2 bits per entry {} bits", bpv);
        assert!(bpv < 10.0, "bpv({}) >= 10.0", bpv);

        for _ in 0..falsesize {
            if filter.contains(&rng.gen::<u64>()) {
                matches += 1_f64;
            }
        }
        let fpp = matches * 100.0 / (falsesize as f64);
        println!("test_basic2 false positive rate {}%", fpp);
        assert!(fpp < 0.40, "fpp({}) >= 0.40", fpp);
    }

    #[test]
    fn test_basic3() {
        let mut seed: u64 = random();
        println!("test_basic3 seed {}", seed);

        let testsize = 100_000;
        let mut keys: Vec<u64> = Vec::with_capacity(testsize);
        keys.resize(testsize, Default::default());
        for key in keys.iter_mut() {
            *key = splitmix64(&mut seed);
        }

        let filter = {
            let mut filter = Xor8::<RandomState>::new();
            keys.iter().for_each(|key| filter.insert(key));
            filter.build();
            filter
        };

        for key in keys.iter() {
            assert!(filter.contains(key), "key {} not present", key);
        }

        let (falsesize, mut matches) = (10_000_000, 0_f64);
        let bpv = (filter.finger_prints.len() as f64) * 8.0 / (testsize as f64);
        println!("test_basic3 bits per entry {} bits", bpv);
        assert!(bpv < 10.0, "bpv({}) >= 10.0", bpv);

        for _ in 0..falsesize {
            let v = splitmix64(&mut seed);
            if filter.contains(&v) {
                matches += 1_f64;
            }
        }
        let fpp = matches * 100.0 / (falsesize as f64);
        println!("test_basic3 false positive rate {}%", fpp);
        assert!(fpp < 0.40, "fpp({}) >= 0.40", fpp);
    }

    #[test]
    fn test_basic4() {
        let seed: u128 = random();
        println!("test_basic4 seed {}", seed);
        let mut rng = SmallRng::from_seed(seed.to_le_bytes());

        let testsize = 1_000_000;
        let mut keys: Vec<u64> = Vec::with_capacity(testsize);
        keys.resize(testsize, Default::default());
        for key in keys.iter_mut() {
            *key = rng.gen();
        }

        let filter = {
            let mut filter = Xor8::<BuildHasherDefault>::new();
            filter.populate(&keys);
            filter.build();
            filter
        };

        for key in keys.iter() {
            assert!(filter.contains(key), "key {} not present", key);
        }

        let (falsesize, mut matches) = (10_000_000, 0_f64);
        let bpv = (filter.finger_prints.len() as f64) * 8.0 / (testsize as f64);
        println!("test_basic4 bits per entry {} bits", bpv);
        assert!(bpv < 10.0, "bpv({}) >= 10.0", bpv);

        for _ in 0..falsesize {
            if filter.contains(&rng.gen::<u64>()) {
                matches += 1_f64;
            }
        }
        let fpp = matches * 100.0 / (falsesize as f64);
        println!("test_basic4 false positive rate {}%", fpp);
        assert!(fpp < 0.40, "fpp({}) >= 0.40", fpp);
    }

    #[test]
    fn test_basic5() {
        let seed: u128 = random();
        println!("test_basic5 seed {}", seed);
        let mut rng = SmallRng::from_seed(seed.to_le_bytes());

        let testsize = 1_000_000;
        let mut keys: Vec<u64> = Vec::with_capacity(testsize);
        keys.resize(testsize, Default::default());
        for key in keys.iter_mut() {
            *key = rng.gen();
        }

        let filter = {
            let mut filter = Xor8::<BuildHasherDefault>::new();
            filter.populate_keys(&keys);
            filter.build();
            filter
        };

        for key in keys.into_iter() {
            assert!(filter.contains_key(key), "key {} not present", key);
        }

        let (falsesize, mut matches) = (10_000_000, 0_f64);
        let bpv = (filter.finger_prints.len() as f64) * 8.0 / (testsize as f64);
        println!("test_basic5 bits per entry {} bits", bpv);
        assert!(bpv < 10.0, "bpv({}) >= 10.0", bpv);

        for _ in 0..falsesize {
            if filter.contains(&rng.gen::<u64>()) {
                matches += 1_f64;
            }
        }
        let fpp = matches * 100.0 / (falsesize as f64);
        println!("test_basic5 false positive rate {}%", fpp);
        assert!(fpp < 0.40, "fpp({}) >= 0.40", fpp);
    }

    #[test]
    fn test_basic6() {
        let mut seed: u64 = random();
        println!("test_basic6 seed {}", seed);

        let testsize = 100_000;
        let mut keys: Vec<u64> = Vec::with_capacity(testsize);
        keys.resize(testsize, Default::default());
        for key in keys.iter_mut() {
            *key = splitmix64(&mut seed);
        }

        let filter = {
            let mut filter = Xor8::<BuildHasherDefault>::new();
            keys.iter().for_each(|key| filter.insert(key));
            filter.build();
            filter
        };

        for key in keys.iter() {
            assert!(filter.contains(key), "key {} not present", key);
        }

        let (falsesize, mut matches) = (10_000_000, 0_f64);
        let bpv = (filter.finger_prints.len() as f64) * 8.0 / (testsize as f64);
        println!("test_basic6 bits per entry {} bits", bpv);
        assert!(bpv < 10.0, "bpv({}) >= 10.0", bpv);

        for _ in 0..falsesize {
            let v = splitmix64(&mut seed);
            if filter.contains(&v) {
                matches += 1_f64;
            }
        }
        let fpp = matches * 100.0 / (falsesize as f64);
        println!("test_basic6 false positive rate {}%", fpp);
        assert!(fpp < 0.40, "fpp({}) >= 0.40", fpp);
    }

    #[test]
    fn test_basic7() {
        let seed: u128 = random();
        println!("test_basic7 seed {}", seed);
        let mut rng = SmallRng::from_seed(seed.to_le_bytes());

        let keys: Vec<u64> = (0..100_000).map(|_| rng.gen::<u64>()).collect();

        let filter = {
            let mut filter = Xor8::<BuildHasherDefault>::new();
            filter.populate(&keys);
            filter.build();
            filter
        };

        for key in keys.iter() {
            assert!(filter.contains(key), "key {} not present", key);
        }

        let filter = {
            let bytes = <Xor8 as Bloom>::to_bytes(&filter).unwrap();
            <Xor8 as Bloom>::from_bytes(&bytes).unwrap().0
        };

        for key in keys.iter() {
            assert!(filter.contains(key), "key {} not present", key);
        }
    }
}
