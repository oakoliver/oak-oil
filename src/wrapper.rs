//! `RUSTC_WRAPPER` mode: Cargo schedules, Oak Oil answers each rustc call
//! from the store or runs rustc, and records the unit for the executor.

use crate::hashing::HashCache;
use crate::rustc_args::{Invocation, target_dir_of};
use crate::unit::{Ctx, Echo, Outcome, Record, run_unit, save_record, select_env};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Instant;

pub fn run(argv: Vec<String>) -> ExitCode {
    let rustc = PathBuf::from(&argv[0]);
    let args = argv[1..].to_vec();
    let passthrough = |rustc: &PathBuf, args: &[String]| {
        let code = Command::new(rustc)
            .args(args)
            .status()
            .map(|s| s.code().unwrap_or(1))
            .unwrap_or(1);
        ExitCode::from(code as u8)
    };
    let Some(inv) = Invocation::parse(&args) else {
        return passthrough(&rustc, &args);
    };
    let target = std::env::var_os("OAKOIL_TARGET")
        .map(PathBuf::from)
        .or_else(|| target_dir_of(&inv.out_dir));
    let Some(target) = target.filter(|t| inv.out_dir.starts_with(t)) else {
        return passthrough(&rustc, &args);
    };
    let Ok(ctx) = Ctx::new(target.clone(), HashCache::default()) else {
        return passthrough(&rustc, &args);
    };

    let mut rec = Record {
        rustc: rustc.clone(),
        args: args.clone(),
        cwd: std::env::current_dir().unwrap_or_default(),
        env: select_env(std::env::vars()),
        out_dir: inv.out_dir.clone(),
        key: crate::unit::unit_key(&inv.crate_name, &inv.extra, &inv.out_dir),
        outputs: vec![],
    };
    let t0 = Instant::now();
    // Store writes go to a spool file, done after the build by the drain:
    // Cargo sees the unit finished as soon as rustc does.
    ctx.defer_puts();
    let result = run_unit(&ctx, &rec, &|| {}, Echo::Raw, None);
    let _ = crate::drain::spool(&ctx, &rec.key);
    match result {
        Ok((Outcome::Failed(code), _)) => ExitCode::from(code.clamp(1, 255) as u8),
        Ok((outcome, outputs)) => {
            rec.outputs = outputs;
            let _ = save_record(&target, &rec);
            log(&target, &rec.key, outcome, t0.elapsed().as_secs_f64());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("oil: {e}; running rustc directly");
            passthrough(&rustc, &args)
        }
    }
}

fn log(target: &std::path::Path, key: &str, outcome: Outcome, secs: f64) {
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(target.join("oakoil").join("wrapper.log"))
    {
        let p = crate::unit::PHASES.with(|p| p.get());
        let _ = writeln!(
            f,
            "{key}\t{outcome:?}\t{secs:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}",
            p[0], p[1], p[2], p[3], p[4]
        );
    }
}
