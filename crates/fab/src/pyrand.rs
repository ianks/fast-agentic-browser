//! CPython's `random.Random`, bit for bit, for the calls the scrape fixture
//! generator makes: MT19937 seeded from a non-negative int (`init_by_array`
//! over its 32-bit chunks), `getrandbits`, `_randbelow`, `random`, `choice`,
//! `randint`, `sample`.

const N: usize = 624;
const M: usize = 397;

pub struct Random {
    mt: [u32; N],
    index: usize,
}

impl Random {
    /// `random.Random(seed)` for an int seed.
    pub fn new(seed: u64) -> Self {
        let mut key: Vec<u32> = Vec::new();
        let mut s = seed;
        while s > 0 {
            key.push(s as u32);
            s >>= 32;
        }
        if key.is_empty() {
            key.push(0);
        }
        let mut r = Random { mt: [0; N], index: N };
        r.init_by_array(&key);
        r
    }

    fn init_genrand(&mut self, s: u32) {
        self.mt[0] = s;
        for i in 1..N {
            let prev = self.mt[i - 1];
            self.mt[i] = 1812433253u32.wrapping_mul(prev ^ (prev >> 30)).wrapping_add(i as u32);
        }
        self.index = N;
    }

    fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19650218);
        let (mut i, mut j) = (1usize, 0usize);
        let mut k = N.max(key.len());
        while k > 0 {
            let prev = self.mt[i - 1];
            self.mt[i] = (self.mt[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1664525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                self.mt[0] = self.mt[N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }
        k = N - 1;
        while k > 0 {
            let prev = self.mt[i - 1];
            self.mt[i] = (self.mt[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1566083941)).wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                self.mt[0] = self.mt[N - 1];
                i = 1;
            }
            k -= 1;
        }
        self.mt[0] = 0x8000_0000;
    }

    fn genrand_u32(&mut self) -> u32 {
        const MAG01: [u32; 2] = [0, 0x9908_b0df];
        if self.index >= N {
            for kk in 0..N {
                let y = (self.mt[kk] & 0x8000_0000) | (self.mt[(kk + 1) % N] & 0x7fff_ffff);
                self.mt[kk] = self.mt[(kk + M) % N] ^ (y >> 1) ^ MAG01[(y & 1) as usize];
            }
            self.index = 0;
        }
        let mut y = self.mt[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// `getrandbits(k)` for `k <= 64`.
    pub fn getrandbits(&mut self, k: u32) -> u64 {
        assert!(k <= 64);
        if k == 0 {
            return 0;
        }
        if k <= 32 {
            return (self.genrand_u32() >> (32 - k)) as u64;
        }
        // Words fill from the least significant end; the last is truncated.
        let lo = self.genrand_u32() as u64;
        let hi = (self.genrand_u32() >> (64 - k)) as u64;
        lo | (hi << 32)
    }

    /// `_randbelow(n)`: rejection sampling on `n.bit_length()` bits.
    pub fn randbelow(&mut self, n: u64) -> u64 {
        assert!(n > 0);
        let k = 64 - n.leading_zeros();
        let mut r = self.getrandbits(k);
        while r >= n {
            r = self.getrandbits(k);
        }
        r
    }

    /// `random()`: 53 bits from two words.
    pub fn random(&mut self) -> f64 {
        let a = (self.genrand_u32() >> 5) as f64;
        let b = (self.genrand_u32() >> 6) as f64;
        (a * 67108864.0 + b) * (1.0 / 9007199254740992.0)
    }

    /// `randint(a, b)`, inclusive.
    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        assert!(b >= a);
        a + self.randbelow((b - a + 1) as u64) as i64
    }

    pub fn choice<T: Clone>(&mut self, seq: &[T]) -> T {
        seq[self.randbelow(seq.len() as u64) as usize].clone()
    }

    /// `sample(population, k)`, including CPython's switch between the
    /// pool algorithm and the set-of-indices algorithm.
    pub fn sample<T: Clone>(&mut self, population: &[T], k: usize) -> Vec<T> {
        let n = population.len();
        assert!(k <= n);
        let mut setsize: usize = 21;
        if k > 5 {
            let e = ((k * 3) as f64).ln() / 4f64.ln();
            setsize += 4usize.pow(e.ceil() as u32);
        }
        let mut result = Vec::with_capacity(k);
        if n <= setsize {
            let mut pool: Vec<T> = population.to_vec();
            for i in 0..k {
                let j = self.randbelow((n - i) as u64) as usize;
                result.push(pool[j].clone());
                pool[j] = pool[n - i - 1].clone();
            }
        } else {
            let mut selected = std::collections::HashSet::new();
            for _ in 0..k {
                let mut j = self.randbelow(n as u64) as usize;
                while selected.contains(&j) {
                    j = self.randbelow(n as u64) as usize;
                }
                selected.insert(j);
                result.push(population[j].clone());
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::Random;

    // Expected values printed by CPython 3.14's `random.Random`.
    #[test]
    fn matches_cpython_seed_20260926() {
        let mut r = Random::new(20260926);
        let w: Vec<u64> = (0..3).map(|_| r.getrandbits(32)).collect();
        assert_eq!(w, [313139642, 199192598, 67921173]);
        let v: Vec<i64> = (0..5).map(|_| r.randint(1, 100)).collect();
        assert_eq!(v, [76, 30, 65, 24, 81]);
        assert_eq!(r.random(), 0.036132142806237555);
        assert_eq!(r.random(), 0.37135023992737115);
        let range: Vec<i64> = (1..29).collect();
        assert_eq!(r.sample(&range, 4), [25, 8, 28, 5]); // set algorithm
        assert_eq!(r.sample(&range, 7), [23, 24, 14, 28, 2, 10, 18]); // pool algorithm
        assert_eq!(r.sample(&["a", "b", "c", "d", "e", "f"], 3), ["e", "b", "d"]);
        let abc: Vec<char> = "abcdefg".chars().collect();
        let c: String = (0..5).map(|_| r.choice(&abc)).collect();
        assert_eq!(c, "fbbgg");
        assert_eq!(r.randint(100000, 999999), 230807);
    }

    #[test]
    fn matches_cpython_other_seeds() {
        let mut r = Random::new(0);
        assert_eq!(r.getrandbits(32), 3626764237);
        assert_eq!(r.random(), 0.3852453064766108);
        let mut r = Random::new((1 << 40) + 5); // two-word key
        assert_eq!(r.getrandbits(32), 2166296868);
        assert_eq!(r.random(), 0.516921470080349);
    }
}
