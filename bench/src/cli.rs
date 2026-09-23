//! The command line.
//!
//! `clap` is not in the preamble's dependency table (6.2) and adding a crate the
//! table lacks is an E2 item, so the parser is written here: a command, then
//! `--key value` or `--key=value` pairs, then a bare `--flag` for a switch.
//! Numbers may carry `_` separators, as in `--rows 2_000_000`.

use std::path::PathBuf;

use crate::dataset::{Dataset, DatasetKind};
use crate::dtype::DType;
use crate::error::{BenchError, Result};
use crate::mrb1::TensorSpec;
use crate::parquet_out::{Codec, ParquetSpec};
use crate::suite::{self, Scale};

/// The default seed: the day the build started.
pub const DEFAULT_SEED: u64 = 20_260_922;

/// The default output directory, relative to the repository root.
pub const DEFAULT_OUT: &str = "bench/data";

/// What a parsed command line asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Print the usage text.
    Help,
    /// Print the generator version and the machine.
    Version,
    /// Print the suite without writing anything.
    List,
    /// Write these datasets.
    Write(Vec<Dataset>),
    /// Run one kernel over one already written dataset and report its
    /// amplification (preamble 6.5).
    Measure(KernelRun),
}

/// One run of one kernel over one dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct KernelRun {
    /// The kernel's name, one of `kernels::KERNEL_NAMES`.
    pub kernel: String,
    /// The dataset it reads, without the `.parquet` suffix.
    pub dataset: String,
    /// The safetensors weights file, which `embed-score` needs.
    pub weights: Option<PathBuf>,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// What to do.
    pub action: Action,
    /// Where files are written.
    pub out: PathBuf,
    /// The seed every value is drawn from.
    pub seed: u64,
    /// The suite scale.
    pub scale: Scale,
    /// Whether to upload to the S3 compatible store when the environment is set.
    pub upload: bool,
}

/// The usage text, printed by `help` and beside every usage error.
pub fn usage() -> String {
    format!(
        "moruna-bench, the Moruna benchmark data generator (preamble 6.5)

usage: moruna-bench <command> [options]

commands:
  suite                     write every dataset of the suite
  dataset <name>            write one dataset of the suite
  list                      print the suite without writing
  parquet                   write one Parquet file from the options below
  safetensors               write one safetensors file from the options below
  mrb1                      write one MRB1 tensor file (contracts e.4)
  kernel <name>             run one kernel over its dataset and report the
                            amplification; names: {kernels}
  version                   print the generator version and this machine
  help                      print this text

common options:
  --out <dir>               output directory (default {DEFAULT_OUT})
  --seed <u64>              the seed every value is drawn from (default {DEFAULT_SEED})
  --scale full|small        suite size (default full; small is a few thousand rows)
  --local-only              skip the S3 upload even when the environment is set
  --name <name>             the dataset name, and the file stem, for the ad hoc commands

parquet options:
  --rows <n>                row count (default 16384)
  --int-cols <n>            i64 columns (default 2)
  --float-cols <n>          f64 columns (default 2)
  --string-cols <n>         short string columns (default 1)
  --text-cols <n>           long text columns (default 1)
  --text-mean-len <bytes>   mean long text length (default 256)
  --text-len-stddev <b>     its standard deviation (default 96)
  --text-len-variance <b>   the same, given as a variance
  --null-ratio <0..1>       probability that a value is null (default 0)
  --row-group-rows <n>      rows per row group (default 8192)
  --compression <codec>     uncompressed or snappy (default uncompressed)

kernel options:
  --dataset <name>          the dataset to read (default: the one bench/README.md
                            pairs with the kernel)
  --weights <file>          the safetensors weights embed-score loads (default
                            <out>/{weights}.safetensors)

tensor options (safetensors and mrb1):
  --dtype <name>            one of i8 i16 i32 i64 u8 u16 u32 u64 f16 bf16 f32 f64 bool
  --shape <d0,d1,..>        up to 8 dimensions; empty is a scalar
  --tensor <name:dtype:shape>  one tensor of a safetensors file, repeatable

The S3 compatible half writes the same files to {}, under MORUNA_S3_PREFIX
(default bench). With any of those unset the upload is skipped with a note.",
        crate::s3::REQUIRED.join(", "),
        kernels = crate::kernels::KERNEL_NAMES.join(", "),
        weights = crate::kernels::runner::DEFAULT_WEIGHTS,
    )
}

