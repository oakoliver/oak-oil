//! One compile unit: its recorded rustc command, its content key, and running
//! it against the store. Shared by the wrapper (Cargo schedules) and the
//! executor (Oak Oil schedules).

use crate::depinfo;
use crate::hashing::{HashCache, hash_bytes};
use crate::rustc_args::Invocation;
use crate::store::{Entry, Store, denorm, norm};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::SystemTime;

/// What Oak Oil keeps about a unit Cargo asked rustc to build.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Record {
    pub rustc: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// The env Cargo gave rustc (minus volatile process variables): build
    /// scripts' `rustc-env` and `[env]` config can add any name.
    pub env: Vec<(String, String)>,
    pub out_dir: PathBuf,
    /// `<crate_name><extra-filename>`, unique per unit.
    pub key: String,
    /// Output file names in `out_dir`.
    pub outputs: Vec<String>,
}

impl Record {
    pub fn invocation(&self) -> Option<Invocation> {
        Invocation::parse(&self.args)
    }

    /// The value rustc sees for `k`: the recorded env, else the process's.
    pub fn env_value(&self, k: &str) -> Option<String> {
        self.env_get(k)
    }

    fn env_get(&self, k: &str) -> Option<String> {
        self.env
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(k).ok())
    }
}

/// rustc's environment as recorded: everything except variables that only
/// describe the calling process (jobserver, logging, terminal, Oak Oil's own).
pub fn select_env(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    const VOLATILE: &[&str] = &[
        "CARGO_MAKEFLAGS",
        "CARGO_LOG",
        "OLDPWD",
        "SHLVL",
        "_",
        "PWD",
        "MAKEFLAGS",
        "MFLAGS",
    ];
    let mut v: Vec<(String, String)> = vars
        .filter(|(k, _)| {
            !VOLATILE.contains(&k.as_str())
                && !k.starts_with("OAKOIL")
                && !k.starts_with("TERM")
                && !k.starts_with("ITERM")
                && !k.starts_with("__CF")
                && !k.starts_with("SSH_")
        })
        .collect();
    v.sort();
    v
}

/// The part of the env that goes into the store key. Anything a crate reads
/// with `env!`/`option_env!` is checked separately through rustc's dep-info.
fn key_env(env: &[(String, String)]) -> impl Iterator<Item = &(String, String)> {
    env.iter().filter(|(k, _)| {
        k.starts_with("CARGO")
            || k == "OUT_DIR"
            || (k.starts_with("RUSTC_") && !k.ends_with("WRAPPER"))
            || k == "MACOSX_DEPLOYMENT_TARGET"
            || k == "SDKROOT"
            || k.ends_with("_DEPLOYMENT_TARGET")
    })
}

pub fn records_dir(target: &Path) -> PathBuf {
    target.join("oakoil").join("units")
}

pub fn save_record(target: &Path, rec: &Record) -> io::Result<()> {
    let dir = records_dir(target);
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.{}.tmp", rec.key, std::process::id()));
    fs::write(&tmp, serde_json::to_vec(rec)?)?;
    fs::rename(tmp, dir.join(format!("{}.json", rec.key)))
}

pub fn load_record(target: &Path, key: &str) -> Option<Record> {
    serde_json::from_slice(&fs::read(records_dir(target).join(format!("{key}.json"))).ok()?).ok()
}

/// Unit key: `<crate><extra-filename>`. Units Cargo gives no extra-filename
/// (crates with a `cdylib`/`dylib` type need stable file names) get a short
/// hash of their out dir instead, so debug and release do not collide.
pub fn unit_key(crate_name: &str, extra: &str, out_dir: &Path) -> String {
    if extra.is_empty() {
        format!(
            "{crate_name}@{}",
            &hash_bytes(out_dir.to_string_lossy().as_bytes())[..12]
        )
    } else {
        format!("{crate_name}{extra}")
    }
}

