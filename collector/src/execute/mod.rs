//! Execute benchmarks.

use crate::{Compiler, Profile, Scenario};
use anyhow::{bail, Context};
use collector::benchmark::category::Category;
use collector::etw_parser;
use collector::{command_output, utils};
use database::{PatchName, QueryLabel};
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use std::cmp;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::{self, File};
use std::hash;
use std::io::Read;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::str;
use std::time::Duration;
use tempfile::TempDir;
use tokio::runtime::Runtime;

pub mod profiler;
mod rustc;

fn default_runs() -> usize {
    3
}

/// This is the internal representation of an individual benchmark's
/// perf-config.json file.
#[derive(Debug, Clone, serde::Deserialize)]
struct BenchmarkConfig {
    cargo_opts: Option<String>,
    cargo_rustc_opts: Option<String>,
    cargo_toml: Option<String>,
    #[serde(default)]
    disabled: bool,
    #[serde(default = "default_runs")]
    runs: usize,

    /// The file that should be touched to ensure cargo re-checks the leaf crate
    /// we're interested in. Likely, something similar to `src/lib.rs`. The
    /// default if this is not present is to touch all .rs files in the
    /// directory that `Cargo.toml` is in.
    #[serde(default)]
    touch_file: Option<String>,

    category: Category,
}

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Hash)]
pub struct BenchmarkName(pub String);

impl fmt::Display for BenchmarkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub struct Benchmark {
    pub name: BenchmarkName,
    pub path: PathBuf,
    pub patches: Vec<Patch>,
    config: BenchmarkConfig,
}

// Tools usable with the benchmarking subcommands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bencher {
    PerfStat,
    PerfStatSelfProfile,
    XperfStat,
    XperfStatSelfProfile,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PerfTool {
    BenchTool(Bencher),
    ProfileTool(profiler::Profiler),
}

impl PerfTool {
    fn name(&self) -> String {
        match self {
            PerfTool::BenchTool(b) => format!("{:?}", b),
            PerfTool::ProfileTool(p) => format!("{:?}", p),
        }
    }

    // What cargo subcommand do we need to run for this profiler? If not
    // `rustc`, must be a subcommand that itself invokes `rustc`.
    fn cargo_subcommand(&self, profile: Profile) -> Option<&'static str> {
        use profiler::Profiler::*;
        use Bencher::*;
        use PerfTool::*;
        match self {
            BenchTool(PerfStat)
            | BenchTool(PerfStatSelfProfile)
            | BenchTool(XperfStat)
            | BenchTool(XperfStatSelfProfile)
            | ProfileTool(SelfProfile)
            | ProfileTool(TimePasses)
            | ProfileTool(PerfRecord)
            | ProfileTool(Oprofile)
            | ProfileTool(Cachegrind)
            | ProfileTool(Callgrind)
            | ProfileTool(Dhat)
            | ProfileTool(DhatCopy)
            | ProfileTool(Massif)
            | ProfileTool(Bytehound)
            | ProfileTool(Eprintln)
            | ProfileTool(DepGraph)
            | ProfileTool(MonoItems)
            | ProfileTool(LlvmIr) => {
                if profile == Profile::Doc {
                    Some("rustdoc")
                } else {
                    Some("rustc")
                }
            }
            ProfileTool(LlvmLines) => match profile {
                Profile::Debug | Profile::Opt => Some("llvm-lines"),
                Profile::Check | Profile::Doc => None,
                Profile::All => unreachable!(),
            },
        }
    }

    fn is_scenario_allowed(&self, scenario: Scenario) -> bool {
        use profiler::Profiler::*;
        use Bencher::*;
        use PerfTool::*;
        match self {
            BenchTool(PerfStat)
            | BenchTool(PerfStatSelfProfile)
            | BenchTool(XperfStat)
            | BenchTool(XperfStatSelfProfile)
            | ProfileTool(SelfProfile)
            | ProfileTool(TimePasses)
            | ProfileTool(PerfRecord)
            | ProfileTool(Oprofile)
            | ProfileTool(Cachegrind)
            | ProfileTool(Callgrind)
            | ProfileTool(Dhat)
            | ProfileTool(DhatCopy)
            | ProfileTool(Massif)
            | ProfileTool(Bytehound)
            | ProfileTool(MonoItems)
            | ProfileTool(LlvmIr)
            | ProfileTool(Eprintln) => true,
            // only incremental
            ProfileTool(DepGraph) => scenario != Scenario::Full,
            ProfileTool(LlvmLines) => scenario == Scenario::Full,
        }
    }
}

struct CargoProcess<'a> {
    compiler: Compiler<'a>,
    cwd: &'a Path,
    profile: Profile,
    incremental: bool,
    processor_etc: Option<(&'a mut dyn Processor, Scenario, &'a str, Option<&'a Patch>)>,
    processor_name: BenchmarkName,
    manifest_path: String,
    cargo_args: Vec<String>,
    rustc_args: Vec<String>,
    touch_file: Option<String>,
    jobserver: Option<jobserver::Client>,
}

