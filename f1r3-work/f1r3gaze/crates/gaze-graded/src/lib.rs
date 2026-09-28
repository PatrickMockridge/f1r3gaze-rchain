//! `gaze-graded` — the resolvers of graded where-clauses (spec §10).
//!
//! A graded page declares a commutative semiring `V`; each enabled candidate of
//! a race carries a clause value in `V`; the resolver chooses. Every operation
//! here is integer arithmetic, so a choice made on `x86_64` is the choice made
//! on `aarch64` and on `wasm32`, bit for bit.
//!
//! Values travel as `u128` so that the store can carry them opaquely
//! (`campf1r3_core::Weight` in work package U5). Packing per semiring:
//!
//! | semiring  | packing                                   | ⊕          | ⊗                 |
//! |-----------|-------------------------------------------|------------|-------------------|
//! | Boolean   | 0 or 1                                    | or         | and               |
//! | Viterbi   | Q32.32 in [0, 1]                          | max        | product, floored  |
//! | tropical  | cost + 2^63 (i64 offset); `u128::MAX` = ∞ | min        | saturating +      |
//! | R≥0       | Q32.32, saturating                        | saturating + | product, floored |

#![forbid(unsafe_code)]

/// One in Q32.32.
pub const ONE_Q: u64 = 1 << 32;
const TROP_ZERO_COST: u128 = 1 << 63;
const TROP_INF: u128 = u128::MAX;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Semiring {
    Boolean,
    Viterbi,
    Tropical,
    Prob,
}

/// How strong a page's clauses may be (spec: the manifest's `ceiling`).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Ceiling {
    Crisp,
    Idempotent,
    Sampled,
}

