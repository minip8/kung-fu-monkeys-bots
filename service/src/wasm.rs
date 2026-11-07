//! The module checks the host makes at load (the engine's `docs/bot-abi.md`
//! §1, `WasmModule::load`), from the binary's sections alone: a small walker
//! over the type, import, function, memory and export sections, no wasm
//! runtime. The smoke match then runs the real host.
//!
//! - at most [`MAX_MODULE_BYTES`];
//! - imports: only the function `env.kfm_log : (i32, i32) -> ()`; no imported
//!   memory, table or global;
//! - one memory of its own, initial size at most [`MAX_INITIAL_PAGES`] (16 MiB), not 64-bit;
//! - exports `memory` (that memory) and the four ABI functions with their
//!   signatures: `kfm_abi_version : () -> i32`, `kfm_obs_buffer : () -> i32`,
//!   `kfm_init : (i64, i32) -> ()`, `kfm_act : (i32) -> i64`. Other exports
//!   (`__stack_pointer`) are allowed.

use std::collections::HashMap;

pub const MAX_MODULE_BYTES: usize = 4 * 1024 * 1024;
/// 16 MiB of 64 KiB pages.
pub const MAX_INITIAL_PAGES: u64 = 256;

const I32: u8 = 0x7f;
const I64: u8 = 0x7e;

/// What the checks found in a module that passed them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    pub bytes: usize,
    pub initial_pages: u64,
    pub imports_log: bool,
    pub exports: Vec<String>,
}

type FuncType = (Vec<u8>, Vec<u8>);

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn byte(&mut self) -> Result<u8, String> {
        let b = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| "the module ends early".to_owned())?;
        self.at += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| "the module ends early".to_owned())?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    /// An unsigned LEB128 of at most `bits` bits.
    fn leb(&mut self, bits: u32) -> Result<u64, String> {
        let mut value: u64 = 0;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            if shift >= bits {
                return Err("a LEB128 number is too long".to_owned());
            }
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                if bits < 64 && value >> bits != 0 {
                    return Err("a LEB128 number is out of range".to_owned());
                }
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(self.leb(32)? as u32)
    }

    fn name(&mut self) -> Result<String, String> {
        let len = self.u32()? as usize;
        let raw = self.take(len)?;
        String::from_utf8(raw.to_vec()).map_err(|_| "a name is not UTF-8".to_owned())
    }

    /// Limits: (initial, maximum, 64-bit, shared).
    fn limits(&mut self) -> Result<(u64, Option<u64>, bool), String> {
        let flags = self.byte()?;
        if flags & !0x07 != 0 {
            return Err(format!("unknown limits flags {flags:#x}"));
        }
        let wide = flags & 0x04 != 0;
        let bits = if wide { 64 } else { 32 };
        let min = self.leb(bits)?;
        let max = if flags & 0x01 != 0 {
            Some(self.leb(bits)?)
        } else {
            None
        };
        Ok((min, max, wide))
    }

    fn valtype(&mut self) -> Result<u8, String> {
        let t = self.byte()?;
        match t {
            0x7f | 0x7e | 0x7d | 0x7c | 0x7b | 0x70 | 0x6f => Ok(t),
            _ => Err(format!("unsupported value type {t:#x}")),
        }
    }
}