impl<'a> CargoProcess<'a> {
    fn incremental(mut self, incremental: bool) -> Self {
        self.incremental = incremental;
        self
    }

    fn processor(
        mut self,
        processor: &'a mut dyn Processor,
        scenario: Scenario,
        scenario_str: &'a str,
        patch: Option<&'a Patch>,
    ) -> Self {
        self.processor_etc = Some((processor, scenario, scenario_str, patch));
        self
    }

    fn base_command(&self, cwd: &Path, subcommand: &str) -> Command {
        let mut cmd = Command::new(Path::new(self.compiler.cargo));
        cmd
            // Not all cargo invocations (e.g. `cargo clean`) need all of these
            // env vars set, but it doesn't hurt to have them.
            .env("RUSTC", &*FAKE_RUSTC)
            .env("RUSTC_REAL", &self.compiler.rustc)
            // We separately pass -Cincremental to the leaf crate --
            // CARGO_INCREMENTAL is cached separately for both the leaf crate
            // and any in-tree dependencies, and we don't want that; it wastes
            // time.
            .env("CARGO_INCREMENTAL", "0")
            // We need to use -Z flags (for example, to force enable ICH
            // verification) so unconditionally enable unstable features, even
            // on stable compilers.
            .env("RUSTC_BOOTSTRAP", "1")
            .current_dir(cwd)
            .arg(subcommand)
            .arg("--manifest-path")
            .arg(&self.manifest_path);

        if let Some(r) = &self.compiler.rustdoc {
            cmd.env("RUSTDOC", &*FAKE_RUSTDOC).env("RUSTDOC_REAL", r);
        }
        cmd
    }

    fn get_pkgid(&self, cwd: &Path) -> anyhow::Result<String> {
        let mut pkgid_cmd = self.base_command(cwd, "pkgid");
        let out = command_output(&mut pkgid_cmd)
            .with_context(|| format!("failed to obtain pkgid in '{:?}'", cwd))?
            .stdout;
        let package_id = str::from_utf8(&out).unwrap();
        Ok(package_id.trim().to_string())
    }

    fn jobserver(mut self, server: jobserver::Client) -> Self {
        self.jobserver = Some(server);
        self
    }

    // FIXME: the needs_final and processor_etc interactions aren't ideal; we
    // would like to "auto know" when we need final but currently we don't
    // really.
    fn run_rustc(&mut self, needs_final: bool) -> anyhow::Result<()> {
        log::info!(
            "run_rustc with incremental={}, profile={:?}, scenario={:?}, patch={:?}",
            self.incremental,
            self.profile,
            self.processor_etc.as_ref().map(|v| v.1),
            self.processor_etc.as_ref().and_then(|v| v.3)
        );

        loop {
            // Get the subcommand. If it's not `rustc` it must should be a
            // subcommand that itself invokes `rustc` (so that the `FAKE_RUSTC`
            // machinery works).
            let cargo_subcommand =
                if let Some((ref mut processor, scenario, ..)) = self.processor_etc {
                    let perf_tool = processor.perf_tool();
                    if !perf_tool.is_scenario_allowed(scenario) {
                        return Err(anyhow::anyhow!(
                            "this perf tool doesn't support {:?} scenarios",
                            scenario
                        ));
                    }

                    match perf_tool.cargo_subcommand(self.profile) {
                        None => {
                            return Err(anyhow::anyhow!(
                                "this perf tool doesn't support the {:?} profile",
                                self.profile
                            ))
                        }
                        Some(sub) => sub,
                    }
                } else {
                    match self.profile {
                        Profile::Doc => "rustdoc",
                        _ => "rustc",
                    }
                };

            let mut cmd = self.base_command(self.cwd, cargo_subcommand);
            cmd.arg("-p").arg(self.get_pkgid(self.cwd)?);
            match self.profile {
                Profile::Check => {
                    cmd.arg("--profile").arg("check");
                }
                Profile::Debug => {}
                Profile::Doc => {}
                Profile::Opt => {
                    cmd.arg("--release");
                }
                Profile::All => unreachable!(),
            }
            cmd.args(&self.cargo_args);
            if env::var_os("CARGO_RECORD_TIMING").is_some() {
                cmd.arg("-Zunstable-options");
                cmd.arg("-Ztimings");
            }
            cmd.arg("--");
            // --wrap-rustc-with is not a valid rustc flag. But rustc-fake
            // recognizes it, strips it (and its argument) out, and uses it as an
            // indicator that the rustc invocation should be profiled. This works
            // out nicely because `cargo rustc` only passes arguments after '--'
            // onto rustc for the final crate, which is exactly the crate for which
            // we want to wrap rustc.
            if needs_final {
                let processor = self
                    .processor_etc
                    .as_mut()
                    .map(|v| &mut v.0)
                    .expect("needs_final needs a processor");
                let perf_tool_name = processor.perf_tool().name();
                // If we're using a processor, we expect that only the crate
                // we're interested in benchmarking will be built, not any
                // dependencies.
                cmd.env("EXPECT_ONLY_WRAPPED_RUSTC", "1");
                cmd.arg("--wrap-rustc-with");
                cmd.arg(perf_tool_name);
                cmd.args(&self.rustc_args);

                // If we're not going to be in a processor, then there's no
                // point ensuring that we recompile anything -- that just wastes
                // time.

                // Touch all the files under the Cargo.toml of the manifest we're
                // benchmarking, so as to not refresh dependencies, which may be
                // in-tree (e.g., in the case of the servo crates there are a lot of
                // other components).
                if let Some(file) = &self.touch_file {
                    utils::fs::touch(&self.cwd.join(Path::new(&file)))?;
                } else {
                    utils::fs::touch_all(
                        &self.cwd.join(
                            Path::new(&self.manifest_path)
                                .parent()
                                .expect("manifest has parent"),
                        ),
                    )?;
                }
            } else {
                // If we're not going to record the final rustc, then there's
                // absolutely no point in waiting for it to build. This will
                // have the final rustc just immediately exit(0) without
                // actually running it.
                cmd.arg("--skip-this-rustc");
            }

            if self.incremental {
                cmd.arg("-C");
                let mut incr_arg = std::ffi::OsString::from("incremental=");
                incr_arg.push(self.cwd.join("incremental-state"));
                cmd.arg(incr_arg);
            }

            if let Some(client) = &self.jobserver {
                client.configure(&mut cmd);
            }

            log::debug!("{:?}", cmd);

            let output = command_output(&mut cmd)?;
            if let Some((ref mut processor, scenario, scenario_str, patch)) = self.processor_etc {
                let data = ProcessOutputData {
                    name: self.processor_name.clone(),
                    cwd: self.cwd,
                    profile: self.profile,
                    scenario,
                    scenario_str,
                    patch,
                };
                match processor.process_output(&data, output) {
                    Ok(Retry::No) => return Ok(()),
                    Ok(Retry::Yes) => {}
                    Err(e) => return Err(e),
                }
            } else {
                return Ok(());
            }
        }
    }
}

