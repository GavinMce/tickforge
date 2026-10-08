//! A strategy as the host takes it: who it is, what it watches, how to build it, and the proof that it
//! has been replayed on a captured tape.

use tf_universe::Spec;

use crate::runner::DynRunner;

/// Builds a fresh runner (with no members; the host gives it its universe). The same builder makes
/// the instance that is replayed for the certificate and the one that runs live.
pub type Build = Box<dyn Fn() -> Box<dyn DynRunner> + Send + Sync>;

/// Where a strategy's orders go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The simulated broker, fed by the live data.
    Sim,
    /// The paper broker given to the host (Alpaca paper, when it exists).
    Paper,
}

pub struct StrategyDef {
    /// The strategy number in its intents, and in the budget tree's id map.
    pub id: u16,
    pub name: String,
    /// Its parameters as text, so a change of parameters is a different strategy for the certificate.
    pub params: String,
    pub universe: Spec,
    /// Its priority for Tier 1 evictions (see `tf_engine::claims`).
    pub priority: u8,
    pub route: Route,
    pub build: Build,
}

pub(crate) fn fnv(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in *p {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // A separator no part contains, so ("ab","c") and ("a","bc") differ.
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl StrategyDef {
    /// Identifies this strategy as configured: number, name, parameters and universe. Not the code.
    pub fn fingerprint(&self) -> u64 {
        fnv(&[
            &self.id.to_le_bytes(),
            self.name.as_bytes(),
            self.params.as_bytes(),
            self.universe.render().as_bytes(),
            &[self.priority],
            &[match self.route {
                Route::Sim => 0,
                Route::Paper => 1,
            }],
        ])
    }
}

/// What a replay on tape showed. Only [`crate::certify`] makes one; the host checks its seal and
/// that it is for the strategy being added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    pub strategy_fp: u64,
    /// Which tape (for a capture, its manifest's fingerprint).
    pub tape_id: u64,
    pub events: u64,
    pub intents: u64,
    pub accepted: u64,
    /// A hash of every decision and fill of the replay, for comparing with a later replay.
    pub outcome_hash: u64,
    seal: u64,
}

impl Certificate {
    pub(crate) fn new(
        strategy_fp: u64,
        tape_id: u64,
        events: u64,
        intents: u64,
        accepted: u64,
        outcome_hash: u64,
    ) -> Certificate {
        let mut c = Certificate {
            strategy_fp,
            tape_id,
            events,
            intents,
            accepted,
            outcome_hash,
            seal: 0,
        };
        c.seal = c.compute_seal();
        c
    }

    fn compute_seal(&self) -> u64 {
        fnv(&[
            b"tf-host certificate v1",
            &self.strategy_fp.to_le_bytes(),
            &self.tape_id.to_le_bytes(),
            &self.events.to_le_bytes(),
            &self.intents.to_le_bytes(),
            &self.accepted.to_le_bytes(),
            &self.outcome_hash.to_le_bytes(),
        ])
    }

    /// Whether the fields are as `certify` left them.
    pub fn is_intact(&self) -> bool {
        self.seal == self.compute_seal()
    }

    /// The certificate as one word of text, to be kept in a file and read back by [`Certificate::from_text`].
    pub fn to_text(&self) -> String {
        format!(
            "cert1:{:016x}:{:016x}:{:x}:{:x}:{:x}:{:016x}:{:016x}",
            self.strategy_fp,
            self.tape_id,
            self.events,
            self.intents,
            self.accepted,
            self.outcome_hash,
            self.seal
        )
    }

    /// A certificate from its text. One whose fields do not match its seal (edited, cut, or made by hand) is refused.
    pub fn from_text(text: &str) -> Result<Certificate, String> {
        let w: Vec<&str> = text.trim().split(':').collect();
        if w.len() != 8 || w[0] != "cert1" {
            return Err("not a certificate (cert1:...)".to_owned());
        }
        let hex =
            |i: usize| u64::from_str_radix(w[i], 16).map_err(|_| format!("field {i} is not hex"));
        let c = Certificate {
            strategy_fp: hex(1)?,
            tape_id: hex(2)?,
            events: hex(3)?,
            intents: hex(4)?,
            accepted: hex(5)?,
            outcome_hash: hex(6)?,
            seal: hex(7)?,
        };
        if !c.is_intact() {
            return Err("the seal does not match: the certificate was altered".to_owned());
        }
        Ok(c)
    }
}
