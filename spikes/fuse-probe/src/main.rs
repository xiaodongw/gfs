//! M0.2 FUSE deployment spike.
//!
//! Answers, by measurement: can we mount in the target environment, at what
//! privilege, and what does the kernel actually do with our caching and
//! concurrency choices.

mod fs;
mod origin;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fs::{Dispatch, ProbeFs, Shared};
use fuser::{Config, MountOption, SessionACL};
use origin::OriginStats;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(about = "XVFS M0.2 FUSE deployment probe")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount and stay up, for out-of-process tests (containers, daemon death).
    Mount {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, default_value = "pooled")]
        dispatch: String,
        #[arg(long, default_value_t = 0)]
        latency_ms: u64,
        #[arg(long, default_value_t = 64)]
        files: usize,
        #[arg(long, default_value_t = 4096)]
        file_size: usize,
        #[arg(long)]
        allow_other: bool,
        #[arg(long)]
        auto_unmount: bool,
    },
    /// Mount in-process, run the measurement workload, unmount, and report.
    Measure {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, default_value_t = 64)]
        files: usize,
        #[arg(long, default_value_t = 65536)]
        file_size: usize,
        /// Per-request origin latency; what makes serialization visible at all.
        #[arg(long, default_value_t = 20)]
        latency_ms: u64,
        #[arg(long, default_value_t = 16)]
        parallel: usize,
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Report what this environment permits, without asserting anything.
    Capabilities,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Capabilities => capabilities(),
        Cmd::Mount {
            dir,
            dispatch,
            latency_ms,
            files,
            file_size,
            allow_other,
            auto_unmount,
        } => {
            let d = if dispatch == "blocking" { Dispatch::Blocking } else { Dispatch::Pooled };
            let mut cfg = base_config(8);
            if allow_other {
                cfg.acl = SessionACL::All;
            }
            if auto_unmount {
                cfg.mount_options.push(MountOption::AutoUnmount);
            }
            let m = mount(&dir, d, latency_ms, files, file_size, Duration::from_secs(60), &cfg)?;
            println!("mounted at {}", dir.display());
            // Block until signalled; the session unmounts when dropped.
            std::thread::park();
            drop(m);
            Ok(())
        }
        Cmd::Measure { dir, files, file_size, latency_ms, parallel, json } => {
            measure(&dir, files, file_size, latency_ms, parallel, json.as_deref())
        }
    }
}

/// The session configuration every mount starts from.
///
/// `n_threads` is a first-class knob in fuser 0.18, so how many FUSE event-loop
/// threads exist is a deployment choice rather than a library constant. It is
/// set explicitly here because leaving it unspecified means one thread, and one
/// thread plus a blocking read is a serialized mount.
fn base_config(n_threads: usize) -> Config {
    // `Config` is #[non_exhaustive], so it is built by mutation rather than a
    // struct literal — which is the point: new options appear with defaults
    // instead of breaking the build, so any that matter must be set explicitly.
    let mut c = Config::default();
    c.mount_options = vec![
        MountOption::FSName("xvfs-probe".into()),
        MountOption::RO,
        // The mount holds someone else's source: no setuid, no device nodes.
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::NoAtime,
    ];
    // Access control is not a mount option in fuser 0.18; `allow_other` is
    // expressed only through the session ACL.
    c.acl = SessionACL::Owner;
    c.n_threads = Some(n_threads);
    // Each worker gets its own /dev/fuse fd via FUSE_DEV_IOC_CLONE (Linux 4.5+).
    c.clone_fd = n_threads > 1;
    c
}

struct Mounted {
    /// Never read, and must not be removed: dropping the session is what
    /// unmounts the filesystem. Deleting this "unused" field would leave every
    /// probe mount live until the process exits.
    #[allow(dead_code)]
    session: fuser::BackgroundSession,
    shared: Arc<Shared>,
    origin: Arc<OriginStats>,
}

fn mount(
    dir: &Path,
    dispatch: Dispatch,
    latency_ms: u64,
    files: usize,
    file_size: usize,
    attr_ttl: Duration,
    config: &Config,
) -> Result<Mounted> {
    std::fs::create_dir_all(dir)?;
    let (entries, blobs) = fs::build_tree(files, file_size);
    let origin = origin::start(blobs, Duration::from_millis(latency_ms))?;

    let shared = Arc::new(Shared {
        by_parent_name: fs::index(&entries),
        entries,
        origin: origin::OriginClient::new(origin.port),
        counters: Default::default(),
        cache: Mutex::new(Default::default()),
        // A fixed, stable base time: identical across remounts and hosts, the
        // property DESIGN.md section 8.2 requires and which anything derived
        // from the host clock would not have.
        snapshot_time: fs::epoch(1_600_000_000),
        attr_ttl,
        entry_ttl: attr_ttl,
        in_flight: Default::default(),
    });

    let probe = ProbeFs::new(Arc::clone(&shared), dispatch, 16);
    let session = fuser::spawn_mount(probe, dir, config)
        .with_context(|| format!("mounting at {}", dir.display()))?;
    Ok(Mounted { session, shared, origin: origin.stats })
}