lazy_static::lazy_static! {
    static ref FAKE_RUSTC: PathBuf = {
        let mut fake_rustc = env::current_exe().unwrap();
        fake_rustc.pop();
        fake_rustc.push("rustc-fake");
        fake_rustc
    };
    static ref FAKE_RUSTDOC: PathBuf = {
        let mut fake_rustdoc = env::current_exe().unwrap();
        fake_rustdoc.pop();
        fake_rustdoc.push("rustdoc-fake");
        // link from rustc-fake to rustdoc-fake
        if !fake_rustdoc.exists() {
            #[cfg(unix)]
            use std::os::unix::fs::symlink;
            #[cfg(windows)]
            use std::os::windows::fs::symlink_file as symlink;

            symlink(&*FAKE_RUSTC, &fake_rustdoc).expect("failed to make symbolic link");
        }
        fake_rustdoc
    };
}

/// Used to indicate if we need to retry a run.
pub enum Retry {
    No,
    Yes,
}

pub struct ProcessOutputData<'a> {
    name: BenchmarkName,
    cwd: &'a Path,
    profile: Profile,
    scenario: Scenario,
    scenario_str: &'a str,
    patch: Option<&'a Patch>,
}

/// Trait used by `Benchmark::measure()` to provide different kinds of
/// processing.
pub trait Processor {
    /// The `PerfTool` being used.
    fn perf_tool(&self) -> PerfTool;

    /// Process the output produced by the particular `Profiler` being used.
    fn process_output(
        &mut self,
        data: &ProcessOutputData<'_>,
        output: process::Output,
    ) -> anyhow::Result<Retry>;

    /// Provided to permit switching on more expensive profiling if it's needed
    /// for the "first" run for any given benchmark (we reuse the processor),
    /// e.g. disabling -Zself-profile.
    fn start_first_collection(&mut self) {}

    /// Provided to permit switching off more expensive profiling if it's needed
    /// for the "first" run, e.g. disabling -Zself-profile.
    ///
    /// Return "true" if planning on doing something different for second
    /// iteration.
    fn finished_first_collection(&mut self) -> bool {
        false
    }
}

pub struct BenchProcessor<'a> {
    rt: &'a mut Runtime,
    benchmark: &'a BenchmarkName,
    conn: &'a mut dyn database::Connection,
    artifact: &'a database::ArtifactId,
    artifact_row_id: database::ArtifactIdNumber,
    upload: Option<Upload>,
    is_first_collection: bool,
    is_self_profile: bool,
    tries: u8,
}

impl<'a> BenchProcessor<'a> {
    pub fn new(
        rt: &'a mut Runtime,
        conn: &'a mut dyn database::Connection,
        benchmark: &'a BenchmarkName,
        artifact: &'a database::ArtifactId,
        artifact_row_id: database::ArtifactIdNumber,
        is_self_profile: bool,
    ) -> Self {
        // Check we have `perf` or (`xperf.exe` and `tracelog.exe`)  available.
        if cfg!(unix) {
            let has_perf = Command::new("perf").output().is_ok();
            assert!(has_perf);
        } else {
            let has_xperf = Command::new(env::var("XPERF").unwrap_or("xperf.exe".to_string()))
                .output()
                .is_ok();
            assert!(has_xperf);

            let has_tracelog =
                Command::new(env::var("TRACELOG").unwrap_or("tracelog.exe".to_string()))
                    .output()
                    .is_ok();
            assert!(has_tracelog);
        }

        BenchProcessor {
            rt,
            upload: None,
            conn,
            benchmark,
            artifact,
            artifact_row_id,
            is_first_collection: true,
            is_self_profile,
            tries: 0,
        }
    }

