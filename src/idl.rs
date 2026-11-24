//! Loads the official Anchor IDLs and decodes instruction args and events
//! with a generic Borsh reader driven by the IDL's own type definitions.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;

/// First 8 bytes of an `emit_cpi!` self-invocation: sha256("anchor:event")[..8].
pub const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

pub struct IxDef {
    pub name: String,
    pub accounts: Vec<String>,
    pub args: Vec<(String, Value)>,
}

pub struct Program {
    pub address: String,
    pub label: String,
    pub instructions: HashMap<[u8; 8], IxDef>,
    pub events: HashMap<[u8; 8], String>,
    types: HashMap<String, Value>,
}

fn disc(v: &Value) -> Result<[u8; 8]> {
    let a = v.as_array().ok_or_else(|| anyhow!("discriminator not an array"))?;
    if a.len() != 8 {
        bail!("discriminator length {}", a.len());
    }
    let mut d = [0u8; 8];
    for (i, b) in a.iter().enumerate() {
        d[i] = b.as_u64().ok_or_else(|| anyhow!("bad discriminator byte"))? as u8;
    }
    Ok(d)
}

impl Program {
    pub fn load(path: &Path, label: &str) -> Result<Program> {
        let raw: Value = serde_json::from_slice(&std::fs::read(path).with_context(|| format!("{path:?}"))?)?;
        let address = raw["address"].as_str().ok_or_else(|| anyhow!("IDL has no address"))?.to_string();
        let mut instructions = HashMap::new();
        for ix in raw["instructions"].as_array().into_iter().flatten() {
            let accounts = ix["accounts"].as_array().into_iter().flatten()
                .map(|a| a["name"].as_str().unwrap_or("?").to_string()).collect();
            let args = ix["args"].as_array().into_iter().flatten()
                .map(|a| (a["name"].as_str().unwrap_or("?").to_string(), a["type"].clone())).collect();
            instructions.insert(disc(&ix["discriminator"])?, IxDef { name: ix["name"].as_str().unwrap_or("?").into(), accounts, args });
        }
        let mut events = HashMap::new();
        for e in raw["events"].as_array().into_iter().flatten() {
            events.insert(disc(&e["discriminator"])?, e["name"].as_str().unwrap_or("?").to_string());
        }
        let mut types = HashMap::new();
        for t in raw["types"].as_array().into_iter().flatten() {
            types.insert(t["name"].as_str().unwrap_or("?").to_string(), t["type"].clone());
        }
        Ok(Program { address, label: label.into(), instructions, events, types })
    }

    /// Decode an instruction's args. Returns (name, account names, args).
    pub fn decode_ix(&self, data: &[u8]) -> Option<(&IxDef, Value)> {
        if data.len() < 8 {
            return None;
        }
        let def = self.instructions.get(&data[..8])?;
        let mut r = Reader { b: &data[8..], i: 0 };
        let mut out = Map::new();
        for (name, ty) in &def.args {
            out.insert(name.clone(), self.read(ty, &mut r).unwrap_or(Value::Null));
        }
        Some((def, Value::Object(out)))
    }

    /// Decode an event payload that starts with its 8-byte discriminator.
    /// Trailing fields missing from older, shorter events come back as null
    /// and are listed in `_missing`, so they are never silently read as zero.
    pub fn decode_event(&self, data: &[u8]) -> Option<(String, Value)> {
        if data.len() < 8 {
            return None;
        }
        let name = self.events.get(&data[..8])?.clone();
        let ty = self.types.get(&name)?;
        let mut r = Reader { b: &data[8..], i: 0 };
        let mut out = Map::new();
        let mut missing = vec![];
        for f in ty["fields"].as_array().into_iter().flatten() {
            let fname = f["name"].as_str().unwrap_or("?").to_string();
            match self.read(&f["type"], &mut r) {
                Ok(v) => { out.insert(fname, v); }
                Err(_) => { missing.push(Value::String(fname.clone())); out.insert(fname, Value::Null); }
            }
        }
        if !missing.is_empty() {
            out.insert("_missing".into(), Value::Array(missing));
        }
        if r.i < r.b.len() {
            out.insert("_trailing_bytes".into(), json!(r.b.len() - r.i));
        }
        Some((name, Value::Object(out)))
    }