/// The unit that produced an output file: `…/deps/libserde-b611c61aabb71f5f.rmeta`
/// → `serde-b611c61aabb71f5f`; `…/deps/libffi_shim.rlib` → `ffi_shim@<dir hash>`.
pub fn record_key_of(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = name.split('.').next().unwrap_or(&name);
    let is_lib = [".rlib", ".rmeta", ".dylib", ".so", ".a"]
        .iter()
        .any(|e| name.ends_with(e));
    let stem = match stem.strip_prefix("lib") {
        Some(s) if is_lib => s,
        _ => stem,
    };
    let has_hash = stem
        .rsplit_once('-')
        .is_some_and(|(_, h)| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    if has_hash {
        stem.to_string()
    } else {
        unit_key(stem, "", path.parent().unwrap_or(Path::new("")))
    }
}

pub struct Ctx {
    pub store: Store,
    pub hc: HashCache,
    pub target: PathBuf,
    pub target_s: String,
    pub cargo_home: PathBuf,
    rustc_info: std::sync::Mutex<std::collections::HashMap<PathBuf, String>>,
    records: std::sync::Mutex<std::collections::HashMap<String, Option<Record>>>,
    deferred: Mutex<Option<Vec<PutJob>>>,
}

impl Ctx {
    pub fn new(target: PathBuf, hc: HashCache) -> io::Result<Ctx> {
        hc.allow_xattr(&target);
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cargo")
            });
        Ok(Ctx {
            store: Store::open()?,
            hc,
            target_s: target.to_string_lossy().to_string(),
            target,
            cargo_home,
            rustc_info: Default::default(),
            records: Default::default(),
            deferred: Mutex::new(None),
        })
    }

    /// `rustc -vV`, cached in the store by the binary's path, size and mtime.
    pub fn rustc_info(&self, rustc: &Path) -> io::Result<String> {
        if let Some(s) = self.rustc_info.lock().unwrap().get(rustc) {
            return Ok(s.clone());
        }
        let md = fs::metadata(rustc).or_else(|_| fs::metadata(which(rustc)))?;
        let id = hash_bytes(
            format!("{}:{}:{:?}", rustc.display(), md.len(), md.modified().ok()).as_bytes(),
        );
        let cache = self.store.root().join("rustc-info").join(&id);
        let info = match fs::read_to_string(&cache) {
            Ok(s) => s,
            Err(_) => {
                let out = Command::new(rustc).arg("-vV").output()?;
                let s = String::from_utf8_lossy(&out.stdout).to_string();
                fs::create_dir_all(cache.parent().unwrap())?;
                fs::write(&cache, &s)?;
                s
            }
        };
        self.rustc_info
            .lock()
            .unwrap()
            .insert(rustc.to_path_buf(), info.clone());
        Ok(info)
    }

    /// Libraries built from registry or git sources without a build script's
    /// OUT_DIR hold no path of this target dir, so their key leaves the
    /// target out and other projects can reuse them. Linked outputs (bins,
    /// proc-macro dylibs, build scripts) record the paths of their object
    /// files in this target dir (macOS debug map), so they stay per project.
    fn shareable(&self, rec: &Record, inv: &Invocation) -> bool {
        rec.cwd.starts_with(&self.cargo_home)
            && !rec.env.iter().any(|(k, _)| k == "OUT_DIR")
            && !inv.links()
    }

    /// The manifest key: everything about a unit known before it runs.
    /// `None` = not cacheable (a dependency we cannot account for).
    pub fn manifest_key(&self, rec: &Record, inv: &Invocation) -> io::Result<Option<String>> {
        let t = self.target_s.as_str();
        let mut h = blake3::Hasher::new();
        let mut put = |s: &str| {
            h.update(s.as_bytes());
            h.update(b"\0");
        };
        put("oil-key-v1");
        put(&self.rustc_info(&rec.rustc)?);
        if !self.shareable(rec, inv) {
            put(t);
        }
        put(&norm(&rec.cwd.to_string_lossy(), t));
        for a in &rec.args {
            put(&norm(a, t));
        }
        for (k, v) in key_env(&rec.env) {
            put(k);
            put(&norm(v, t));
        }
        for e in &inv.externs {
            let Ok(hash) = self.hc.get(e) else {
                return Ok(None);
            };
            put(&norm(&e.to_string_lossy(), t));
            put(&hash);
        }
        for d in inv
            .native_dirs
            .iter()
            .filter(|d| d.starts_with(&self.target))
        {
            let mut files: Vec<PathBuf> = fs::read_dir(d)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
                .unwrap_or_default();
            files.sort();
            for f in files.iter().filter(|f| f.is_file()) {
                put(&norm(&f.to_string_lossy(), t));
                put(&self.hc.get(f)?);
            }
        }
        if inv.links() {
            // rustc finds transitive rlibs through `-L dependency=`; key on them all.
            let Some(libs) = self.transitive_libs(inv) else {
                return Ok(None);
            };
            for f in libs {
                put(&norm(&f.to_string_lossy(), t));
                put(&self.hc.get(&f)?);
            }
        }
        Ok(Some(h.finalize().to_hex().to_string()))
    }

    /// Records are read once per process.
    fn record(&self, key: &str) -> Option<Record> {
        let mut m = self.records.lock().unwrap();
        m.entry(key.to_string())
            .or_insert_with(|| load_record(&self.target, key))
            .clone()
    }

    fn transitive_libs(&self, inv: &Invocation) -> Option<BTreeSet<PathBuf>> {
        let mut seen = HashSet::new();
        let mut libs = BTreeSet::new();
        let mut queue: VecDeque<PathBuf> = inv.externs.iter().cloned().collect();
        while let Some(p) = queue.pop_front() {
            let key = record_key_of(&p);
            if !seen.insert(key.clone()) {
                continue;
            }
            let rec = self.record(&key)?;
            let dep = rec.invocation()?;
            for o in &rec.outputs {
                if [".rlib", ".dylib", ".so", ".a"]
                    .iter()
                    .any(|e| o.ends_with(e))
                {
                    libs.insert(rec.out_dir.join(o));
                }
            }
            queue.extend(dep.externs.iter().cloned());
        }
        Some(libs)
    }
}