/// The flags of one command line, consumed as they are read.
struct Flags {
    pairs: Vec<(String, Option<String>)>,
    positionals: Vec<String>,
}

impl Flags {
    fn split(args: &[String]) -> Flags {
        let mut pairs = Vec::new();
        let mut positionals = Vec::new();
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            match arg.strip_prefix("--") {
                Some(body) => {
                    if let Some((key, value)) = body.split_once('=') {
                        pairs.push((key.to_string(), Some(value.to_string())));
                        index += 1;
                    } else if index + 1 < args.len() && !args[index + 1].starts_with("--") {
                        pairs.push((body.to_string(), Some(args[index + 1].clone())));
                        index += 2;
                    } else {
                        pairs.push((body.to_string(), None));
                        index += 1;
                    }
                }
                None => {
                    positionals.push(arg.clone());
                    index += 1;
                }
            }
        }
        Flags { pairs, positionals }
    }

    /// Take every occurrence of a key, in the order it was given.
    fn take_all(&mut self, key: &str) -> Vec<Option<String>> {
        let mut taken = Vec::new();
        let mut kept = Vec::with_capacity(self.pairs.len());
        for (name, value) in std::mem::take(&mut self.pairs) {
            if name == key {
                taken.push(value);
            } else {
                kept.push((name, value));
            }
        }
        self.pairs = kept;
        taken
    }

    fn take(&mut self, key: &str) -> Result<Option<String>> {
        let mut values = self.take_all(key);
        match values.len() {
            0 => Ok(None),
            1 => match values.remove(0) {
                Some(value) => Ok(Some(value)),
                None => Err(BenchError::Usage(format!("--{key} needs a value"))),
            },
            n => Err(BenchError::Usage(format!("--{key} was given {n} times"))),
        }
    }

    fn switch(&mut self, key: &str) -> Result<bool> {
        match self.take_all(key).len() {
            0 => Ok(false),
            1 => Ok(true),
            n => Err(BenchError::Usage(format!("--{key} was given {n} times"))),
        }
    }

    fn number<T: std::str::FromStr>(&mut self, key: &str, default: T) -> Result<T> {
        match self.take(key)? {
            None => Ok(default),
            Some(text) => text
                .replace('_', "")
                .parse::<T>()
                .map_err(|_| BenchError::Usage(format!("--{key} {text} is not a number"))),
        }
    }

    fn finish(self) -> Result<()> {
        if let Some((key, _)) = self.pairs.first() {
            return Err(BenchError::Usage(format!("unknown option --{key}")));
        }
        if let Some(extra) = self.positionals.first() {
            return Err(BenchError::Usage(format!("unexpected argument {extra}")));
        }
        Ok(())
    }
}

fn parse_shape(text: &str) -> Result<Vec<i64>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut shape = Vec::new();
    for part in trimmed.split(',') {
        let value = part.trim().replace('_', "");
        let dim = value
            .parse::<i64>()
            .map_err(|_| BenchError::Shape(format!("shape {text}: {part} is not an integer")))?;
        shape.push(dim);
    }
    Ok(shape)
}

fn parse_tensor(text: &str) -> Result<TensorSpec> {
    let parts: Vec<&str> = text.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err(BenchError::Shape(format!(
            "--tensor {text}: expected name:dtype:shape, for example weight:f32:128,64"
        )));
    }
    let shape = if parts.len() == 3 {
        parse_shape(parts[2])?
    } else {
        Vec::new()
    };
    TensorSpec::new(parts[0], DType::parse(parts[1])?, shape)
}

fn parquet_spec(flags: &mut Flags) -> Result<ParquetSpec> {
    let base = ParquetSpec::default();
    let stddev = match flags.take("text-len-variance")? {
        Some(text) => {
            let variance: f64 = text.replace('_', "").parse().map_err(|_| {
                BenchError::Usage(format!("--text-len-variance {text} is not a number"))
            })?;
            if variance < 0.0 {
                return Err(BenchError::Shape(format!(
                    "text-len-variance {variance} is negative"
                )));
            }
            variance.sqrt()
        }
        None => base.text_len_stddev,
    };
    let spec = ParquetSpec {
        rows: flags.number("rows", base.rows)?,
        int_cols: flags.number("int-cols", base.int_cols)?,
        float_cols: flags.number("float-cols", base.float_cols)?,
        short_string_cols: flags.number("string-cols", base.short_string_cols)?,
        text_cols: flags.number("text-cols", base.text_cols)?,
        text_mean_len: flags.number("text-mean-len", base.text_mean_len)?,
        text_len_stddev: flags.number("text-len-stddev", stddev)?,
        null_ratio: flags.number("null-ratio", base.null_ratio)?,
        row_group_rows: flags.number("row-group-rows", base.row_group_rows)?,
        compression: match flags.take("compression")? {
            Some(text) => Codec::parse(&text)?,
            None => base.compression,
        },
    };
    spec.validate()?;
    Ok(spec)
}

