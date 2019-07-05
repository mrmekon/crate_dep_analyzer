use crates_io_api::{SyncClient, Crate, Error, Sort, ListOptions};
use serde::{Deserialize};
use serde_json::{Deserializer};
use std::process::{Command, Stdio};
use std::io::Write;

#[derive(Deserialize, Debug)]
struct BuildMetadataTarget {
    kind: Vec<String>,
    crate_types: Vec<String>,
    name: String,
}

#[derive(Deserialize, Debug)]
struct BuildMetadata {
    reason: String,
    package_id: String,
    target: Option<BuildMetadataTarget>,
    filenames: Option<Vec<String>>,
    executable: Option<String>
}

#[derive(Debug, PartialEq)]
enum ArtifactType {
    Binary,
    RustLibrary,
}

#[derive(Debug)]
struct Artifact {
    filename: String,
    kind: ArtifactType,
    size: u64,
}

#[derive(Debug)]
struct CrateResult {
    deps: usize,
    artifacts: Vec<Artifact>,
}
impl std::fmt::Display for CrateResult {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let libs: Vec<&Artifact> = self.artifacts.iter().filter(|a| a.kind == ArtifactType::RustLibrary).collect();
        let bins: Vec<&Artifact> = self.artifacts.iter().filter(|a| a.kind == ArtifactType::Binary).collect();
        let lib = libs.first().map(|a| a.size).unwrap_or(0);
        let bin = bins.first().map(|a| a.size).unwrap_or(0);
        let lib_str = match lib {
            0 => "        ".to_string(),
            _ => format!("{:>8.2}", lib as f64 / 1024. / 1024.),
        };
        let bin_str = match bin {
            0 => "        ".to_string(),
            _ => format!("{:>8.2}", bin as f64 / 1024. / 1024.),
        };
        let res = write!(f, " {:>6} \t {} \t {} ", self.deps, lib_str, bin_str);
        if libs.len() > 1 {
            let _ = write!(f, "  WARNING: >1 lib");
        }
        res
    }
}

#[derive(Debug)]
struct PkgId {
    name: String,
    version: String,
}

fn top_crates(count: usize, category: Option<String>, filter_fn: Option<fn(&Crate) -> bool>) -> Result<Vec<Crate>, Error> {
    let mut remaining = count;
    let mut page = 1;
    let mut all_crates = Vec::<Crate>::with_capacity(count);
    while remaining > 0 {
        let client = SyncClient::new();
        let options = ListOptions {
            sort: Sort::Downloads,
            per_page: 100,
            page: page,
            query: None,
            category: category.clone(),
        };
        let mut crates = client.crates(options)?;

        crates.crates.retain(|c| match filter_fn {
            Some(f) => f(&c),
            None => true,
        });
        all_crates.append(&mut crates.crates);
        remaining -= match remaining {
            x if x > 100 => 100,
            x => x,
        };
        page += 1;
    }
    Ok(all_crates)
}

#[allow(dead_code)]
fn search_crates(query: &str) -> Result<Vec<Crate>, Error> {
    let client = SyncClient::new();
    let options = ListOptions {
        sort: Sort::Downloads,
        per_page: 100,
        page: 1,
        query: Some(query.to_string()),
        category: None,
    };
    let crates = client.crates(options)?;
    Ok(crates.crates)
}

fn pkgid(crt: &Crate) -> Result<PkgId, std::io::Error> {
    let dir = format!("clone_{}", crt.id);
    let output = Command::new("cargo")
        .args(&["pkgid"])
        .current_dir(&dir)
        .output()
        .expect("pkgid failed");
    let output = String::from_utf8(output.stdout).expect("Unreadable pkgid output");
    let substr = output.split("#").nth(1).expect("pkgid missing #");
    let mut bits = substr.split(":");

    Ok(PkgId {
        name: bits.next().expect("pkgid missing crate").trim().into(),
        version: bits.next().expect("pkgid missing version").trim().into(),
    })
}