    fn insert_stats(
        &mut self,
        scenario: database::Scenario,
        profile: Profile,
        stats: (Stats, Option<SelfProfile>, Option<SelfProfileFiles>),
    ) {
        let version = String::from_utf8(
            Command::new("git")
                .arg("rev-parse")
                .arg("HEAD")
                .output()
                .context("git rev-parse HEAD")
                .unwrap()
                .stdout,
        )
        .context("utf8")
        .unwrap();

        let collection = self.rt.block_on(self.conn.collection_id(&version));
        let profile = match profile {
            Profile::Check => database::Profile::Check,
            Profile::Debug => database::Profile::Debug,
            Profile::Doc => database::Profile::Doc,
            Profile::Opt => database::Profile::Opt,
            Profile::All => unreachable!(),
        };

        if let Some(files) = stats.2 {
            if env::var_os("RUSTC_PERF_UPLOAD_TO_S3").is_some() {
                // We can afford to have the uploads run concurrently with
                // rustc. Generally speaking, they take up almost no CPU time
                // (just copying data into the network). Plus, during
                // self-profile data timing noise doesn't matter as much. (We'll
                // be migrating to instructions soon, hopefully, where the
                // upload will cause even less noise). We may also opt at some
                // point to defer these uploads entirely to the *end* or
                // something like that. For now though this works quite well.
                if let Some(u) = self.upload.take() {
                    u.wait();
                }
                let prefix = PathBuf::from("self-profile")
                    .join(self.artifact_row_id.0.to_string())
                    .join(self.benchmark.0.as_str())
                    .join(profile.to_string())
                    .join(scenario.to_id());
                self.upload = Some(Upload::new(prefix, collection, files));
                self.rt.block_on(self.conn.record_raw_self_profile(
                    collection,
                    self.artifact_row_id,
                    self.benchmark.0.as_str(),
                    profile,
                    scenario,
                ));
            }
        }

        let mut buf = FuturesUnordered::new();
        for (stat, value) in stats.0.iter() {
            buf.push(self.conn.record_statistic(
                collection,
                self.artifact_row_id,
                self.benchmark.0.as_str(),
                profile,
                scenario,
                stat,
                value,
            ));
        }

        if let Some(sp) = &stats.1 {
            let conn = &*self.conn;
            let artifact_row_id = self.artifact_row_id;
            let benchmark = self.benchmark.0.as_str();
            for qd in &sp.query_data {
                buf.push(conn.record_self_profile_query(
                    collection,
                    artifact_row_id,
                    benchmark,
                    profile,
                    scenario,
                    qd.label.as_str(),
                    database::QueryDatum {
                        self_time: qd.self_time,
                        blocked_time: qd.blocked_time,
                        incremental_load_time: qd.incremental_load_time,
                        number_of_cache_hits: qd.number_of_cache_hits,
                        invocation_count: qd.invocation_count,
                    },
                ));
            }
        }

        self.rt
            .block_on(async move { while let Some(()) = buf.next().await {} });
    }

    pub fn measure_rustc(&mut self, compiler: Compiler<'_>) -> anyhow::Result<()> {
        rustc::measure(
            self.rt,
            self.conn,
            compiler,
            self.artifact,
            self.artifact_row_id,
        )
    }
}

struct Upload(std::process::Child, tempfile::NamedTempFile);