/// Parse a command line, the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Plan> {
    let (command, rest) = match args.split_first() {
        Some((first, rest)) => (first.as_str(), rest),
        None => ("help", &[] as &[String]),
    };
    let command = match command {
        "-h" | "--help" | "-help" => "help",
        "-V" | "--version" => "version",
        other => other,
    };
    let mut flags = Flags::split(rest);
    let out = PathBuf::from(
        flags
            .take("out")?
            .unwrap_or_else(|| DEFAULT_OUT.to_string()),
    );
    let seed = flags.number("seed", DEFAULT_SEED)?;
    let scale = match flags.take("scale")? {
        Some(text) => Scale::parse(&text)?,
        None => Scale::Full,
    };
    let upload = !flags.switch("local-only")?;
    let name = flags.take("name")?;

    let action = match command {
        "help" => Action::Help,
        "version" => Action::Version,
        "list" => Action::List,
        "suite" => Action::Write(suite::datasets(scale)?),
        "dataset" => {
            if flags.positionals.len() != 1 {
                return Err(BenchError::Usage(
                    "dataset takes exactly one name, for example: dataset text-explode".to_string(),
                ));
            }
            let wanted = flags.positionals.remove(0);
            Action::Write(vec![suite::dataset(&wanted, scale)?])
        }
        "parquet" => {
            let spec = parquet_spec(&mut flags)?;
            Action::Write(vec![Dataset {
                name: name.clone().unwrap_or_else(|| "adhoc-parquet".to_string()),
                kind: DatasetKind::Parquet(spec),
            }])
        }
        "safetensors" => {
            let dataset_name = name.clone().unwrap_or_else(|| "adhoc-tensors".to_string());
            let mut specs = Vec::new();
            for value in flags.take_all("tensor") {
                match value {
                    Some(text) => specs.push(parse_tensor(&text)?),
                    None => return Err(BenchError::Usage("--tensor needs a value".to_string())),
                }
            }
            if specs.is_empty() {
                specs.push(single_tensor(&mut flags, "weight")?);
            }
            Action::Write(vec![Dataset {
                name: dataset_name,
                kind: DatasetKind::SafeTensors(specs),
            }])
        }
        "mrb1" => {
            let dataset_name = name.clone().unwrap_or_else(|| "adhoc-tensor".to_string());
            let spec = single_tensor(&mut flags, &dataset_name)?;
            Action::Write(vec![Dataset {
                name: dataset_name,
                kind: DatasetKind::Amb1(spec),
            }])
        }
        "kernel" => {
            if flags.positionals.len() != 1 {
                return Err(BenchError::Usage(format!(
                    "kernel takes exactly one name, one of: {}",
                    crate::kernels::KERNEL_NAMES.join(", ")
                )));
            }
            let kernel = flags.positionals.remove(0);
            let dataset = match flags.take("dataset")? {
                Some(dataset) => dataset,
                None => crate::kernels::runner::default_dataset(&kernel)?.to_string(),
            };
            let weights = match flags.take("weights")? {
                Some(path) => Some(PathBuf::from(path)),
                None if kernel == "embed-score" => Some(out.join(format!(
                    "{}.safetensors",
                    crate::kernels::runner::DEFAULT_WEIGHTS
                ))),
                None => None,
            };
            Action::Measure(KernelRun {
                kernel,
                dataset,
                weights,
            })
        }
        other => {
            return Err(BenchError::Usage(format!(
                "unknown command {other}; run moruna-bench help"
            )));
        }
    };
    flags.finish()?;
    Ok(Plan {
        action,
        out,
        seed,
        scale,
        upload,
    })
}