fn which(p: &Path) -> PathBuf {
    if p.components().count() > 1 {
        return p.to_path_buf();
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(p))
                .find(|c| c.is_file())
        })
        .unwrap_or_else(|| p.to_path_buf())
}

/// A GNU make jobserver like Cargo's: a pipe holding one token per job. Each
/// rustc the executor starts takes a token; rustc itself takes more for its
/// codegen threads only while tokens are free, so the machine is never
/// oversubscribed.
pub struct Jobserver {
    r: i32,
    w: i32,
    pub makeflags: String,
}

impl Jobserver {
    pub fn new(tokens: usize) -> io::Result<Jobserver> {
        let mut fds = [0i32; 2];
        // SAFETY: fds is a valid 2-int array. The fds stay inheritable (no
        // CLOEXEC) so rustc children can use the jobserver.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let js = Jobserver {
            r: fds[0],
            w: fds[1],
            makeflags: format!(
                "-j --jobserver-fds={0},{1} --jobserver-auth={0},{1}",
                fds[0], fds[1]
            ),
        };
        for _ in 0..tokens {
            js.release();
        }
        Ok(js)
    }

    pub fn acquire(&self) {
        let mut b = [0u8; 1];
        loop {
            // SAFETY: reading one byte into a valid buffer from our own pipe.
            let n = unsafe { libc::read(self.r, b.as_mut_ptr().cast(), 1) };
            if n == 1 || (n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted)
            {
                return;
            }
        }
    }

    pub fn release(&self) {
        // SAFETY: writing one byte from a valid buffer to our own pipe.
        unsafe { libc::write(self.w, b"|".as_ptr().cast(), 1) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Outputs already in place and matching the store.
    Fresh,
    /// Restored from the store.
    Hit,
    /// rustc ran.
    Compiled,
    /// rustc failed; exit code.
    Failed(i32),
}

/// How rustc's stderr reaches the user.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    /// Raw JSON lines, for Cargo to parse (wrapper mode).
    Raw,
    /// Rendered diagnostics only (executor mode).
    Rendered,
}

fn emit(line: &str, echo: Echo, err: &mut impl Write) {
    match echo {
        Echo::Raw => {
            let _ = writeln!(err, "{line}");
        }
        Echo::Rendered => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(r) = v.get("rendered").and_then(|r| r.as_str()) {
                    let _ = write!(err, "{r}");
                }
            } else {
                let _ = writeln!(err, "{line}");
            }
        }
    }
}