#[derive(serde::Serialize)]
struct Report {
    environment: serde_json::Value,
    measurements: Vec<Measurement>,
}

#[derive(serde::Serialize)]
struct Measurement {
    name: String,
    detail: String,
}

fn m(name: &str, detail: impl Into<String>) -> Measurement {
    Measurement { name: name.into(), detail: detail.into() }
}

fn measure(
    dir: &Path,
    files: usize,
    file_size: usize,
    latency_ms: u64,
    parallel: usize,
    json: Option<&Path>,
) -> Result<()> {
    let mut out = Vec::new();

    // ---- mount cost -------------------------------------------------------
    let t = Instant::now();
    let mnt = mount(
        dir,
        Dispatch::Pooled,
        latency_ms,
        files,
        file_size,
        Duration::from_secs(60),
        &base_config(8),
    )?;
    out.push(m(
        "mount_time",
        format!("{:.1} ms to a usable mount point", t.elapsed().as_secs_f64() * 1000.0),
    ));

    // ---- root readdir -----------------------------------------------------
    let t = Instant::now();
    let listed = std::fs::read_dir(dir)?.count();
    out.push(m(
        "readdir_root",
        format!("{listed} entries in {:.2} ms", t.elapsed().as_secs_f64() * 1000.0),
    ));

    // ---- attribute caching ------------------------------------------------
    // With a long TTL the kernel must answer repeated stats itself. If it does
    // not, an `ls -l` over a monorepo becomes one round trip per file, which is
    // the difference between a usable mount and an unusable one.
    let probe_file = dir.join("file-0000");
    let before = mnt.shared.counters.getattr.load(Ordering::Relaxed);
    for _ in 0..1000 {
        std::fs::metadata(&probe_file)?;
    }
    let getattrs = mnt.shared.counters.getattr.load(Ordering::Relaxed) - before;
    out.push(m(
        "attr_cache",
        format!("1000 stat(2) calls produced {getattrs} getattr upcalls at a 60 s TTL"),
    ));

    // ---- cold sequential reads -------------------------------------------
    let t = Instant::now();
    let mut bytes = 0u64;
    for i in 0..files {
        let name = format!("file-{i:04}");
        bytes += std::fs::read(dir.join(&name))
            .or_else(|_| std::fs::read(dir.join("sub").join(&name)))
            .map(|v| v.len() as u64)
            .unwrap_or(0);
    }
    out.push(m(
        "sequential_cold_read",
        format!(
            "{files} files, {bytes} bytes in {:.0} ms at {latency_ms} ms origin latency",
            t.elapsed().as_secs_f64() * 1000.0
        ),
    ));

    // ---- warm reads -------------------------------------------------------
    let before_req = mnt.origin.blob_requests.load(Ordering::Relaxed);
    let t = Instant::now();
    for i in 0..files {
        let name = format!("file-{i:04}");
        let _ = std::fs::read(dir.join(&name)).or_else(|_| std::fs::read(dir.join("sub").join(&name)));
    }
    let new_reqs = mnt.origin.blob_requests.load(Ordering::Relaxed) - before_req;
    out.push(m(
        "warm_read",
        format!(
            "re-read in {:.1} ms with {new_reqs} new origin fetches",
            t.elapsed().as_secs_f64() * 1000.0
        ),
    ));

    // ---- read-only enforcement and symlink --------------------------------
    let link = std::fs::read_link(dir.join("link-to-first"))?;
    let (blocks, bsize) = statfs(dir)?;
    let write_err = std::fs::write(dir.join("file-0000"), b"nope")
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "SUCCEEDED (unexpected)".into());
    out.push(m("readlink", format!("link-to-first -> {}", link.display())));
    out.push(m(
        "statfs",
        format!("{blocks} blocks of {bsize} bytes (overlay quota, not the host filesystem)"),
    ));
    out.push(m("read_only", format!("write to a base file: {write_err}")));
    drop(mnt);

    // ---- dispatch model: the measurement M2's design depends on -----------
    //
    // Three configurations, because there are two independent ways to get
    // concurrency and they need to be told apart: more FUSE event-loop threads,
    // and not blocking the ones you have.
    for (label, dispatch, threads) in [
        ("blocking_1thread", Dispatch::Blocking, 1),
        ("blocking_8thread", Dispatch::Blocking, 8),
        ("pooled_1thread", Dispatch::Pooled, 1),
    ] {
        let sub = PathBuf::from(format!("{}-{label}", dir.display()));
        let mnt = mount(
            &sub,
            dispatch,
            latency_ms,
            files,
            file_size,
            Duration::from_secs(60),
            &base_config(threads),
        )?;
        let t = Instant::now();
        std::thread::scope(|s| {
            let per = files.div_ceil(parallel.max(1));
            for chunk in (0..files).collect::<Vec<_>>().chunks(per) {
                let chunk = chunk.to_vec();
                let sub = &sub;
                s.spawn(move || {
                    for i in chunk {
                        let name = format!("file-{i:04}");
                        let _ = std::fs::read(sub.join(&name))
                            .or_else(|_| std::fs::read(sub.join("sub").join(&name)));
                    }
                });
            }
        });
        let elapsed = t.elapsed().as_secs_f64() * 1000.0;
        let peak = mnt.shared.counters.peak_concurrent_reads.load(Ordering::Relaxed);
        out.push(m(
            &format!("parallel_read_{label}"),
            format!(
                "{files} files over {parallel} reader threads in {elapsed:.0} ms; \
                 peak {peak} concurrent origin fetches"
            ),
        ));
        drop(mnt);
        let _ = std::fs::remove_dir(&sub);
    }

    let report = Report { environment: environment()?, measurements: out };
    for x in &report.measurements {
        println!("{:26}  {}", x.name, x.detail);
    }
    if let Some(p) = json {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(p, serde_json::to_string_pretty(&report)?)?;
        println!("\njson report: {}", p.display());
    }
    Ok(())
}

