//! The replay log, `.gzlog` (spec §6.3).
//!
//! ```text
//! magic "GZLOG\0\0\0", version u16 LE
//! header: program hash (32), grant hash (32), seed (32), manifest (LEB len + bytes)
//! records: tag u8, then
//!   0x01 Frame   n u64 LE, now_ms u64 LE
//!   0x02 Inject  class u8, LEB len + encoding of `chan!(args...)`
//!   0x03 Commit  n u64 LE, hash (32)
//!   0x04 Budget  n u64 LE
//!   0x05 Error   n u64 LE, LEB len + UTF-8
//!   0x06 Dispatch n u64 LE, bubbles u8, LEB count + LEB path indices,
//!                LEB len + type, LEB len + encoding of the fields map
//!   0x07 Script  program hash (32), grant hash (32)
//! ```
//!
//! A `Dispatch` records a user-input event between frames `n` and `n + 1`
//! by the path of child indices to its target, which is the same on every
//! backend whose committed documents hash alike. Replay re-runs it, so the
//! names it mints, the `once` listeners it retires and any synchronous
//! `decide` drain it performs happen again exactly; the data it produced for
//! ordinary listeners is not regenerated, because those injections are
//! already in the log. A `Script` record lists each script after the first
//! in a multi-script document, in document order.
//!
//! Injections are stored as the normal form of the send they cause, so the log
//! carries names and data in the executive's own encoding. Grants are recorded
//! by capability, never by key: the seed regenerates the keys.

use crate::{as_send, Class, Injection};
use k1ndl1ng_norm::term::leb;
use k1ndl1ng_norm::Norm;

pub const LOG_MAGIC: &[u8; 8] = b"GZLOG\0\0\0";
pub const LOG_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub program_hash: [u8; 32],
    pub grant_hash: [u8; 32],
    pub seed: [u8; 32],
    pub manifest: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Record {
    Frame { n: u64, now_ms: u64 },
    Inject(Injection),
    Commit { n: u64, hash: [u8; 32] },
    Budget { n: u64 },
    Error { n: u64, msg: String },
    Dispatch { n: u64, path: Vec<u32>, ty: String, fields: Norm, bubbles: bool },
    Script { program_hash: [u8; 32], grant_hash: [u8; 32] },
}

#[derive(Clone, Debug)]
pub struct TabLog {
    pub header: Header,
    pub records: Vec<Record>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LogError {
    BadMagic,
    Truncated,
    BadTag(u8),
    BadInject,
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], LogError> {
        let s = self.b.get(self.i..self.i + n).ok_or(LogError::Truncated)?;
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, LogError> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64, LogError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }
    fn h32(&mut self) -> Result<[u8; 32], LogError> {
        let mut a = [0u8; 32];
        a.copy_from_slice(self.take(32)?);
        Ok(a)
    }
    fn leb(&mut self) -> Result<usize, LogError> {
        let mut out = 0usize;
        for s in 0..5 {
            let c = self.u8()?;
            out |= ((c & 0x7f) as usize) << (7 * s);
            if c & 0x80 == 0 {
                return Ok(out);
            }
        }
        Err(LogError::Truncated)
    }
    fn blob(&mut self) -> Result<&'a [u8], LogError> {
        let n = self.leb()?;
        self.take(n)
    }
}