fn describe(ty: &FuncType) -> String {
    let names = |ts: &[u8]| {
        ts.iter()
            .map(|t| match *t {
                I32 => "i32",
                I64 => "i64",
                0x7d => "f32",
                0x7c => "f64",
                0x7b => "v128",
                0x70 => "funcref",
                _ => "externref",
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("({}) -> ({})", names(&ty.0), names(&ty.1))
}

/// The ABI's exported functions and their signatures.
const ABI_EXPORTS: [(&str, &[u8], &[u8]); 4] = [
    ("kfm_abi_version", &[], &[I32]),
    ("kfm_obs_buffer", &[], &[I32]),
    ("kfm_init", &[I64, I32], &[]),
    ("kfm_act", &[I32], &[I64]),
];

/// Checks `bytes` as the host would at load; the error names the first rule broken.
pub fn check(bytes: &[u8]) -> Result<ModuleInfo, String> {
    if bytes.len() > MAX_MODULE_BYTES {
        return Err(format!(
            "the module is {} bytes, over the 4 MiB cap ({MAX_MODULE_BYTES} bytes)",
            bytes.len()
        ));
    }
    let mut r = Reader::new(bytes);
    if r.take(4).ok() != Some(b"\0asm") {
        return Err("not a WebAssembly module (no \\0asm magic)".to_owned());
    }
    if r.take(4).ok() != Some(&[1, 0, 0, 0]) {
        return Err("not a WebAssembly 1.0 core module (version)".to_owned());
    }

    let mut types: Vec<FuncType> = Vec::new();
    // Type index of every function, imported ones first.
    let mut funcs: Vec<u32> = Vec::new();
    let mut imports_log = false;
    let mut memories: Vec<(u64, bool)> = Vec::new();
    let mut exports: HashMap<String, (u8, u32)> = HashMap::new();
    let mut export_order = Vec::new();
    let mut last_id = 0u8;

    while !r.done() {
        let id = r.byte()?;
        let size = r.u32()? as usize;
        let body = r.take(size)?;
        if id != 0 {
            if id <= last_id && id <= 11 {
                return Err(format!("section {id} is out of order or repeated"));
            }
            if id <= 11 {
                last_id = id;
            }
        }
        let mut s = Reader::new(body);
        match id {
            1 => {
                for _ in 0..s.u32()? {
                    let form = s.byte()?;
                    if form != 0x60 {
                        return Err(format!("unsupported type form {form:#x}"));
                    }
                    let params = (0..s.u32()?)
                        .map(|_| s.valtype())
                        .collect::<Result<_, _>>()?;
                    let results = (0..s.u32()?)
                        .map(|_| s.valtype())
                        .collect::<Result<_, _>>()?;
                    types.push((params, results));
                }
            }
            2 => {
                for _ in 0..s.u32()? {
                    let module = s.name()?;
                    let field = s.name()?;
                    let kind = s.byte()?;
                    let what = format!("{module}.{field}");
                    match kind {
                        0x00 => {
                            let ty = s.u32()?;
                            if what != "env.kfm_log" {
                                return Err(format!(
                                    "imports {what}: the only import allowed is env.kfm_log"
                                ));
                            }
                            let sig = types
                                .get(ty as usize)
                                .ok_or_else(|| format!("{what} names a missing type"))?;
                            if sig.0 != [I32, I32] || !sig.1.is_empty() {
                                return Err(format!(
                                    "imports env.kfm_log as {}, want (i32, i32) -> ()",
                                    describe(sig)
                                ));
                            }
                            imports_log = true;
                            funcs.push(ty);
                        }
                        0x02 => return Err(format!("imports its memory ({what})")),
                        0x01 => return Err(format!("imports a table ({what})")),
                        0x03 => return Err(format!("imports a global ({what})")),
                        _ => return Err(format!("imports {what} of an unknown kind {kind:#x}")),
                    }
                }
            }
            3 => {
                for _ in 0..s.u32()? {
                    funcs.push(s.u32()?);
                }
            }
            5 => {
                for _ in 0..s.u32()? {
                    let (min, _, wide) = s.limits()?;
                    memories.push((min, wide));
                }
            }
            7 => {
                for _ in 0..s.u32()? {
                    let name = s.name()?;
                    let kind = s.byte()?;
                    let index = s.u32()?;
                    if exports.insert(name.clone(), (kind, index)).is_some() {
                        return Err(format!("exports {name} twice"));
                    }
                    export_order.push(name);
                }
            }
            _ => {}
        }
    }

    let (initial_pages, wide) = match memories.as_slice() {
        [] => return Err("the module has no linear memory of its own".to_owned()),
        [one] => *one,
        _ => return Err("the module has more than one memory".to_owned()),
    };
    if wide {
        return Err("the module's memory is 64-bit (memory64)".to_owned());
    }
    if initial_pages > MAX_INITIAL_PAGES {
        return Err(format!(
            "the initial memory is {initial_pages} pages, over the 16 MiB cap ({MAX_INITIAL_PAGES} pages)"
        ));
    }
    match exports.get("memory") {
        Some((0x02, 0)) => {}
        Some(_) => return Err("the export memory is not the module's memory".to_owned()),
        None => return Err("the module does not export memory".to_owned()),
    }
    for (name, params, results) in ABI_EXPORTS {
        let (kind, index) = *exports
            .get(name)
            .ok_or_else(|| format!("the module does not export {name}"))?;
        if kind != 0x00 {
            return Err(format!("the export {name} is not a function"));
        }
        let ty = funcs
            .get(index as usize)
            .and_then(|&t| types.get(t as usize))
            .ok_or_else(|| format!("the export {name} names a missing function"))?;
        if ty.0 != params || ty.1 != results {
            let want = describe(&(params.to_vec(), results.to_vec()));
            return Err(format!(
                "the export {name} is {}, want {want}",
                describe(ty)
            ));
        }
    }
    Ok(ModuleInfo {
        bytes: bytes.len(),
        initial_pages,
        imports_log,
        exports: export_order,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leb(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn section(id: u8, body: Vec<u8>) -> Vec<u8> {
        let mut out = vec![id];
        out.extend(leb(body.len() as u64));
        out.extend(body);
        out
    }

    fn name(s: &str) -> Vec<u8> {
        let mut out = leb(s.len() as u64);
        out.extend(s.as_bytes());
        out
    }

    fn vec_of(items: Vec<Vec<u8>>) -> Vec<u8> {
        let mut out = leb(items.len() as u64);
        for item in items {
            out.extend(item);
        }
        out
    }

    /// A module crafted section by section.
    struct Crafted {
        imports: Vec<Vec<u8>>,
        /// Imported functions, for the function index space.
        imported_funcs: u32,
        memory: Option<Vec<u8>>,
        exports: Vec<(&'static str, u8, u32)>,
        act_type: u32,
    }

    impl Default for Crafted {
        fn default() -> Crafted {
            Crafted {
                imports: vec![[name("env"), name("kfm_log"), vec![0x00, 3]].concat()],
                imported_funcs: 1,
                memory: Some(vec![0x00, 2]),
                exports: vec![
                    ("memory", 0x02, 0),
                    ("kfm_abi_version", 0x00, 0),
                    ("kfm_obs_buffer", 0x00, 0),
                    ("kfm_init", 0x00, 1),
                    ("kfm_act", 0x00, 2),
                ],
                act_type: 2,
            }
        }
    }

    impl Crafted {
        fn build(&self) -> Vec<u8> {
            // Types: 0 () -> i32, 1 (i64, i32) -> (), 2 (i32) -> i64, 3 (i32, i32) -> ().
            let types = vec_of(vec![
                vec![0x60, 0, 1, I32],
                vec![0x60, 2, I64, I32, 0],
                vec![0x60, 1, I32, 1, I64],
                vec![0x60, 2, I32, I32, 0],
            ]);
            let mut out = b"\0asm\x01\0\0\0".to_vec();
            out.extend(section(1, types));
            out.extend(section(2, vec_of(self.imports.clone())));
            // Defined functions: abi_version/obs_buffer share type 0, init 1, act.
            let funcs = vec_of(vec![leb(0), leb(1), leb(u64::from(self.act_type))]);
            out.extend(section(3, funcs));
            if let Some(memory) = &self.memory {
                out.extend(section(5, vec_of(vec![memory.clone()])));
            }
            let exports = self
                .exports
                .iter()
                .map(|(n, kind, index)| {
                    let index = if *kind == 0x00 {
                        index + self.imported_funcs
                    } else {
                        *index
                    };
                    [name(n), vec![*kind], leb(u64::from(index))].concat()
                })
                .collect();
            out.extend(section(7, vec_of(exports)));
            // A custom section is skipped.
            out.extend(section(0, [name("producers"), vec![1, 2, 3]].concat()));
            out
        }
    }

    #[test]
    fn a_crafted_good_module_passes() {
        let info = check(&Crafted::default().build()).unwrap();
        assert!(info.imports_log);
        assert_eq!(info.initial_pages, 2);
        assert_eq!(info.exports.len(), 5);
        // Without the optional import too.
        let alone = Crafted {
            imports: vec![],
            imported_funcs: 0,
            ..Crafted::default()
        };
        assert!(!check(&alone.build()).unwrap().imports_log);
    }

    fn refused(module: Crafted) -> String {
        check(&module.build()).expect_err("the module should be refused")
    }

    #[test]
    fn an_extra_import_is_refused() {
        let mut m = Crafted::default();
        m.imports
            .push([name("env"), name("memcpy"), vec![0x00, 3]].concat());
        m.imported_funcs = 2;
        let err = refused(m);
        assert!(err.contains("env.memcpy"), "{err}");
    }

    #[test]
    fn imported_memory_is_refused() {
        let m = Crafted {
            imports: vec![[name("env"), name("memory"), vec![0x02, 0x00, 1]].concat()],
            imported_funcs: 0,
            memory: None,
            ..Crafted::default()
        };
        let err = refused(m);
        assert!(err.contains("imports its memory"), "{err}");
    }

    #[test]
    fn a_missing_export_is_refused() {
        let mut m = Crafted::default();
        m.exports.retain(|(n, _, _)| *n != "kfm_act");
        assert!(refused(m).contains("does not export kfm_act"));
        let mut m = Crafted::default();
        m.exports.retain(|(n, _, _)| *n != "memory");
        assert!(refused(m).contains("does not export memory"));
    }

    #[test]
    fn a_wrong_signature_is_refused() {
        let m = Crafted {
            act_type: 0,
            ..Crafted::default()
        };
        let err = refused(m);
        assert!(
            err.contains("kfm_act is () -> (i32), want (i32) -> (i64)"),
            "{err}"
        );
        let m = Crafted {
            imports: vec![[name("env"), name("kfm_log"), vec![0x00, 0]].concat()],
            ..Crafted::default()
        };
        assert!(refused(m).contains("want (i32, i32) -> ()"));
    }

    #[test]
    fn memory_caps_are_enforced() {
        let m = Crafted {
            memory: Some([vec![0x00], leb(257)].concat()),
            ..Crafted::default()
        };
        assert!(refused(m).contains("257 pages"));
        let m = Crafted {
            memory: Some([vec![0x01], leb(256), leb(65536)].concat()),
            ..Crafted::default()
        };
        assert_eq!(check(&m.build()).unwrap().initial_pages, 256);
        let m = Crafted {
            memory: None,
            ..Crafted::default()
        };
        assert!(refused(m).contains("no linear memory"));
        let m = Crafted {
            memory: Some(vec![0x04, 1]),
            ..Crafted::default()
        };
        assert!(refused(m).contains("64-bit"));
    }

    #[test]
    fn garbage_and_oversize_are_refused() {
        assert!(check(b"\0asm").unwrap_err().contains("version"));
        assert!(check(b"hello world!").unwrap_err().contains("magic"));
        let mut cut = Crafted::default().build();
        cut.truncate(cut.len() - 20);
        assert!(check(&cut).unwrap_err().contains("ends early"));
        let big = vec![0u8; MAX_MODULE_BYTES + 1];
        assert!(check(&big).unwrap_err().contains("4 MiB"));
    }

    /// The examples, built by the recipe (tools/build.sh) when a wasm clang is
    /// around: `KFM_LADDER_TEST_EXAMPLES` names a directory of `<name>.wasm`
    /// (CI's out/local), else the test looks in ../out/local.
    #[test]
    fn the_examples_built_by_the_recipe_pass() {
        let dir = std::env::var("KFM_LADDER_TEST_EXAMPLES")
            .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../out/local").to_owned());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("the_examples_built_by_the_recipe_pass: skipped: no {dir} (tools/build.sh examples/<name> out/local/<name>.wasm)");
            return;
        };
        let mut seen = 0;
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "wasm") {
                let bytes = std::fs::read(&path).unwrap();
                let info = check(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(info.exports.iter().any(|e| e == "__stack_pointer"));
                seen += 1;
            }
        }
        eprintln!("checked {seen} example modules in {dir}");
    }
}