impl Ceiling {
    pub fn parse(s: &str) -> Option<Ceiling> {
        Some(match s {
            "crisp" => Ceiling::Crisp,
            "idempotent" => Ceiling::Idempotent,
            "sampled" => Ceiling::Sampled,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Ceiling::Crisp => "crisp",
            Ceiling::Idempotent => "idempotent",
            Ceiling::Sampled => "sampled",
        }
    }
}

impl Semiring {
    pub fn parse(s: &str) -> Option<Semiring> {
        Some(match s {
            "boolean" => Semiring::Boolean,
            "viterbi" => Semiring::Viterbi,
            "tropical" => Semiring::Tropical,
            "prob" => Semiring::Prob,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Semiring::Boolean => "boolean",
            Semiring::Viterbi => "viterbi",
            Semiring::Tropical => "tropical",
            Semiring::Prob => "prob",
        }
    }
    /// The least ceiling that admits this semiring.
    pub fn strength(self) -> Ceiling {
        match self {
            Semiring::Boolean => Ceiling::Crisp,
            Semiring::Viterbi | Semiring::Tropical => Ceiling::Idempotent,
            Semiring::Prob => Ceiling::Sampled,
        }
    }
    /// ⊕'s unit: the value of a disabled candidate.
    pub fn zero(self) -> u128 {
        match self {
            Semiring::Tropical => TROP_INF,
            _ => 0,
        }
    }
    /// ⊗'s unit.
    pub fn one(self) -> u128 {
        match self {
            Semiring::Boolean => 1,
            Semiring::Viterbi | Semiring::Prob => ONE_Q as u128,
            Semiring::Tropical => TROP_ZERO_COST,
        }
    }
    /// A crisp condition, lifted: `1` if it holds, `0` otherwise.
    pub fn crisp(self, b: bool) -> u128 {
        if b { self.one() } else { self.zero() }
    }
    /// `w(n, d)`: the scalar n/d, floored to Q32.32. Viterbi clamps to 1.
    /// Returns `None` for a negative numerator, a non-positive denominator, or
    /// a semiring that has no such scalars.
    pub fn w(self, n: i64, d: i64) -> Option<u128> {
        if n < 0 || d <= 0 {
            return None;
        }
        let q = ((n as u128) << 32) / d as u128;
        match self {
            Semiring::Viterbi => Some(q.min(ONE_Q as u128)),
            Semiring::Prob => Some(q.min(u64::MAX as u128)),
            Semiring::Boolean => Some(if q > 0 { 1 } else { 0 }),
            Semiring::Tropical => None,
        }
    }
    /// `cost(n)`: a tropical scalar.
    pub fn cost(self, n: i64) -> Option<u128> {
        match self {
            Semiring::Tropical => Some(((n as i128) + (1i128 << 63)) as u128),
            _ => None,
        }
    }
    /// ⊕ — disjunction.
    pub fn add(self, a: u128, b: u128) -> u128 {
        match self {
            Semiring::Boolean => ((a | b) != 0) as u128,
            Semiring::Viterbi => a.max(b),
            Semiring::Tropical => a.min(b),
            Semiring::Prob => a.saturating_add(b).min(u64::MAX as u128),
        }
    }
    /// ⊗ — conjunction.
    pub fn mul(self, a: u128, b: u128) -> u128 {
        match self {
            Semiring::Boolean => ((a != 0) && (b != 0)) as u128,
            Semiring::Viterbi | Semiring::Prob => ((a * b) >> 32).min(u64::MAX as u128),
            Semiring::Tropical => {
                if a == TROP_INF || b == TROP_INF {
                    TROP_INF
                } else {
                    // (a - Z) + (b - Z) + Z, saturating below ∞.
                    let s = (a as i128 - TROP_ZERO_COST as i128) + (b as i128 - TROP_ZERO_COST as i128);
                    let v = s + TROP_ZERO_COST as i128;
                    if v < 0 { 0 } else { (v as u128).min(TROP_INF - 1) }
                }
            }
        }
    }
    /// The least enabled value, for the fairness combinator ψ ⊕ ε.
    pub fn epsilon(self) -> u128 {
        match self {
            Semiring::Boolean => 1,
            Semiring::Viterbi | Semiring::Prob => 1,
            Semiring::Tropical => TROP_INF - 1,
        }
    }
    /// Is a value the ⊕-unit, i.e. is the candidate disabled?
    pub fn is_zero(self, v: u128) -> bool {
        v == self.zero()
    }
}

/// xoshiro256**, seeded from 32 bytes. Deterministic across targets.
#[derive(Clone, Debug)]
pub struct Xoshiro {
    s: [u64; 4],
}

impl Xoshiro {
    pub fn new(seed: [u8; 32]) -> Xoshiro {
        let mut s = [0u64; 4];
        for (i, w) in s.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            b.copy_from_slice(&seed[8 * i..8 * i + 8]);
            *w = u64::from_le_bytes(b);
        }
        if s == [0; 4] {
            s[0] = 0x9E37_79B9_7F4A_7C15;
        }
        Xoshiro { s }
    }
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

/// One selection, for the replay log (`Event::Select`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub n: u32,
    pub chosen: Option<u32>,
}

/// A page's resolver.
#[derive(Clone, Debug)]
pub struct Resolver {
    pub semiring: Semiring,
    pub fair: bool,
    rng: Xoshiro,
}

impl Resolver {
    pub fn new(semiring: Semiring, fair: bool, seed: [u8; 32]) -> Resolver {
        Resolver {
            semiring,
            fair,
            rng: Xoshiro::new(seed),
        }
    }

