//! Store writes outside the build: the executor hands its queue to a
//! detached `cargo oil store-drain` running at background QoS (efficiency
//! cores on Apple Silicon) and returns. `cargo oil wait-store` waits for
//! running drains.

use crate::hashing::HashCache;
use crate::unit::{Ctx, PutJob};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
struct Batch {
    target: PathBuf,
    jobs: Vec<PutJob>,
}

fn drains_dir(store_root: &Path) -> PathBuf {
    store_root.join("drains")
}

pub fn spawn(ctx: &Ctx) -> io::Result<()> {
    let jobs = ctx.take_puts();
    if jobs.is_empty() {
        return Ok(());
    }
    let dir = drains_dir(ctx.store.root());
    fs::create_dir_all(&dir)?;
    let file = dir.join(format!(
        "batch-{}-{}.json",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    fs::write(
        &file,
        serde_json::to_vec(&Batch {
            target: ctx.target.clone(),
            jobs,
        })?,
    )?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("store-drain")
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid in the child before exec only detaches it from our
    // session and process group; it touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

/// `cargo oil store-drain <batch>`: store every job of the batch.
pub fn run(batch_file: &Path) -> io::Result<()> {
    background_qos();
    let batch: Batch = serde_json::from_slice(&fs::read(batch_file)?)?;
    let ctx = Ctx::new(batch.target, HashCache::default())?;
    let marker = drains_dir(ctx.store.root()).join(format!("{}.pid", std::process::id()));
    fs::write(&marker, b"")?;
    let jobs = std::thread::available_parallelism().map_or(4, |n| n.get() / 2);
    ctx.run_puts(batch.jobs, jobs);
    let _ = fs::remove_file(batch_file);
    let _ = fs::remove_file(marker);
    Ok(())
}

/// Waits until no drain is running; returns how long it waited.
pub fn wait(store_root: &Path) -> Duration {
    let t0 = Instant::now();
    let dir = drains_dir(store_root);
    loop {
        let busy = fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("batch-") {
                    return true;
                }
                let pid: i32 = name.trim_end_matches(".pid").parse().unwrap_or(0);
                // SAFETY: signal 0 only checks that the process exists.
                pid > 0 && unsafe { libc::kill(pid, 0) } == 0
            });
        if !busy {
            return t0.elapsed();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// This thread yields to compiles: on Apple Silicon, background QoS runs it
/// on the efficiency cores.
pub fn background_qos() {
    #[cfg(target_os = "macos")]
    // SAFETY: sets the calling thread's own QoS class; no pointers involved.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
    }
}