fn artifacts(_crt: &Crate, metadata: &str, pkgid: &PkgId) -> Result<Vec<Artifact>, std::io::Error> {
    let mut artifacts: Vec<Artifact> = vec!();
    let pkgid_str = format!("{} {}", pkgid.name, pkgid.version);
    let stream  = Deserializer::from_str(metadata).into_iter::<BuildMetadata>();
    let mut meta_objs: Vec<BuildMetadata> = vec!();
    for value in stream {
        let m: BuildMetadata = value.expect("Fail parsing metadata json");
        if m.package_id.starts_with(&pkgid_str) &&
            m.reason == "compiler-artifact" &&
            m.target.is_some() {
            meta_objs.push(m);
        }
    }
    for m in &meta_objs {
        let target = m.target.as_ref().unwrap();
        if target.kind.iter().any(|x| x == "bin") {
            if let Some(exe) = &m.executable {
                artifacts.push(Artifact {
                    filename: exe.clone(),
                    kind: ArtifactType::Binary,
                    size: 0,
                });
            }
        }
        if target.kind.iter().any(|x| x == "lib" || x == "rlib") {
            if let Some(filenames) = &m.filenames {
                for file in filenames {
                    if file.ends_with(".rlib") {
                        artifacts.push(Artifact {
                            filename: file.clone(),
                            kind: ArtifactType::RustLibrary,
                            size: 0,
                        });
                    }
                }
            }
        }
    }
    for mut arty in &mut artifacts {
        if let Ok(file) = std::fs::File::open(&arty.filename) {
            if let Ok(stat) = file.metadata() {
                arty.size = stat.len();
            }
        }
    }
    Ok(artifacts)
}