fn single_tensor(flags: &mut Flags, name: &str) -> Result<TensorSpec> {
    let dtype = match flags.take("dtype")? {
        Some(text) => DType::parse(&text)?,
        None => DType::F32,
    };
    let shape = match flags.take("shape")? {
        Some(text) => parse_shape(&text)?,
        None => vec![128, 64],
    };
    TensorSpec::new(name, dtype, shape)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn plan(line: &str) -> Plan {
        match parse(&args(line)) {
            Ok(plan) => plan,
            Err(err) => panic!("{line}: {err}"),
        }
    }

    #[test]
    fn no_arguments_and_the_help_spellings_print_the_usage() {
        assert_eq!(plan("").action, Action::Help);
        assert_eq!(plan("help").action, Action::Help);
        assert_eq!(plan("--help").action, Action::Help);
        assert_eq!(plan("-h").action, Action::Help);
        assert_eq!(plan("version").action, Action::Version);
        assert_eq!(plan("-V").action, Action::Version);
        let text = usage();
        assert!(text.contains("MORUNA_S3_ENDPOINT"), "{text}");
        assert!(text.contains("--row-group-rows"), "{text}");
        assert!(text.contains("contracts e.4"), "{text}");
    }

    #[test]
    fn the_kernel_command_names_a_kernel_its_dataset_and_its_weights() {
        let run = match plan("kernel identity").action {
            Action::Measure(run) => run,
            other => panic!("{other:?}"),
        };
        assert_eq!(run.kernel, "identity");
        assert_eq!(run.dataset, "identity-mixed");
        assert_eq!(run.weights, None);

        // embed-score is the one kernel that needs weights, so it gets the
        // default path under --out when none is given.
        let run = match plan("kernel embed-score --out /tmp/x").action {
            Action::Measure(run) => run,
            other => panic!("{other:?}"),
        };
        assert_eq!(run.dataset, "numeric-embed");
        assert_eq!(
            run.weights,
            Some(PathBuf::from(format!(
                "/tmp/x/{}.safetensors",
                crate::kernels::runner::DEFAULT_WEIGHTS
            )))
        );

        // Both overrides are read.
        let run = match plan("kernel normalise --dataset nulls-heavy --weights /tmp/w.safetensors")
            .action
        {
            Action::Measure(run) => run,
            other => panic!("{other:?}"),
        };
        assert_eq!(run.dataset, "nulls-heavy");
        assert_eq!(run.weights, Some(PathBuf::from("/tmp/w.safetensors")));
    }

    #[test]
    fn the_kernel_command_refuses_no_name_two_names_and_an_unknown_name() {
        for line in ["kernel", "kernel identity normalise"] {
            let err = parse(&args(line)).expect_err(line);
            assert!(err.to_string().contains("exactly one name"), "{err}");
        }
        let err = parse(&args("kernel torch-score")).expect_err("not built");
        assert!(err.to_string().contains("no kernel named"), "{err}");
        assert!(usage().contains("kernel <name>"), "{}", usage());
        assert!(usage().contains("--weights"), "{}", usage());
    }

    #[test]
    fn the_common_options_have_the_documented_defaults() {
        let listed = plan("list");
        assert_eq!(listed.out, PathBuf::from(DEFAULT_OUT));
        assert_eq!(listed.seed, DEFAULT_SEED);
        assert_eq!(listed.scale, Scale::Full);
        assert!(listed.upload);
        assert_eq!(listed.action, Action::List);
    }

    #[test]
    fn the_common_options_are_read_in_both_spellings() {
        let first = plan("suite --out /tmp/x --seed 7 --scale small --local-only");
        assert_eq!(first.out, PathBuf::from("/tmp/x"));
        assert_eq!(first.seed, 7);
        assert_eq!(first.scale, Scale::Small);
        assert!(!first.upload);
        let same = plan("suite --out=/tmp/x --seed=7 --scale=small --local-only=yes");
        assert_eq!(same.out, PathBuf::from("/tmp/x"));
        assert_eq!(same.seed, 7);
        assert!(!same.upload);
    }

    #[test]
    fn the_suite_and_one_dataset_resolve_to_datasets() {
        match plan("suite --scale small").action {
            Action::Write(datasets) => assert_eq!(datasets.len(), 13),
            other => panic!("{other:?}"),
        }
        match plan("dataset text-explode --scale small").action {
            Action::Write(datasets) => {
                assert_eq!(datasets.len(), 1);
                assert_eq!(datasets[0].name, "text-explode");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_ad_hoc_parquet_takes_every_knob() {
        let line = "parquet --name mine --rows 1_000 --int-cols 3 --float-cols 4 --string-cols 5 \
                    --text-cols 6 --text-mean-len 300 --text-len-stddev 40 --null-ratio 0.25 \
                    --row-group-rows 250 --compression snappy";
        match plan(line).action {
            Action::Write(datasets) => {
                assert_eq!(datasets[0].name, "mine");
                match &datasets[0].kind {
                    DatasetKind::Parquet(spec) => {
                        assert_eq!(spec.rows, 1_000);
                        assert_eq!(spec.int_cols, 3);
                        assert_eq!(spec.float_cols, 4);
                        assert_eq!(spec.short_string_cols, 5);
                        assert_eq!(spec.text_cols, 6);
                        assert_eq!(spec.text_mean_len, 300.0);
                        assert_eq!(spec.text_len_stddev, 40.0);
                        assert_eq!(spec.null_ratio, 0.25);
                        assert_eq!(spec.row_group_rows, 250);
                        assert_eq!(spec.compression, Codec::Snappy);
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_text_length_can_be_given_as_a_variance() {
        match plan("parquet --text-len-variance 100").action {
            Action::Write(datasets) => match &datasets[0].kind {
                DatasetKind::Parquet(spec) => assert_eq!(spec.text_len_stddev, 10.0),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        assert!(parse(&args("parquet --text-len-variance -4")).is_err());
        assert!(parse(&args("parquet --text-len-variance x")).is_err());
    }

    #[test]
    fn tensors_are_parsed_from_one_pair_or_from_repeated_specs() {
        match plan("mrb1 --name t --dtype i16 --shape 2,3,4").action {
            Action::Write(datasets) => match &datasets[0].kind {
                DatasetKind::Amb1(spec) => {
                    assert_eq!(spec.dtype, DType::I16);
                    assert_eq!(spec.shape, vec![2, 3, 4]);
                    assert_eq!(spec.name, "t");
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_scalar_shape_and_the_tensor_defaults() {
        match plan("mrb1 --shape=").action {
            Action::Write(datasets) => match &datasets[0].kind {
                DatasetKind::Amb1(spec) => assert!(spec.shape.is_empty()),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        match plan("safetensors").action {
            Action::Write(datasets) => {
                assert_eq!(datasets[0].name, "adhoc-tensors");
                match &datasets[0].kind {
                    DatasetKind::SafeTensors(specs) => {
                        assert_eq!(specs.len(), 1);
                        assert_eq!(specs[0].dtype, DType::F32);
                        assert_eq!(specs[0].shape, vec![128, 64]);
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
        match plan("safetensors --tensor w:f16:4,4 --tensor b:bf16:4").action {
            Action::Write(datasets) => match &datasets[0].kind {
                DatasetKind::SafeTensors(specs) => {
                    assert_eq!(specs.len(), 2);
                    assert_eq!(specs[0].name, "w");
                    assert_eq!(specs[1].dtype, DType::BF16);
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_tensor_without_a_shape_is_a_scalar() {
        match plan("safetensors --tensor s:u8").action {
            Action::Write(datasets) => match &datasets[0].kind {
                DatasetKind::SafeTensors(specs) => assert!(specs[0].shape.is_empty()),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bad_command_line_is_a_usage_error_and_never_a_panic() {
        for line in [
            "nonsense",
            "dataset",
            "dataset a b",
            "suite --nope 1",
            "suite --seed x",
            "suite --seed",
            "suite --seed 1 --seed 2",
            "suite --scale sideways",
            "dataset not-a-dataset",
            "parquet --null-ratio 4",
            "parquet --row-group-rows 0",
            "mrb1 --dtype f128",
            "mrb1 --shape 1,x",
            "mrb1 --shape 1,2,3,4,5,6,7,8,9",
            "safetensors --tensor w",
            "safetensors --tensor",
            "mrb1 --shape",
            "suite --local-only --local-only",
            "list extra",
        ] {
            let err = parse(&args(line)).err();
            assert!(err.is_some(), "{line} should not parse");
        }
    }

    #[test]
    fn a_negative_dimension_is_refused() {
        assert!(parse(&args("mrb1 --shape=-1,4")).is_err());
    }
}