/// The path in a rustc artifact notice (`{"$message_type":"artifact","artifact":"…"}`).
fn artifact_path(line: &str) -> Option<String> {
    if !line.contains("\"$message_type\":\"artifact\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("artifact")?.as_str().map(str::to_string)
}

fn is_meta_notice(line: &str) -> bool {
    line.contains("\"$message_type\":\"artifact\"") && line.contains("\"emit\":\"metadata\"")
}

thread_local! {
    /// Phase timings of the last `run_unit` on this thread:
    /// key, lookup, jobserver wait, rustc, after rustc.
    pub static PHASES: std::cell::Cell<[f64; 5]> = const { std::cell::Cell::new([0.0; 5]) };
}

fn phase(i: usize, since: std::time::Instant) {
    PHASES.with(|p| {
        let mut v = p.get();
        v[i] = since.elapsed().as_secs_f64();
        p.set(v);
    });
}

/// Runs one unit: fresh, store hit, or rustc. `on_meta` fires once the
/// unit's `.rmeta` exists (pipelining). Returns the outcome and output names.
pub fn run_unit(
    ctx: &Ctx,
    rec: &Record,
    on_meta: &dyn Fn(),
    echo: Echo,
    js: Option<&Jobserver>,
) -> io::Result<(Outcome, Vec<String>)> {
    let inv = rec
        .invocation()
        .ok_or_else(|| io::Error::other("not a compile invocation"))?;
    let t = ctx.target_s.as_str();
    PHASES.with(|p| p.set([0.0; 5]));
    let t_key = std::time::Instant::now();
    let mkey = ctx.manifest_key(rec, &inv)?;
    phase(0, t_key);
    let t_lookup = std::time::Instant::now();
    let env_get = |k: &str| rec.env_get(k);

    let found = mkey
        .as_ref()
        .and_then(|mkey| ctx.store.lookup(mkey, t, &ctx.hc, &env_get));
    phase(1, t_lookup);
    if let Some(e) = found {
        // Under Cargo (the wrapper) Cargo has decided this unit must be
        // rebuilt and will expect its outputs to be newer than its inputs, so
        // identical bytes already in place are restamped like a restore;
        // only the executor may leave a present unit untouched.
        let executor = matches!(echo, Echo::Rendered);
        let outcome = if executor && ctx.store.is_present(&e, &inv.out_dir, &ctx.hc) {
            ctx.store.touch(&e.obj);
            Outcome::Fresh
        } else {
            ctx.store.materialize(&e, &inv.out_dir, t, &ctx.hc)?;
            Outcome::Hit
        };
        replay(&e, t, echo);
        on_meta();
        return Ok((outcome, e.files.iter().map(|(n, _)| n.clone()).collect()));
    }

    let start = SystemTime::now();
    let t_rustc = std::time::Instant::now();
    let mut cmd = Command::new(&rec.rustc);
    cmd.args(&rec.args)
        .current_dir(&rec.cwd)
        .envs(rec.env.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());
    if let Some(js) = js {
        cmd.env("CARGO_MAKEFLAGS", &js.makeflags);
        js.acquire();
    }
    phase(2, t_rustc);
    let t_rustc = std::time::Instant::now();
    let child = cmd.spawn();
    let result = child.and_then(|child| finish_rustc(child, echo, on_meta));
    if let Some(js) = js {
        js.release();
    }
    let (status, lines, meta_sent) = result?;
    phase(3, t_rustc);
    let t_store = std::time::Instant::now();
    if !status.success() {
        return Ok((
            Outcome::Failed(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            ),
            vec![],
        ));
    }
    if !meta_sent {
        on_meta();
    }

    // Main outputs come from rustc's own artifact notices; the full set
    // (debug-info .o files kept beside them) is found when storing, off the
    // critical path: an out dir can hold tens of thousands of objects.
    let mut names: Vec<String> = lines
        .iter()
        .filter_map(|l| artifact_path(l))
        .filter_map(|p| {
            Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .collect();
    names.push(
        inv.dep_info()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string(),
    );
    names.sort();
    names.dedup();

    if let Some(mkey) = mkey
        && let Ok(text) = fs::read_to_string(inv.dep_info())
    {
        let d = depinfo::parse(&text, &rec.cwd, &inv.out_dir);
        let job = PutJob {
            mkey,
            inputs: d.files,
            env: d.env,
            out_dir: inv.out_dir.clone(),
            prefixes: inv.output_prefixes(),
            artifacts: lines
                .iter()
                .filter_map(|l| artifact_path(l))
                .map(PathBuf::from)
                .collect(),
            dep_info: inv.dep_info(),
            stderr: lines.iter().map(|l| norm(l, t)).collect(),
            start,
        };
        match ctx.deferred.lock().unwrap().as_mut() {
            Some(queue) => queue.push(job),
            None => ctx.complete_put(job)?,
        }
    }
    phase(4, t_store);
    Ok((Outcome::Compiled, names))
}

/// A finished compile waiting to be stored.
#[derive(Serialize, Deserialize)]
pub struct PutJob {
    mkey: String,
    inputs: Vec<PathBuf>,
    env: Vec<(String, Option<String>)>,
    out_dir: PathBuf,
    prefixes: [String; 2],
    /// Paths from rustc's artifact notices.
    artifacts: Vec<PathBuf>,
    dep_info: PathBuf,
    stderr: Vec<String>,
    /// When rustc started: an input modified after this may not be what
    /// rustc read, so the result is not stored.
    start: SystemTime,
}

impl Ctx {
    /// Queue store writes instead of doing them before the unit counts as
    /// done (executor); drain with `drain_puts`.
    pub fn defer_puts(&self) {
        *self.deferred.lock().unwrap() = Some(Vec::new());
    }

    /// All queued store writes.
    pub fn take_puts(&self) -> Vec<PutJob> {
        self.deferred
            .lock()
            .unwrap()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    pub fn drain_puts(&self, jobs: usize) {
        let queue = self.take_puts();
        self.run_puts(queue, jobs);
    }

    pub fn run_puts(&self, queue: Vec<PutJob>, jobs: usize) {
        let queue = Mutex::new(queue);
        std::thread::scope(|s| {
            for _ in 0..jobs.max(1) {
                s.spawn(|| {
                    while let Some(job) = queue.lock().unwrap().pop() {
                        let _ = self.complete_put(job); // the store is a cache
                    }
                });
            }
        });
    }

    /// Exactly what a compile left in its out dir: rustc's artifacts, its
    /// dep-info, the `.o` members of its rlib and the objects in its linked
    /// binary's debug map. Without artifact notices, falls back to name
    /// prefix + mtime.
    fn unit_outputs(&self, job: &PutJob) -> io::Result<Vec<PathBuf>> {
        use crate::rustc_args::matches_prefixes;
        let mine = |p: &Path| {
            p.parent() == Some(job.out_dir.as_path())
                && p.file_name()
                    .is_some_and(|n| matches_prefixes(&job.prefixes, &n.to_string_lossy()))
                && p.is_file()
        };
        let mut out: BTreeSet<PathBuf> = BTreeSet::new();
        let mut exact = !job.artifacts.is_empty();
        for a in &job.artifacts {
            if !a.is_file() {
                continue;
            }
            out.insert(a.clone());
            match a.extension().and_then(|e| e.to_str()) {
                Some("rmeta") => {}
                Some("rlib") | Some("a") => {
                    for m in crate::objects::ar_members(a)?
                        .into_iter()
                        .filter(|m| m.ends_with(".o"))
                    {
                        let p = job.out_dir.join(m);
                        if mine(&p) {
                            out.insert(p);
                        }
                    }
                }
                _ => match crate::objects::debug_map_objects(a) {
                    Some(objs) => out.extend(objs.into_iter().filter(|p| mine(p))),
                    None => exact = false,
                },
            }
        }
        if !exact {
            out.extend(
                fs::read_dir(&job.out_dir)?
                    .filter_map(Result::ok)
                    .filter(|e| matches_prefixes(&job.prefixes, &e.file_name().to_string_lossy()))
                    .filter(|e| {
                        e.metadata()
                            .and_then(|m| m.modified())
                            .is_ok_and(|m| m >= job.start)
                    })
                    .map(|e| e.path())
                    .filter(|p| p.is_file()),
            );
        }
        if job.dep_info.is_file() {
            out.insert(job.dep_info.clone());
        }
        Ok(out.into_iter().collect())
    }

    pub fn complete_put(&self, job: PutJob) -> io::Result<()> {
        let t = self.target_s.as_str();
        let mut outputs = self.unit_outputs(&job)?;
        outputs.sort();
        let mut inputs = Vec::with_capacity(job.inputs.len());
        for f in &job.inputs {
            let Ok(md) = fs::metadata(f) else {
                return Ok(());
            };
            if md.modified().is_ok_and(|m| m > job.start) {
                return Ok(()); // edited during the compile
            }
            let Ok(h) = self.hc.get(f) else { return Ok(()) };
            inputs.push((norm(&f.to_string_lossy(), t), h));
        }
        self.store
            .put(
                &job.mkey, inputs, job.env, &outputs, job.stderr, t, &self.hc,
            )
            .map(drop)
    }
}

/// Streams rustc's stderr (signalling the `.rmeta` notice) and waits.
fn finish_rustc(
    mut child: std::process::Child,
    echo: Echo,
    on_meta: &dyn Fn(),
) -> io::Result<(std::process::ExitStatus, Vec<String>, bool)> {
    let mut lines = Vec::new();
    let mut meta_sent = false;
    for line in BufReader::new(child.stderr.take().unwrap()).lines() {
        let line = line?;
        // stderr is locked per line only; `on_meta` takes the scheduler lock
        // and must never run under it.
        emit(&line, echo, &mut io::stderr().lock());
        if !meta_sent && is_meta_notice(&line) {
            meta_sent = true;
            on_meta();
        }
        lines.push(line);
    }
    Ok((child.wait()?, lines, meta_sent))
}

fn replay(e: &Entry, target: &str, echo: Echo) {
    let err = io::stderr();
    let mut err = err.lock();
    for l in &e.stderr {
        emit(&denorm(l, target), echo, &mut err);
    }
}