fn analyze_crate(crt: &Crate) -> Result<CrateResult, std::io::Error> {
    let dir = format!("clone_{}", crt.id);
    let repo = crt.repository.as_ref().ok_or(std::io::ErrorKind::NotFound)?;
    // Always provide a username/password so git fails fast if one is required.
    let repo = repo.replace("https://", "https://dummy_user:dummy_password@");
    let _result = Command::new("git")
        .args(&["clone", "--recursive", "--quiet", &repo, &dir])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone failed");
    if !std::path::Path::new(&dir).exists() {
        return Err(std::io::ErrorKind::Other.into());
    }

    let cargo_toml_path = format!("{}/Cargo.toml", dir);
    if !std::path::Path::new(&cargo_toml_path).exists() {
        return Err(std::io::ErrorKind::Other.into());
    }

    let result = Command::new("cargo")
        .args(&["build", "--release", "--message-format=json"])
        .current_dir(&dir)
        .stderr(Stdio::null())
        .output()
        .expect("build failed");
    if !result.status.success() {
        return Err(std::io::ErrorKind::Other.into());
    }
    let metadata = String::from_utf8(result.stdout).expect("Unreadable pkgid output");

    // $ cargo tree --no-indent -a |sort |uniq -c |sort -nr |wc -l
    let mut cargo_result = Command::new("cargo")
        .current_dir(&dir)
        .args(&["tree", "--no-indent", "--no-dev-dependencies", "-a"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("tree failed");
    let cargo_out = cargo_result.stdout.take().expect("Cargo tree stdout failed");
    let mut sort_result = Command::new("sort")
        .current_dir(&dir)
        .stdin(Stdio::from(cargo_out))
        .stdout(Stdio::piped())
        .spawn()
        .expect("sort failed");
    let sort_out = sort_result.stdout.take().expect("sort stdout failed");
    let mut awk_result = Command::new("awk")
        .current_dir(&dir)
        .args(&["{print $1}"])
        .stdin(Stdio::from(sort_out))
        .stdout(Stdio::piped())
        .spawn()
        .expect("awk failed");
    let awk_out = awk_result.stdout.take().expect("awk stdout failed");
    let mut uniq_result = Command::new("uniq")
        .current_dir(&dir)
        .args(&["-c"])
        .stdin(Stdio::from(awk_out))
        .stdout(Stdio::piped())
        .spawn()
        .expect("uniq failed");
    let uniq_out = uniq_result.stdout.take().expect("uniq stdout failed");
    let sort2_result = Command::new("sort")
        .current_dir(&dir)
        .args(&["-nr"])
        .stdin(Stdio::from(uniq_out))
        .stdout(Stdio::piped())
        .spawn()
        .expect("sort failed");
    let output = sort2_result.wait_with_output().expect("sort failed");
    let _ = cargo_result.wait();
    let _ = sort_result.wait();
    let _ = awk_result.wait();
    let _ = uniq_result.wait();
    let output = String::from_utf8(output.stdout).expect("Unreadable output");

    // Subtract 1 for the root crate.
    let dep_count = match output.lines().count() {
        e if e > 0 => e - 1,
        _ => return Err(std::io::ErrorKind::Other.into()),
    };

    let artifacts = artifacts(crt, &metadata, &pkgid(crt)?)?;

    Ok(CrateResult {
        deps: dep_count,
        artifacts: artifacts,
    })
}

#[derive(Debug, Default)]
struct Statistics {
    count: usize,
    mean: f64,
    median: f64,
    stddev: f64,
    max: usize,
}

#[derive(Debug)]
struct BatchStatistics {
    deps: Statistics,
    libs: Statistics,
    bins: Statistics,
}

fn statistics(crates: &Vec<CrateResult>) -> Result<BatchStatistics, std::io::Error> {
    let deps: Vec<usize> = crates.iter().filter_map(|c| match c.deps {
        0 => None,
        c => Some(c),
    }).collect();
    let deps_f64: Vec<f64> = deps.iter().map(|v| *v as f64).collect();

    let libs: Vec<u64> = crates.iter().filter_map(|c| {
        c.artifacts.iter().filter_map(|a| {
            match a.kind {
                ArtifactType::RustLibrary => Some(a.size),
                _ => None,
            }
        }).next()
    }).collect();
    let libs_f64: Vec<f64> = libs.iter().map(|v| *v as f64).collect();

    let bins: Vec<u64> = crates.iter().filter_map(|c| {
        c.artifacts.iter().filter_map(|a| {
            match a.kind {
                ArtifactType::Binary => Some(a.size),
                _ => None,
            }
        }).next()
    }).collect();
    let bins_f64: Vec<f64> = bins.iter().map(|v| *v as f64).collect();

    Ok(BatchStatistics {
        deps: Statistics {
            count: deps.len(),
            mean: statistical::mean(deps_f64.as_slice()),
            median: statistical::median(deps_f64.as_slice()),
            stddev: statistical::standard_deviation(deps_f64.as_slice(), None),
            max: *deps.iter().max().unwrap_or(&0),
        },
        libs: Statistics {
            count: libs.len(),
            mean: statistical::mean(libs_f64.as_slice()),
            median: statistical::median(libs_f64.as_slice()),
            stddev: statistical::standard_deviation(libs_f64.as_slice(), None),
            max: *libs.iter().max().unwrap_or(&0) as usize,
        },
        bins: Statistics {
            count: bins.len(),
            mean: statistical::mean(bins_f64.as_slice()),
            median: statistical::median(bins_f64.as_slice()),
            stddev: statistical::standard_deviation(bins_f64.as_slice(), None),
            max: *bins.iter().max().unwrap_or(&0) as usize,
        },
    })
}

fn analyze(crates: Vec<Crate>) {
    let blacklist = [
        "rustc-ap-rustc_cratesio_shim", // all of rust compiler
        "rustc-ap-rustc_target",
        "rustc-ap-serialize",
        "rustc-ap-rustc_data_structures",
        "rustc-ap-syntax_pos",
        "rustc-ap-syntax",
        "rustc-ap-rustc_errors",
        // these are identical to rls-analysis
        "rls-data",
        "rls-span",
        "rls-vfs",
        // these are identical to actix-http
        "actix-files",
        "actix-http-test",
        "actix-web",
        "actix-web-httpauth",
        // identical to winapi
        "winapi-build",
        "winapi-i686-pc-windows-gnu",
        "winapi-x86_64-pc-windows-gnu",
        // identical to rand
        "rand_xorshift",
        "rand_pcg",
        "rand_os",
        "rand_jitter",
        "rand_isaac",
        "rand_hc",
        "rand_core",
        "rand_chacha",
        // identical to wayland-client
        "wayland-commons",
        "wayland-kbd",
        "wayland-protocols",
        "wayland-scanner",
        "wayland-server",
        "wayland-window",
        // identical to clone_tokio
        "clone_tokio-codec",
        "clone_tokio-core",
        "clone_tokio-curl",
        "clone_tokio-current-thread",
        "clone_tokio-executor",
        "clone_tokio-fs",
        "clone_tokio-io",
        "clone_tokio-proto",
        "clone_tokio-reactor",
        "clone_tokio-service",
        "clone_tokio-signal",
        "clone_tokio-sync",
        "clone_tokio-tcp",
        "clone_tokio-threadpool",
        "clone_tokio-timer",
        "clone_tokio-tls",
        "clone_tokio-trace-core",
        "clone_tokio-tungstenite",
        "clone_tokio-udp",
        "clone_tokio-uds",
    ];

    // Buckets of 1, up to 20
    let mut buckets: [u8; 22] = [
        0,0,0,0,0,0,0,0,0,0,
        0,0,0,0,0,0,0,0,0,0,
        0,
        0,
    ];

    let mut results: Vec<CrateResult> = vec!();
    println!("");
    println!("{:<32}:  {:>6} \t {:>7} \t {:>7} ", "CRATE", "DEPS", "LIB (MB)", "BIN (MB)");
    println!("{}", std::iter::repeat("-").take(73).collect::<String>());
    let crate_count = crates.len();
    for (idx,c) in crates.iter().enumerate() {
        if !blacklist.contains(&c.id.as_str()) {
            let progress = format!("[{:>3}/{}]", idx, crate_count);
            print!("{:<10} {:<21}: ", progress, c.id.chars().take(21).collect::<String>());
            let _ = std::io::stdout().flush();
            match analyze_crate(c) {
                Err(_) => {
                    println!("");
                },
                Ok(res) => {
                    println!("{}", res);
                    match res.deps {
                        e if e <= 20 => buckets[e] += 1,
                        _ => buckets[21] += 1,
                    }
                    results.push(res);
                },
            }
        }
    }

    let stats = statistics(&results).expect("failed to generate statistics");

    println!("");
    println!("Number of crates analyzed: {}", results.len());
    println!("");
    println!("Dependencies:");
    println!("    count: {}", stats.deps.count);
    println!("     mean: {:.2} +/- {:.2}", stats.deps.mean, stats.deps.stddev);
    println!("   median: {:.2}", stats.deps.median);
    println!("  maximum: {}", stats.deps.max);
    println!("");
    println!("Library size:");
    println!("    count: {}", stats.libs.count);
    println!("     mean: {:.2} +/- {:.2} [{:.2} MB + / {:.2} MB]",
             stats.libs.mean, stats.libs.stddev,
             stats.libs.mean / 1024. / 1024., stats.libs.stddev / 1024. / 1024.);
    println!("   median: {:.2} [{:.2} MB]", stats.libs.median, stats.libs.median / 1024. / 1024.);
    println!("  maximum: {} [{:.2} MB]", stats.libs.max, stats.libs.max as f64 / 1024. / 1024.);
    println!("");
    println!("Binary size:");
    println!("    count: {}", stats.bins.count);
    println!("     mean: {:.2} +/- {:.2} [{:.2} MB + / {:.2} MB]",
             stats.bins.mean, stats.bins.stddev,
             stats.bins.mean / 1024. / 1024., stats.bins.stddev / 1024. / 1024.);
    println!("   median: {:.2} [{:.2} MB]", stats.bins.median, stats.bins.median / 1024. / 1024.);
    println!("  maximum: {} [{:.2} MB]", stats.bins.max, stats.bins.max as f64 / 1024. / 1024.);

    println!("");
    println!("Dependency count histogram (buckets 0-20 by 1, 20+):");
    for (i, count) in buckets.iter().enumerate() {
        let idx = match i {
            21 => "> 20".to_string(),
            _ => format!("{:>4}", i),
        };
        print!("{} ({:>5.1}%): ", idx, 100.0 * (*count as f64) / results.len() as f64);
        println!("{}", ['*'].iter().cycle().take(*count as usize).collect::<String>());
    }

    // Buckets of 10, up to 200
    let mut buckets: [u8; 21] = [
        0,0,0,0,0,0,0,0,0,0,
        0,0,0,0,0,0,0,0,0,0,
        0,
    ];
    for res in &results {
        match res.deps {
            e if e < 200 => buckets[e / 10] += 1,
            _ => buckets[20] += 1,
        }
    }

    println!("");
    println!("Dependency count histogram (buckets 0-200 by 10, 200+):");
    for (i, count) in buckets.iter().enumerate() {
        let idx = match i {
            20 => "    > 200".to_string(),
            _ => format!("{:>3} - {:>3}", 10*i, 10*(i+1)),
        };
        print!("{} ({:>5.1}%): ", idx, 100.0 * (*count as f64) / results.len() as f64);
        println!("{}", ['*'].iter().cycle().take(std::cmp::min(50, *count as usize)).collect::<String>());
    }
    println!("");
}

fn main() {
    println!("========== 200 command-line-utilities crates ==========");
    let crates = top_crates(200, Some("command-line-utilities".into()), None).unwrap();
    analyze(crates);

    println!("========== 100 graphics crates ==========");
    let crates = top_crates(100, Some("graphics".into()), None).unwrap();
    analyze(crates);

    println!("========== 100 gui crates ==========");
    let crates = top_crates(100, Some("gui".into()), None).unwrap();
    analyze(crates);

    println!("========== 100 web-programming crates ==========");
    let crates = top_crates(100, Some("web-programming".into()), None).unwrap();
    analyze(crates);

    println!("========== Top 400 crates ==========");
    let crates = top_crates(400, None, None).unwrap();
    analyze(crates);
}