    /// Choose among candidates given their clause values, in candidate order.
    /// Ties go to the earlier candidate, which is the store's deterministic
    /// order. `None` means no candidate is enabled.
    pub fn select(&mut self, ws: &[u128]) -> Selection {
        let sr = self.semiring;
        let ws: Vec<u128> = if self.fair {
            ws.iter().map(|w| sr.add(*w, sr.epsilon())).collect()
        } else {
            ws.to_vec()
        };
        let chosen = match sr {
            Semiring::Boolean => ws.iter().position(|w| *w != 0),
            Semiring::Viterbi => best_by(&ws, sr, |a, b| a > b),
            Semiring::Tropical => best_by(&ws, sr, |a, b| a < b),
            Semiring::Prob => {
                let total: u128 = ws.iter().fold(0u128, |t, w| t.saturating_add(*w));
                if total == 0 {
                    None
                } else {
                    // r uniform in [0, total): a 64-bit draw scaled by total.
                    let r = (self.rng.next_u64() as u128 * total) >> 64;
                    let mut acc = 0u128;
                    ws.iter().position(|w| {
                        acc += *w;
                        r < acc
                    })
                }
            }
        };
        Selection {
            n: ws.len() as u32,
            chosen: chosen.map(|i| i as u32),
        }
    }
}

fn best_by(ws: &[u128], sr: Semiring, better: impl Fn(u128, u128) -> bool) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, w) in ws.iter().enumerate() {
        if sr.is_zero(*w) {
            continue;
        }
        match best {
            None => best = Some(i),
            Some(b) if better(*w, ws[b]) => best = Some(i),
            _ => {}
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boolean_is_first_enabled() {
        let mut r = Resolver::new(Semiring::Boolean, false, [1; 32]);
        assert_eq!(r.select(&[0, 1, 1]).chosen, Some(1));
        assert_eq!(r.select(&[0, 0]).chosen, None);
    }

    #[test]
    fn viterbi_is_argmax_with_ties_to_the_first() {
        let sr = Semiring::Viterbi;
        let mut r = Resolver::new(sr, false, [1; 32]);
        let a = sr.w(1, 4).unwrap();
        let b = sr.w(3, 4).unwrap();
        assert_eq!(r.select(&[a, b, b]).chosen, Some(1));
        assert_eq!(sr.mul(sr.one(), a), a);
        assert_eq!(sr.w(5, 1), Some(ONE_Q as u128), "clamped to one");
    }

    #[test]
    fn tropical_is_least_cost() {
        let sr = Semiring::Tropical;
        let mut r = Resolver::new(sr, false, [1; 32]);
        let c = |n| sr.cost(n).unwrap();
        assert_eq!(r.select(&[c(5), c(-2), c(3)]).chosen, Some(1));
        assert_eq!(sr.mul(c(2), c(3)), c(5));
        assert_eq!(sr.mul(c(2), sr.zero()), sr.zero());
        assert_eq!(r.select(&[sr.zero()]).chosen, None);
    }

    #[test]
    fn prob_samples_in_proportion_and_replays() {
        let sr = Semiring::Prob;
        let ws = [sr.w(1, 4).unwrap(), sr.w(3, 4).unwrap()];
        let mut r = Resolver::new(sr, false, [7; 32]);
        let mut counts = [0u32; 2];
        let mut seq = Vec::new();
        for _ in 0..10_000 {
            let c = r.select(&ws).chosen.unwrap() as usize;
            counts[c] += 1;
            seq.push(c);
        }
        assert!((2_200..2_800).contains(&counts[0]), "{counts:?}");
        let mut again = Resolver::new(sr, false, [7; 32]);
        let seq2: Vec<usize> = (0..10_000).map(|_| again.select(&ws).chosen.unwrap() as usize).collect();
        assert_eq!(seq, seq2, "same seed, same choices");
    }

    #[test]
    fn fairness_enables_starved_candidates() {
        let sr = Semiring::Prob;
        let ws = [ONE_Q as u128, 0];
        let mut unfair = Resolver::new(sr, false, [9; 32]);
        assert!((0..1000).all(|_| unfair.select(&ws).chosen == Some(0)));
        // ε is tiny against 1, so only its existence is checked: the second
        // candidate is now enabled and the total is not the first's alone.
        let fair_ws: Vec<u128> = ws.iter().map(|w| sr.add(*w, sr.epsilon())).collect();
        assert!(fair_ws[1] > 0);
        let mut vit = Resolver::new(Semiring::Viterbi, true, [9; 32]);
        assert_eq!(vit.select(&[0, 0]).chosen, Some(0), "all-zero is enabled under ε");
    }

    #[test]
    fn ceilings_order() {
        assert!(Semiring::Prob.strength() > Semiring::Viterbi.strength());
        assert!(Semiring::Boolean.strength() <= Ceiling::Crisp);
    }
}