    fn read(&self, ty: &Value, r: &mut Reader) -> Result<Value> {
        if let Some(s) = ty.as_str() {
            return Ok(match s {
                "bool" => json!(r.take(1)?[0] != 0),
                "u8" => json!(r.take(1)?[0]),
                "i8" => json!(r.take(1)?[0] as i8),
                "u16" => json!(u16::from_le_bytes(r.arr()?)),
                "i16" => json!(i16::from_le_bytes(r.arr()?)),
                "u32" => json!(u32::from_le_bytes(r.arr()?)),
                "i32" => json!(i32::from_le_bytes(r.arr()?)),
                "u64" => json!(u64::from_le_bytes(r.arr()?)),
                "i64" => json!(i64::from_le_bytes(r.arr()?)),
                // 128-bit values exceed JSON number precision: keep them as strings.
                "u128" => json!(u128::from_le_bytes(r.arr()?).to_string()),
                "i128" => json!(i128::from_le_bytes(r.arr()?).to_string()),
                "pubkey" => json!(bs58::encode(r.take(32)?).into_string()),
                "string" => {
                    let n = u32::from_le_bytes(r.arr()?) as usize;
                    json!(String::from_utf8_lossy(r.take(n)?))
                }
                "bytes" => {
                    let n = u32::from_le_bytes(r.arr()?) as usize;
                    json!(hex(r.take(n)?))
                }
                other => bail!("unsupported primitive {other}"),
            });
        }
        let (k, v) = ty.as_object().and_then(|o| o.iter().next()).ok_or_else(|| anyhow!("bad type {ty}"))?;
        match k.as_str() {
            "option" => {
                if r.take(1)?[0] == 0 { Ok(Value::Null) } else { self.read(v, r) }
            }
            "vec" => {
                let n = u32::from_le_bytes(r.arr()?) as usize;
                (0..n).map(|_| self.read(v, r)).collect::<Result<Vec<_>>>().map(Value::Array)
            }
            "array" => {
                let n = v[1].as_u64().ok_or_else(|| anyhow!("array len"))? as usize;
                (0..n).map(|_| self.read(&v[0], r)).collect::<Result<Vec<_>>>().map(Value::Array)
            }
            "defined" => {
                let name = v["name"].as_str().or(v.as_str()).ok_or_else(|| anyhow!("defined name"))?;
                let def = self.types.get(name).ok_or_else(|| anyhow!("unknown type {name}"))?;
                match def["kind"].as_str() {
                    Some("struct") => {
                        let fields = def["fields"].as_array().cloned().unwrap_or_default();
                        if fields.iter().all(|f| f.is_object()) {
                            let mut m = Map::new();
                            for f in &fields {
                                m.insert(f["name"].as_str().unwrap_or("?").into(), self.read(&f["type"], r)?);
                            }
                            Ok(Value::Object(m))
                        } else {
                            // Tuple struct, e.g. OptionBool(bool): unwrap a single field.
                            let vals = fields.iter().map(|f| self.read(f, r)).collect::<Result<Vec<_>>>()?;
                            Ok(if vals.len() == 1 { vals.into_iter().next().unwrap() } else { Value::Array(vals) })
                        }
                    }
                    Some("enum") => {
                        let idx = r.take(1)?[0] as usize;
                        let var = &def["variants"][idx];
                        if var.get("fields").is_some() {
                            bail!("enum variants with fields are not used by these IDLs");
                        }
                        Ok(json!(var["name"].as_str().unwrap_or("?")))
                    }
                    _ => bail!("unsupported kind for {name}"),
                }
            }
            other => bail!("unsupported type form {other}"),
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.i + n > self.b.len() {
            bail!("unexpected end of data");
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The three programs, loaded from one pinned IDL commit directory.
pub struct Registry {
    pub programs: Vec<Program>,
    pub idl_commit: String,
}

impl Registry {
    pub fn load(dir: &Path) -> Result<Registry> {
        let programs = vec![
            Program::load(&dir.join("pump.json"), "pump")?,
            Program::load(&dir.join("pump_amm.json"), "pump_amm")?,
            Program::load(&dir.join("pump_fees.json"), "pump_fees")?,
        ];
        let idl_commit = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Registry { programs, idl_commit })
    }
    pub fn get(&self, address: &str) -> Option<&Program> {
        self.programs.iter().find(|p| p.address == address)
    }
}