impl Upload {
    fn new(prefix: PathBuf, collection: database::CollectionId, files: SelfProfileFiles) -> Upload {
        // Files are placed at
        //  * self-profile/<artifact id>/<benchmark>/<profile>/<scenario>
        //    /self-profile-<collection-id>.{extension}
        let upload = tempfile::NamedTempFile::new()
            .context("create temporary file")
            .unwrap();
        let filename = match files {
            SelfProfileFiles::Seven {
                string_index,
                string_data,
                events,
            } => {
                let tarball = snap::write::FrameEncoder::new(Vec::new());
                let mut builder = tar::Builder::new(tarball);
                builder.mode(tar::HeaderMode::Deterministic);

                let append_file = |builder: &mut tar::Builder<_>,
                                   file: &Path,
                                   name: &str|
                 -> anyhow::Result<()> {
                    if file.exists() {
                        // Silently ignore missing files, the new self-profile
                        // experiment with one file has a different structure.
                        builder.append_path_with_name(file, name)?;
                    }
                    Ok(())
                };

                append_file(&mut builder, &string_index, "self-profile.string_index")
                    .expect("append string index");
                append_file(&mut builder, &string_data, "self-profile.string_data")
                    .expect("append string data");
                append_file(&mut builder, &events, "self-profile.events").expect("append events");
                builder.finish().expect("complete tarball");
                std::fs::write(
                    upload.path(),
                    builder
                        .into_inner()
                        .expect("get")
                        .into_inner()
                        .expect("snap success"),
                )
                .expect("wrote tarball");
                format!("self-profile-{}.tar.sz", collection)
            }
            SelfProfileFiles::Eight { file } => {
                let data = std::fs::read(&file).expect("read profile data");
                let mut data = snap::read::FrameEncoder::new(&data[..]);
                let mut compressed = Vec::new();
                data.read_to_end(&mut compressed).expect("compressed");
                std::fs::write(upload.path(), &compressed).expect("write compressed profile data");

                format!("self-profile-{}.mm_profdata.sz", collection)
            }
        };

        let child = Command::new("aws")
            .arg("s3")
            .arg("cp")
            .arg("--storage-class")
            .arg("INTELLIGENT_TIERING")
            .arg("--only-show-errors")
            .arg(upload.path())
            .arg(&format!(
                "s3://rustc-perf/{}",
                &prefix.join(&filename).to_str().unwrap()
            ))
            .spawn()
            .expect("spawn aws");

        Upload(child, upload)
    }

    fn wait(mut self) {
        let start = std::time::Instant::now();
        let status = self.0.wait().expect("waiting for child");
        if !status.success() {
            panic!("S3 upload failed: {:?}", status);
        }

        log::trace!("uploaded to S3, additional wait: {:?}", start.elapsed());
    }
}

impl<'a> Processor for BenchProcessor<'a> {
    fn perf_tool(&self) -> PerfTool {
        if self.is_first_collection && self.is_self_profile {
            if cfg!(unix) {
                PerfTool::BenchTool(Bencher::PerfStatSelfProfile)
            } else {
                PerfTool::BenchTool(Bencher::XperfStatSelfProfile)
            }
        } else {
            if cfg!(unix) {
                PerfTool::BenchTool(Bencher::PerfStat)
            } else {
                PerfTool::BenchTool(Bencher::XperfStat)
            }
        }
    }

    fn start_first_collection(&mut self) {
        self.is_first_collection = true;
    }

    fn finished_first_collection(&mut self) -> bool {
        let original = self.perf_tool();
        self.is_first_collection = false;
        // We need to run again if we're going to use a different perf tool
        self.perf_tool() != original
    }

    fn process_output(
        &mut self,
        data: &ProcessOutputData<'_>,
        output: process::Output,
    ) -> anyhow::Result<Retry> {
        match process_stat_output(output) {
            Ok(mut res) => {
                if let Some(ref profile) = res.1 {
                    store_artifact_sizes_into_stats(&mut res.0, profile);
                }
                if let Profile::Doc = data.profile {
                    let doc_dir = data.cwd.join("target/doc");
                    if doc_dir.is_dir() {
                        store_documentation_size_into_stats(&mut res.0, &doc_dir);
                    }
                }

                match data.scenario {
                    Scenario::Full => {
                        self.insert_stats(database::Scenario::Empty, data.profile, res);
                    }
                    Scenario::IncrFull => {
                        self.insert_stats(database::Scenario::IncrementalEmpty, data.profile, res);
                    }
                    Scenario::IncrUnchanged => {
                        self.insert_stats(database::Scenario::IncrementalFresh, data.profile, res);
                    }
                    Scenario::IncrPatched => {
                        let patch = data.patch.unwrap();
                        self.insert_stats(
                            database::Scenario::IncrementalPatch(patch.name),
                            data.profile,
                            res,
                        );
                    }
                    Scenario::All => unreachable!(),
                }
                Ok(Retry::No)
            }
            Err(DeserializeStatError::NoOutput(output)) => {
                if self.tries < 5 {
                    log::warn!(
                        "failed to deserialize stats, retrying (try {}); output: {:?}",
                        self.tries,
                        output
                    );
                    self.tries += 1;
                    Ok(Retry::Yes)
                } else {
                    panic!("failed to collect statistics after 5 tries");
                }
            }
            Err(
                e
                @ (DeserializeStatError::ParseError { .. } | DeserializeStatError::XperfError(..)),
            ) => {
                panic!("process_perf_stat_output failed: {:?}", e);
            }
        }
    }
}

fn store_documentation_size_into_stats(stats: &mut Stats, doc_dir: &Path) {
    match utils::fs::get_file_count_and_size(doc_dir) {
        Ok((count, size)) => {
            stats.insert("size:doc_files_count".to_string(), count as f64);
            stats.insert("size:doc_bytes".to_string(), size as f64);
        }
        Err(error) => log::error!(
            "Cannot get size of documentation directory {}: {:?}",
            doc_dir.display(),
            error
        ),
    }
}

fn store_artifact_sizes_into_stats(stats: &mut Stats, profile: &SelfProfile) {
    for artifact in profile.artifact_sizes.iter() {
        stats
            .stats
            .insert(format!("size:{}", artifact.label), artifact.size as f64);
    }
}

