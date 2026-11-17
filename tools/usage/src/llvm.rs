//! LLVM's tools, from the kernel toolchain's `llvm-tools` (the version that compiled the usage
//! build), and the parts of `llvm-cov export`'s JSON the report reads.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

/// One coverage segment: `[line, column, count, has count, region entry, gap]`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct Segment(pub u64, pub u64, pub u64, pub bool, pub bool, pub bool);

impl Segment {
    pub fn line(&self) -> u64 {
        self.0
    }
    pub fn col(&self) -> u64 {
        self.1
    }
    pub fn count(&self) -> u64 {
        self.2
    }
    /// A counted region starts here (not a gap, not code left out).
    pub fn starts_region(&self) -> bool {
        self.3 && self.4 && !self.5
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Counts {
    pub count: u64,
    pub covered: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Summary {
    #[serde(default)]
    pub lines: Counts,
    #[serde(default)]
    pub functions: Counts,
    #[serde(default)]
    pub regions: Counts,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct File {
    pub filename: String,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub summary: Summary,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub count: u64,
    /// `[line start, column start, line end, column end, count, file id, expanded file id,
    /// kind]`.
    #[serde(default)]
    pub regions: Vec<Vec<u64>>,
    #[serde(default)]
    pub filenames: Vec<String>,
}

impl Function {
    /// The function's own file.
    pub fn file(&self) -> &str {
        self.filenames.first().map_or("", String::as_str)
    }

    /// Its first and last line in its own file.
    pub fn span(&self) -> Option<(u64, u64)> {
        let own = self
            .regions
            .iter()
            .filter(|r| r.len() >= 6 && r[5] == 0)
            .map(|r| (r[0], r[2]));
        let first = own.clone().map(|(a, _)| a).min()?;
        let last = own.map(|(_, b)| b).max()?;
        Some((first, last))
    }

    /// Its lines, from its span.
    pub fn lines(&self) -> u64 {
        self.span().map_or(0, |(a, b)| b + 1 - a)
    }

    /// Its demangled name, without crate hashes.
    pub fn demangled(&self) -> String {
        demangle(&self.name)
    }
}

/// A Rust symbol's readable name, without hashes; a local function's `file:` prefix dropped.
pub fn demangle(name: &str) -> String {
    let name = match name.rsplit_once(':') {
        Some((file, rest)) if file.contains('/') || file.ends_with(".rs") => rest,
        _ => name,
    };
    format!("{:#}", rustc_demangle::demangle(name))
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Export {
    #[serde(default)]
    pub files: Vec<File>,
    #[serde(default)]
    pub functions: Vec<Function>,
    #[serde(default)]
    pub totals: Summary,
}

#[derive(Deserialize)]
struct Document {
    data: Vec<Export>,
}

/// The export of `llvm-cov export -format=text`.
pub fn parse_export(json: &[u8]) -> Result<Export, String> {
    let doc: Document =
        serde_json::from_slice(json).map_err(|e| format!("llvm-cov export: {e}"))?;
    doc.data
        .into_iter()
        .next()
        .ok_or_else(|| "llvm-cov export: no data".to_string())
}

impl Export {
    /// The file whose name ends with `suffix`.
    pub fn file(&self, suffix: &str) -> Option<&File> {
        self.files.iter().find(|f| f.filename.ends_with(suffix))
    }
}

impl File {
    /// The count of the first counted region that starts on `line` after column `col`, or on
    /// one of the next two lines: a match arm's body.
    pub fn count_after(&self, line: u64, col: u64) -> Option<u64> {
        self.segments
            .iter()
            .filter(|s| s.starts_region())
            .find(|s| {
                (s.line() == line && s.col() > col) || (s.line() > line && s.line() <= line + 2)
            })
            .map(Segment::count)
    }
}

/// What the report runs: `llvm-profdata merge`, `llvm-cov export` and `llvm-objdump -d`.
pub trait Tools {
    /// Merges raw or indexed profiles into one indexed profile at `out`.
    fn merge(&self, inputs: &[PathBuf], out: &Path) -> Result<(), String>;
    /// The coverage export of `binary` under `profile`.
    fn export(&self, binary: &Path, profile: &Path) -> Result<Export, String>;
    /// The disassembly of `binary`.
    fn disassemble(&self, binary: &Path) -> Result<String, String>;
    /// The functions an indexed profile has records for: those compiled into the binary.
    /// `llvm-cov` also shows generic functions never instantiated, which are not in the image.
    fn functions(&self, profile: &Path) -> Result<std::collections::BTreeSet<String>, String>;
}

/// The function names in `llvm-profdata show --all-functions` output: the lines indented by two
/// spaces that end with `:`.
pub fn profile_functions(text: &str) -> std::collections::BTreeSet<String> {
    text.lines()
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .filter_map(|l| l.trim().strip_suffix(':'))
        .map(str::to_string)
        .collect()
}

/// The tools in one directory.
pub struct Llvm {
    pub bin: PathBuf,
}

fn run(command: &mut Command) -> Result<Vec<u8>, String> {
    let shown = format!("{command:?}");
    let out = command
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{shown}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{shown}: {}: {}", out.status, err.trim()));
    }
    Ok(out.stdout)
}

impl Tools for Llvm {
    fn merge(&self, inputs: &[PathBuf], out: &Path) -> Result<(), String> {
        run(Command::new(self.bin.join("llvm-profdata"))
            .args(["merge", "-sparse"])
            .args(inputs)
            .arg("-o")
            .arg(out))
        .map(|_| ())
    }

    fn export(&self, binary: &Path, profile: &Path) -> Result<Export, String> {
        let json = run(Command::new(self.bin.join("llvm-cov"))
            .args([
                "export",
                "-format=text",
                "-skip-expansions",
                "-skip-branches",
            ])
            .arg(format!("-instr-profile={}", profile.display()))
            .arg(binary))?;
        parse_export(&json)
    }

    fn disassemble(&self, binary: &Path) -> Result<String, String> {
        let text = run(Command::new(self.bin.join("llvm-objdump"))
            .args(["-d", "--no-show-raw-insn"])
            .arg(binary))?;
        Ok(String::from_utf8_lossy(&text).into_owned())
    }

    fn functions(&self, profile: &Path) -> Result<std::collections::BTreeSet<String>, String> {
        let text = run(Command::new(self.bin.join("llvm-profdata"))
            .args(["show", "--all-functions"])
            .arg(profile))?;
        Ok(profile_functions(&String::from_utf8_lossy(&text)))
    }
}

/// Where the LLVM tools are: `--llvm DIR`, else `MORUNA_LLVM_BIN`, else the active rustc's
/// sysroot's `lib/rustlib/HOST/bin`.
pub fn locate(given: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(dir) = given.or_else(|| std::env::var_os("MORUNA_LLVM_BIN").map(PathBuf::from)) {
        return Ok(dir);
    }
    let sysroot = run(Command::new("rustc").args(["--print", "sysroot"]))?;
    let host = run(Command::new("rustc").arg("-vV"))?;
    let host = String::from_utf8_lossy(&host)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
        .ok_or("rustc -vV named no host")?;
    let sysroot = String::from_utf8_lossy(&sysroot).trim().to_string();
    Ok(Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host)
        .join("bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = r#"{"data":[{"files":[{"filename":"/src/kernel/core/src/syscall.rs",
        "segments":[[10,5,7,true,true,false],[10,30,9,true,true,false],[10,40,0,false,false,false],
        [12,9,0,true,true,true],[13,9,4,true,true,false]],
        "summary":{"lines":{"count":10,"covered":4,"percent":40},"functions":{"count":2,"covered":1},
        "regions":{"count":5,"covered":3}}}],
        "functions":[{"name":"_RNvNtCs7n3AVLcjyHa_18moruna_microkernel7syscall8dispatch","count":7,
        "regions":[[9,1,20,2,7,0,0,0],[30,1,31,2,7,1,0,0]],"filenames":["/src/kernel/core/src/syscall.rs"]}],
        "totals":{"lines":{"count":10,"covered":4}}}],"type":"llvm.coverage.json.export"}"#;

    #[test]
    fn an_export_is_read_and_an_arm_counted() {
        let e = parse_export(EXPORT.as_bytes()).unwrap();
        let f = e.file("core/src/syscall.rs").unwrap();
        assert_eq!(
            f.summary.lines,
            Counts {
                count: 10,
                covered: 4
            }
        );
        assert_eq!(f.count_after(10, 20), Some(9));
        assert_eq!(f.count_after(10, 2), Some(7));
        // A gap region is not an arm's body; the next counted line is.
        assert_eq!(f.count_after(11, 50), Some(4));
        assert_eq!(f.count_after(20, 1), None);
        assert!(e.file("nothing.rs").is_none());
        let func = &e.functions[0];
        assert_eq!(func.demangled(), "moruna_microkernel::syscall::dispatch");
        assert_eq!(
            (func.span(), func.lines(), func.file()),
            (Some((9, 20)), 12, "/src/kernel/core/src/syscall.rs")
        );
        assert_eq!(e.totals.lines.covered, 4);
    }

    #[test]
    fn a_bad_export_is_an_error() {
        assert!(
            parse_export(b"{")
                .unwrap_err()
                .starts_with("llvm-cov export")
        );
        assert_eq!(
            parse_export(br#"{"data":[]}"#).unwrap_err(),
            "llvm-cov export: no data"
        );
        let f = Function {
            name: "x".into(),
            count: 0,
            regions: vec![],
            filenames: vec![],
        };
        assert_eq!((f.span(), f.lines(), f.file()), (None, 0, ""));
    }

    #[test]
    fn names_are_demangled_without_hashes_or_files() {
        assert_eq!(
            demangle("_ZN4core3ptr4read17h0123456789abcdefE"),
            "core::ptr::read"
        );
        assert_eq!(
            demangle("kernel/core/src/vm.rs:_RNvNtCs7n3AVLcjyHa_18moruna_microkernel2vm5fault"),
            "moruna_microkernel::vm::fault"
        );
        assert_eq!(demangle("plain"), "plain");
    }

    #[test]
    fn the_tools_are_run_and_their_failure_named() {
        let llvm = Llvm {
            bin: PathBuf::from("/nonexistent/bin"),
        };
        assert!(
            llvm.merge(&[], Path::new("/tmp/x"))
                .unwrap_err()
                .contains("llvm-profdata")
        );
        assert!(
            llvm.export(Path::new("a"), Path::new("b"))
                .unwrap_err()
                .contains("llvm-cov")
        );
        assert!(
            llvm.disassemble(Path::new("a"))
                .unwrap_err()
                .contains("llvm-objdump")
        );
        assert!(
            llvm.functions(Path::new("a"))
                .unwrap_err()
                .contains("llvm-profdata")
        );
        let shown = "Counters:\n  _RNvA:\n    Hash: 0x1\n    Counters: 1\n  src/x.rs:_RNvB:\n    Function count: 0\nInstrumentation level: Front-end\n";
        assert_eq!(
            profile_functions(shown),
            ["_RNvA".to_string(), "src/x.rs:_RNvB".to_string()].into()
        );
        let failed = run(&mut Command::new("false")).unwrap_err();
        assert!(failed.contains("exit status"), "{failed}");
        assert_eq!(
            locate(Some(PathBuf::from("/x"))).unwrap(),
            PathBuf::from("/x")
        );
        assert!(locate(None).unwrap().ends_with("bin"));
    }
}