fn statfs(dir: &Path) -> Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    anyhow::ensure!(unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0, "statvfs failed");
    Ok((s.f_blocks as u64, s.f_bsize as u64))
}

fn environment() -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "kernel": read_trim("/proc/sys/kernel/osrelease"),
        "uid": unsafe { libc::getuid() },
        "in_container": Path::new("/.dockerenv").exists(),
        "dev_fuse": Path::new("/dev/fuse").exists(),
        "fuse_conf_user_allow_other":
            std::fs::read_to_string("/etc/fuse.conf")
                .map(|s| s.lines().any(|l| l.trim() == "user_allow_other"))
                .unwrap_or(false),
        "fusermount": which("fusermount3").or_else(|| which("fusermount")),
    }))
}

fn read_trim(p: &str) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn which(bin: &str) -> Option<String> {
    std::env::var("PATH").ok()?.split(':').find_map(|d| {
        let p = Path::new(d).join(bin);
        p.exists().then(|| p.display().to_string())
    })
}

/// What this environment permits. Reports rather than asserts: the answer
/// differs per deployment target, and that difference is the finding.
fn capabilities() -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&environment()?)?);
    println!();

    let base = std::env::temp_dir().join(format!("xvfs-cap-{}", std::process::id()));
    // `AutoUnmount` requires allow_other (or allow_root) to be permitted, so
    // the two are varied together: whether the host daemon can rely on the
    // kernel cleaning up after a crashed daemon depends on both.
    for (label, acl, auto_unmount) in [
        ("plain", SessionACL::Owner, false),
        ("allow_other", SessionACL::All, false),
        ("auto_unmount", SessionACL::Owner, true),
        ("allow_other+auto_unmount", SessionACL::All, true),
        ("multithreaded+clone_fd", SessionACL::Owner, false),
    ] {
        let dir = base.join(label.replace('+', "_"));
        let mut cfg = base_config(if label.starts_with("multithreaded") { 8 } else { 1 });
        cfg.acl = acl;
        if auto_unmount {
            cfg.mount_options.push(MountOption::AutoUnmount);
        }
        match mount(&dir, Dispatch::Pooled, 0, 2, 64, Duration::from_secs(1), &cfg) {
            Ok(mnt) => {
                let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
                println!("{label:26} mounted ok ({n} entries visible)");
                drop(mnt);
            }
            Err(e) => println!("{label:26} FAILED: {}", root_cause(&e)),
        }
        let _ = std::fs::remove_dir(&dir);
    }
    let _ = std::fs::remove_dir(&base);
    Ok(())
}

fn root_cause(e: &anyhow::Error) -> String {
    e.chain().last().map(|c| c.to_string()).unwrap_or_else(|| e.to_string())
}