impl Benchmark {
    pub fn new(name: String, path: PathBuf) -> anyhow::Result<Self> {
        let mut patches = vec![];
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "patch" {
                    patches.push(path.clone());
                }
            }
        }

        let mut patches: Vec<_> = patches.into_iter().map(|p| Patch::new(p)).collect();
        patches.sort_by_key(|p| p.index);

        let config_path = path.join("perf-config.json");
        let config: BenchmarkConfig = if config_path.exists() {
            serde_json::from_reader(
                File::open(&config_path)
                    .with_context(|| format!("failed to open {:?}", config_path))?,
            )
            .with_context(|| format!("failed to parse {:?}", config_path))?
        } else {
            bail!("missing a perf-config.json file for `{}`", name);
        };

        Ok(Benchmark {
            name: BenchmarkName(name),
            path,
            patches,
            config,
        })
    }

    pub fn category(&self) -> Category {
        self.config.category
    }

    #[cfg(windows)]
    fn copy(from: &Path, to: &Path) -> anyhow::Result<()> {
        crate::utils::fs::robocopy(from, to, &[])
    }

    #[cfg(unix)]
    fn copy(from: &Path, to: &Path) -> anyhow::Result<()> {
        let mut cmd = Command::new("cp");
        cmd.arg("-pLR").arg(from).arg(to);
        command_output(&mut cmd)?;
        Ok(())
    }

    fn make_temp_dir(&self, base: &Path) -> anyhow::Result<TempDir> {
        // Appending `.` means we copy just the contents of `base` into
        // `tmp_dir`, rather than `base` itself.
        let mut base_dot = base.to_path_buf();
        base_dot.push(".");
        let tmp_dir = TempDir::new()?;
        Self::copy(&base_dot, tmp_dir.path())
            .with_context(|| format!("copying {} to tmp dir", self.name))?;
        Ok(tmp_dir)
    }

    fn mk_cargo_process<'a>(
        &'a self,
        compiler: Compiler<'a>,
        cwd: &'a Path,
        profile: Profile,
    ) -> CargoProcess<'a> {
        let mut cargo_args = self
            .config
            .cargo_opts
            .clone()
            .unwrap_or_default()
            .split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>();
        if let Some(count) = env::var("CARGO_THREAD_COUNT")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
        {
            cargo_args.push(format!("-j{}", count));
        }

        CargoProcess {
            compiler,
            processor_name: self.name.clone(),
            cwd,
            profile,
            incremental: false,
            processor_etc: None,
            manifest_path: self
                .config
                .cargo_toml
                .clone()
                .unwrap_or_else(|| String::from("Cargo.toml")),
            cargo_args,
            rustc_args: self
                .config
                .cargo_rustc_opts
                .clone()
                .unwrap_or_default()
                .split_whitespace()
                .map(String::from)
                .collect(),
            touch_file: self.config.touch_file.clone(),
            jobserver: None,
        }
    }

    /// Run a specific benchmark under a processor + profiler combination.
    pub fn measure(
        &self,
        processor: &mut dyn Processor,
        profiles: &[Profile],
        scenarios: &[Scenario],
        compiler: Compiler<'_>,
        iterations: Option<usize>,
    ) -> anyhow::Result<()> {
        let iterations = iterations.unwrap_or(self.config.runs);

        if self.config.disabled || profiles.is_empty() {
            eprintln!("Skipping {}: disabled", self.name);
            bail!("disabled benchmark");
        }

        eprintln!("Preparing {}", self.name);
        let profile_dirs = profiles
            .iter()
            .map(|profile| Ok((*profile, self.make_temp_dir(&self.path)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;

        // In parallel (but with a limit to the number of CPUs), prepare all
        // profiles. This is done in parallel vs. sequentially because:
        //  * We don't record any measurements during this phase, so the
        //    performance need not be consistent.
        //  * We want to make use of the reality that rustc is single-threaded
        //    during a good portion of compilation; that means that it is faster
        //    to run this preparation when we can interleave rustc's as needed
        //    rather than fully sequentially, where we have long periods of a
        //    single CPU core being used.
        //
        // As one example, with a full (All profiles x All scenarios)
        // configuration, script-servo-2 took 2995s without this parallelization
        // and 2915s with. This is a small win, admittedly, but even a few
        // minutes shaved off is important -- and there's not too much mangling
        // of our code needed to get this to work. This benchmark has since been
        // deleted, but the optimization holds for other crates as well.
        //
        // Ideally we would not separately build build-script's (which are
        // otherwise shared between the configurations), but there's no good way
        // to do this in Cargo today. We would also ideally build in the same
        // target directory, but that's also not possible, as Cargo takes a
        // target-directory global lock during compilation.
        crossbeam_utils::thread::scope::<_, anyhow::Result<()>>(|s| {
            let server = jobserver::Client::new(num_cpus::get()).context("jobserver::new")?;
            for (profile, prep_dir) in &profile_dirs {
                let server = server.clone();
                s.spawn::<_, anyhow::Result<()>>(move |_| {
                    self.mk_cargo_process(compiler, prep_dir.path(), *profile)
                        .jobserver(server)
                        .run_rustc(false)?;
                    Ok(())
                });
            }
            Ok(())
        })
        .unwrap()?;

        for (profile, prep_dir) in profile_dirs {
            eprintln!("Running {}: {:?} + {:?}", self.name, profile, scenarios);

            // We want at least two runs for all benchmarks (since we run
            // self-profile separately).
            processor.start_first_collection();
            for i in 0..cmp::max(iterations, 2) {
                if i == 1 {
                    let different = processor.finished_first_collection();
                    if iterations == 1 && !different {
                        // Don't run twice if this processor doesn't need it and
                        // we've only been asked to run once.
                        break;
                    }
                }
                log::debug!("Benchmark iteration {}/{}", i + 1, iterations);
                // Don't delete the directory on error.
                let timing_dir = ManuallyDrop::new(self.make_temp_dir(prep_dir.path())?);
                let cwd = timing_dir.path();

                // A full non-incremental build.
                if scenarios.contains(&Scenario::Full) {
                    self.mk_cargo_process(compiler, cwd, profile)
                        .processor(processor, Scenario::Full, "Full", None)
                        .run_rustc(true)?;
                }

                // Rustdoc does not support incremental compilation
                if profile != Profile::Doc {
                    // An incremental  from scratch (slowest incremental case).
                    // This is required for any subsequent incremental builds.
                    if scenarios.iter().any(|s| s.is_incr()) {
                        self.mk_cargo_process(compiler, cwd, profile)
                            .incremental(true)
                            .processor(processor, Scenario::IncrFull, "IncrFull", None)
                            .run_rustc(true)?;
                    }

                    // An incremental build with no changes (fastest incremental case).
                    if scenarios.contains(&Scenario::IncrUnchanged) {
                        self.mk_cargo_process(compiler, cwd, profile)
                            .incremental(true)
                            .processor(processor, Scenario::IncrUnchanged, "IncrUnchanged", None)
                            .run_rustc(true)?;
                    }

                    if scenarios.contains(&Scenario::IncrPatched) {
                        for (i, patch) in self.patches.iter().enumerate() {
                            log::debug!("applying patch {}", patch.name);
                            patch.apply(cwd).map_err(|s| anyhow::anyhow!("{}", s))?;

                            // An incremental build with some changes (realistic
                            // incremental case).
                            let scenario_str = format!("IncrPatched{}", i);
                            self.mk_cargo_process(compiler, cwd, profile)
                                .incremental(true)
                                .processor(
                                    processor,
                                    Scenario::IncrPatched,
                                    &scenario_str,
                                    Some(&patch),
                                )
                                .run_rustc(true)?;
                        }
                    }
                }
                drop(ManuallyDrop::into_inner(timing_dir));
            }
        }

        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
enum DeserializeStatError {
    #[error("could not deserialize empty output to stats, output: {:?}", .0)]
    NoOutput(process::Output),
    #[error("could not parse `{}` as a float", .0)]
    ParseError(String, #[source] ::std::num::ParseFloatError),
    #[error("could not process xperf data")]
    XperfError(#[from] anyhow::Error),
}

enum SelfProfileFiles {
    Seven {
        string_data: PathBuf,
        string_index: PathBuf,
        events: PathBuf,
    },
    Eight {
        file: PathBuf,
    },
}

fn process_stat_output(
    output: process::Output,
) -> Result<(Stats, Option<SelfProfile>, Option<SelfProfileFiles>), DeserializeStatError> {
    let stdout = String::from_utf8(output.stdout.clone()).expect("utf8 output");
    let mut stats = Stats::new();

    let mut profile: Option<SelfProfile> = None;
    let mut dir: Option<PathBuf> = None;
    let mut prefix: Option<String> = None;
    let mut file: Option<PathBuf> = None;
    for line in stdout.lines() {
        if line.starts_with("!self-profile-output:") {
            profile = Some(serde_json::from_str(&line["!self-profile-output:".len()..]).unwrap());
            continue;
        }
        if line.starts_with("!self-profile-dir:") {
            dir = Some(PathBuf::from(&line["!self-profile-dir:".len()..]));
            continue;
        }
        if line.starts_with("!self-profile-prefix:") {
            prefix = Some(String::from(&line["!self-profile-prefix:".len()..]));
            continue;
        }
        if line.starts_with("!self-profile-file:") {
            file = Some(PathBuf::from(&line["!self-profile-file:".len()..]));
            continue;
        }
        if line.starts_with("!counters-file:") {
            let counter_file = &line["!counters-file:".len()..];
            let counters = etw_parser::parse_etw_file(counter_file).unwrap();

            stats.insert("cycles".into(), counters.total_cycles as f64);
            stats.insert(
                "instructions:u".into(),
                counters.instructions_retired as f64,
            );
            stats.insert("cpu-clock".into(), counters.cpu_clock);
            continue;
        }
        if line.starts_with("!wall-time:") {
            let d = &line["!wall-time:".len()..];
            stats.insert(
                "wall-time".into(),
                d.parse()
                    .map_err(|e| DeserializeStatError::ParseError(d.to_string(), e))?,
            );
            continue;
        }

        // The rest of the loop body handles processing output from the Linux `perf` tool
        // so on Windows, we just skip it and go to the next line.
        if cfg!(windows) {
            continue;
        }

        // github.com/torvalds/linux/blob/bc78d646e708/tools/perf/Documentation/perf-stat.txt#L281
        macro_rules! get {
            ($e: expr) => {
                match $e {
                    Some(s) => s,
                    None => {
                        log::warn!("unhandled line: {}", line);
                        continue;
                    }
                }
            };
        }
        let mut parts = line.split(';').map(|s| s.trim());
        let cnt = get!(parts.next());
        let _unit = get!(parts.next());
        let name = get!(parts.next());
        let _time = get!(parts.next());
        let pct = get!(parts.next());
        if cnt == "<not supported>" || cnt.len() == 0 {
            continue;
        }
        if !pct.starts_with("100.") {
            panic!(
                "measurement of `{}` only active for {}% of the time",
                name, pct
            );
        }
        stats.insert(
            name.to_owned(),
            cnt.parse()
                .map_err(|e| DeserializeStatError::ParseError(cnt.to_string(), e))?,
        );
    }

    let files = if let (Some(prefix), Some(dir)) = (prefix, dir) {
        let mut string_index = PathBuf::new();
        let mut string_data = PathBuf::new();
        let mut events = PathBuf::new();
        for entry in fs::read_dir(&dir).unwrap() {
            let filename = entry.unwrap().file_name();
            let filename_str = filename.to_str().unwrap();
            let path = dir.join(filename_str);
            if filename_str.ends_with(".events") {
                assert!(filename_str.contains(&prefix), "{:?}", path);
                events = path;
            } else if filename_str.ends_with(".string_data") {
                assert!(filename_str.contains(&prefix), "{:?}", path);
                string_data = path;
            } else if filename_str.ends_with(".string_index") {
                assert!(filename_str.contains(&prefix), "{:?}", path);
                string_index = path;
            }
        }

        Some(SelfProfileFiles::Seven {
            string_index,
            string_data,
            events,
        })
    } else if let Some(file) = file {
        Some(SelfProfileFiles::Eight { file })
    } else {
        None
    };

    if stats.is_empty() {
        return Err(DeserializeStatError::NoOutput(output));
    }

    Ok((stats, profile, files))
}

#[derive(Clone)]
pub struct Stats {
    stats: HashMap<String, f64>,
}

impl Default for Stats {
    fn default() -> Self {
        Stats::new()
    }
}

impl Stats {
    pub fn new() -> Stats {
        Stats {
            stats: HashMap::new(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, f64)> + '_ {
        self.stats.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }

    pub fn insert(&mut self, stat: String, value: f64) {
        self.stats.insert(stat, value);
    }
}

#[derive(Debug, Clone)]
pub struct Patch {
    index: usize,
    pub name: PatchName,
    path: PathBuf,
}

impl PartialEq for Patch {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Patch {}

impl hash::Hash for Patch {
    fn hash<H: hash::Hasher>(&self, h: &mut H) {
        self.name.hash(h);
    }
}

impl Patch {
    pub fn new(path: PathBuf) -> Self {
        assert!(path.is_file());
        let (index, name) = {
            let file_name = path.file_name().unwrap().to_string_lossy();
            let mut parts = file_name.split("-");
            let index = parts.next().unwrap().parse().unwrap_or_else(|e| {
                panic!(
                    "{:?} should be in the format 000-name.patch, \
                     but did not start with a number: {:?}",
                    &path, e
                );
            });
            let mut name = parts.fold(String::new(), |mut acc, part| {
                acc.push_str(part);
                acc.push(' ');
                acc
            });
            let len = name.len();
            // take final space off
            name.truncate(len - 1);
            let name = name.replace(".patch", "");
            (index, name)
        };

        Patch {
            path: PathBuf::from(path.file_name().unwrap().to_str().unwrap()),
            index,
            name: name.as_str().into(),
        }
    }

    pub fn apply(&self, dir: &Path) -> anyhow::Result<()> {
        log::debug!("applying {} to {:?}", self.name, dir);

        let mut cmd = Command::new("git");
        cmd.current_dir(dir).args(&["apply"]).arg(&*self.path);

        command_output(&mut cmd)?;

        Ok(())
    }
}

#[derive(serde::Deserialize, Clone)]
pub struct SelfProfile {
    pub query_data: Vec<QueryData>,
    pub artifact_sizes: Vec<ArtifactSize>,
}

#[derive(serde::Deserialize, Clone)]
pub struct ArtifactSize {
    pub label: QueryLabel,
    #[serde(rename = "value")]
    pub size: u64,
}

#[derive(serde::Deserialize, Clone)]
pub struct QueryData {
    pub label: QueryLabel,
    pub self_time: Duration,
    pub number_of_cache_hits: u32,
    pub invocation_count: u32,
    pub blocked_time: Duration,
    pub incremental_load_time: Duration,
}