impl TabLog {
    pub fn new(header: Header) -> TabLog {
        TabLog {
            header,
            records: Vec::new(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut o = LOG_MAGIC.to_vec();
        o.extend_from_slice(&LOG_VERSION.to_le_bytes());
        o.extend_from_slice(&self.header.program_hash);
        o.extend_from_slice(&self.header.grant_hash);
        o.extend_from_slice(&self.header.seed);
        leb(self.header.manifest.len() as u32, &mut o);
        o.extend_from_slice(&self.header.manifest);
        for r in &self.records {
            match r {
                Record::Frame { n, now_ms } => {
                    o.push(1);
                    o.extend_from_slice(&n.to_le_bytes());
                    o.extend_from_slice(&now_ms.to_le_bytes());
                }
                Record::Inject(i) => {
                    o.push(2);
                    o.push(i.class as u8);
                    let s = Norm::send(i.chan.clone(), false, i.args.clone());
                    leb(s.encode().len() as u32, &mut o);
                    o.extend_from_slice(s.encode());
                }
                Record::Commit { n, hash } => {
                    o.push(3);
                    o.extend_from_slice(&n.to_le_bytes());
                    o.extend_from_slice(hash);
                }
                Record::Budget { n } => {
                    o.push(4);
                    o.extend_from_slice(&n.to_le_bytes());
                }
                Record::Error { n, msg } => {
                    o.push(5);
                    o.extend_from_slice(&n.to_le_bytes());
                    leb(msg.len() as u32, &mut o);
                    o.extend_from_slice(msg.as_bytes());
                }
                Record::Dispatch { n, path, ty, fields, bubbles } => {
                    o.push(6);
                    o.extend_from_slice(&n.to_le_bytes());
                    o.push(*bubbles as u8);
                    leb(path.len() as u32, &mut o);
                    for i in path {
                        leb(*i, &mut o);
                    }
                    leb(ty.len() as u32, &mut o);
                    o.extend_from_slice(ty.as_bytes());
                    leb(fields.encode().len() as u32, &mut o);
                    o.extend_from_slice(fields.encode());
                }
                Record::Script { program_hash, grant_hash } => {
                    o.push(7);
                    o.extend_from_slice(program_hash);
                    o.extend_from_slice(grant_hash);
                }
            }
        }
        o
    }

    pub fn from_bytes(b: &[u8]) -> Result<TabLog, LogError> {
        let mut r = Rd { b, i: 0 };
        if r.take(8)? != LOG_MAGIC {
            return Err(LogError::BadMagic);
        }
        r.take(2)?;
        let header = Header {
            program_hash: r.h32()?,
            grant_hash: r.h32()?,
            seed: r.h32()?,
            manifest: r.blob()?.to_vec(),
        };
        let mut records = Vec::new();
        let mut seq = 0u64;
        while r.i < b.len() {
            let rec = match r.u8()? {
                1 => Record::Frame {
                    n: r.u64()?,
                    now_ms: r.u64()?,
                },
                2 => {
                    let class = Class::from_u8(r.u8()?).ok_or(LogError::BadInject)?;
                    let t = Norm::decode(r.blob()?).map_err(|_| LogError::BadInject)?;
                    let (chan, args) = as_send(&t).ok_or(LogError::BadInject)?;
                    seq += 1;
                    Record::Inject(Injection { class, chan, args, seq })
                }
                3 => Record::Commit {
                    n: r.u64()?,
                    hash: r.h32()?,
                },
                4 => Record::Budget { n: r.u64()? },
                5 => {
                    let n = r.u64()?;
                    Record::Error {
                        n,
                        msg: String::from_utf8_lossy(r.blob()?).into_owned(),
                    }
                }
                6 => {
                    let n = r.u64()?;
                    let bubbles = r.u8()? != 0;
                    let count = r.leb()?;
                    let mut path = Vec::with_capacity(count.min(4096));
                    for _ in 0..count {
                        path.push(r.leb()? as u32);
                    }
                    let ty = String::from_utf8_lossy(r.blob()?).into_owned();
                    let fields = Norm::decode(r.blob()?).map_err(|_| LogError::BadInject)?;
                    Record::Dispatch { n, path, ty, fields, bubbles }
                }
                7 => Record::Script {
                    program_hash: r.h32()?,
                    grant_hash: r.h32()?,
                },
                t => return Err(LogError::BadTag(t)),
            };
            records.push(rec);
        }
        Ok(TabLog { header, records })
    }
}
