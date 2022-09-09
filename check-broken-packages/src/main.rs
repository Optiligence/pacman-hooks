use std::cmp;
use std::env;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;

use ansi_term::Colour::*;
use anyhow::Context;
use crossbeam::thread as cb_thread;
use glob::glob;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use log::debug;
use simple_logger::SimpleLogger;

type CrossbeamChannel<T> = (
    crossbeam::channel::Sender<T>,
    crossbeam::channel::Receiver<T>,
);

/// Executable file work unit for a worker thread to process
#[derive(Debug)]
struct ExecFileWork {
    /// AUR package name
    #[allow(clippy::rc_buffer)]
    package: Arc<String>,

    // Executable filepath
    #[allow(clippy::rc_buffer)]
    exec_filepath: Arc<String>,

    /// True if this is the last executable filepath for the package (used to report progress)
    package_last: bool,
}

struct PythonPackageVersion {
    major: u8,
    minor: u8,
    release: u8,
    package: u8,
}

impl fmt::Display for PythonPackageVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}-{}",
            self.major, self.minor, self.release, self.package
        )
    }
}

fn get_python_version() -> anyhow::Result<PythonPackageVersion> {
    let output = Command::new("pacman")
        .args(&["-Qi", "python"])
        .env("LANG", "C")
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to query Python version with pacman",);
    }

    let version_line = output
        .stdout
        .lines()
        .filter_map(Result::ok)
        .find(|l| l.starts_with("Version"))
        .ok_or_else(|| anyhow::anyhow!("Unexpected pacman output: unable to find version line"))?;
    let version_str = version_line
        .split(':')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Unexpected pacman output: unable to parse version line"))?
        .trim_start();

    let mut dot_iter = version_str.split('.');
    let major = u8::from_str(dot_iter.next().ok_or_else(|| {
        anyhow::anyhow!("Unexpected pacman output: unable to parse Python version major part")
    })?)?;
    let minor = u8::from_str(dot_iter.next().ok_or_else(|| {
        anyhow::anyhow!("Unexpected pacman output: unable to parse Python version minor part")
    })?)?;
    let mut dash_iter = dot_iter
        .next()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unexpected pacman output: unable to parse Python version release/package part",
            )
        })?
        .split('-');
    let release = u8::from_str(dash_iter.next().ok_or_else(|| {
        anyhow::anyhow!("Unexpected pacman output: unable to parse Python version release part")
    })?)?;
    let package = u8::from_str(dash_iter.next().ok_or_else(|| {
        anyhow::anyhow!("Unexpected pacman output: unable to parse Python version package part")
    })?)?;

    Ok(PythonPackageVersion {
        major,
        minor,
        release,
        package,
    })
}

fn get_package_owning_path(path: &str) -> anyhow::Result<Vec<String>> {
    let output = Command::new("pacman")
        .args(&["-Fq", path])
        .env("LANG", "C")
        .output()?;

    Ok(output
        .stdout
        .lines().map(|l| l.map(|i| i.split_once('/').expect("no file").1.to_string()))
        .collect::<Result<Vec<String>, std::io::Error>>()?)
}

fn get_broken_python_packages(
    current_python_version: &PythonPackageVersion,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut packages = Vec::new();

    let current_python_dir = format!(
        "/usr/lib/python{}.{}",
        current_python_version.major, current_python_version.minor
    );

    for python_dir_entry in glob(&format!("/usr/lib/python{}*", current_python_version.major))? {
        let python_dir = python_dir_entry?
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Failed to convert OS string to native string"))?;

        if python_dir != current_python_dir {
            let dir_packages = get_package_owning_path(&python_dir)?;
            for package in dir_packages {
                let couple = (package, python_dir.clone());
                if !packages.contains(&couple) {
                    packages.push(couple);
                }
            }
        }
    }

    Ok(packages)
}

fn get_aur_packages() -> anyhow::Result<Vec<String>> {
    let output = Command::new("pacman")
        .args(&["-Qqm"])
        .env("LANG", "C")
        .output()?;

    Ok(output
        .stdout
        .lines()
        .collect::<Result<Vec<String>, std::io::Error>>()?)
}

fn get_package_linked_files(package: &str) -> anyhow::Result<Vec<String>> {
    let output = Command::new("pacman")
        .args(&["-Ql", package])
        .env("LANG", "C")
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to list files for package {:?} with pacman", package);
    }

    let files = output
        .stdout
        .lines()
        .collect::<Result<Vec<String>, _>>()?
        .into_iter()
        .filter_map(|l| l.split(' ').nth(1).map(|p| p.to_string()))
        .map(|s| {
            fs::read_link(&s)
                .map(|p| p.to_str().unwrap().to_string())
                .unwrap_or(s)
        })
        .filter(|p| {
            fs::metadata(&p)
                .map(|m| m.file_type().is_file() && ((m.permissions().mode() & 0o111) != 0 || (p.chars().filter(|&c|c=='/').count() == 3 && p.ends_with(".so"))))
                .unwrap_or(false)
        })
        .collect();

    Ok(files)
}

fn is_direct_dep(exec_file: &str, dep: &str) -> anyhow::Result<bool> {
    return Ok(Command::new("patchelf")
        .args(&["--print-needed", exec_file])
        .output()?.stdout.lines().any(|d| d.unwrap() == dep));
}

fn get_missing_dependencies(exec_file: &str) -> anyhow::Result<Vec<String>> {
    let output = Command::new("ldd")
        .args(&[exec_file])
        .env("LANG", "C")
        .output()?;

    let missing_deps = if output.status.success() {
        output
            .stdout
            .lines()
            .collect::<Result<Vec<String>, _>>()?
            .into_iter()
            .filter(|l| l.ends_with("=> not found"))
            .filter_map(|l| l.split(' ').next().map(|s| s.to_owned()))
            .map(|l| l.trim_start().to_string())
            //.filter(|l| !output.status.success() || direct_deps.contains(l))
            .collect()
    } else {
        Vec::new()
    };

    Ok(missing_deps)
}

fn get_sd_enabled_service_links() -> anyhow::Result<Vec<PathBuf>> {
    let mut dirs_content = [
        glob("/etc/systemd/system/*.target.*"),
        glob("/etc/systemd/user/*.target.*"),
    ];

    let service_links: Vec<PathBuf> = dirs_content
        .iter_mut()
        .flatten()
        .flatten()
        .collect::<Result<Vec<PathBuf>, _>>()?
        .into_iter()
        .filter_map(|p| fs::read_dir(p.as_path()).ok())
        .flatten()
        .flatten()
        .filter(|f| f.file_type().map_or(false, |f| f.is_symlink()))
        .map(|f| f.path())
        .collect();

    Ok(service_links)
}

fn is_valid_link(link: &Path) -> anyhow::Result<bool> {
    let mut target: PathBuf = link.into();
    loop {
        target = fs::read_link(target)?;
        let metadata = match fs::metadata(&target) {
            Err(_) => {
                return Ok(false);
            }
            Ok(m) => m,
        };

        let ftype = metadata.file_type();
        if ftype.is_file() {
            return Ok(true);
        } else if ftype.is_symlink() {
            continue;
        } else {
            anyhow::bail!("Unexpected file type for {:?}", target);
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Init logger
    SimpleLogger::new()
        .init()
        .context("Failed to init logger")?;

    // Python broken packages channel
    let (python_broken_packages_tx, python_broken_packages_rx) = crossbeam::unbounded();
    thread::Builder::new()
        .spawn(move || {
            let to_send = match get_python_version() {
                Ok(current_python_version) => {
                    debug!("Python version: {}", current_python_version);
                    let broken_python_packages =
                        get_broken_python_packages(&current_python_version);
                    match broken_python_packages {
                        Ok(broken_python_packages) => broken_python_packages,
                        Err(err) => {
                            eprintln!("Failed to list Python packages: {}", err);
                            Vec::<(String, String)>::new()
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Failed to get Python version: {}", err);
                    Vec::<(String, String)>::new()
                }
            };
            python_broken_packages_tx.send(to_send).unwrap();
        })
        .context("Failed to start thread")?;

    // Get usable core count
    let cpu_count = num_cpus::get();

    // Get package names
    let aur_packages = get_aur_packages().context("Unable to get list of AUR packages")?;

    // Get systemd enabled services
    let enabled_sd_service_links =
        get_sd_enabled_service_links().context("Unable to Systemd enabled services")?;
    let mut broken_sd_service_links: Vec<PathBuf> = Vec::new();

    // Init progressbar
    let progress = ProgressBar::with_draw_target(
        (aur_packages.len() + enabled_sd_service_links.len()) as u64,
        ProgressDrawTarget::stderr(),
    );
    progress.set_style(ProgressStyle::default_bar().template("Analyzing {wide_bar} {pos}/{len}"));

    // Missing deps channel
    let (missing_deps_tx, missing_deps_rx) = crossbeam::unbounded();

    cb_thread::scope(|scope| {
        // Executable file channel
        let (exec_files_tx, exec_files_rx): CrossbeamChannel<ExecFileWork> = crossbeam::unbounded();

        // Executable files to missing deps workers
        for _ in 0..cpu_count {
            let exec_files_rx = exec_files_rx.clone();
            let missing_deps_tx = missing_deps_tx.clone();
            let progress = progress.clone();
            scope.spawn(move |_| {
                while let Ok(exec_file_work) = exec_files_rx.recv() {
                    debug!("exec_files_rx => {:?}", &exec_file_work);
                    let missing_deps = get_missing_dependencies(&exec_file_work.exec_filepath);
                    match missing_deps {
                        Ok(missing_deps) => {
                            for missing_dep in missing_deps {
                                let to_send = (
                                    Arc::clone(&exec_file_work.package),
                                    Arc::clone(&exec_file_work.exec_filepath),
                                    missing_dep.clone(),
                                    get_package_owning_path(missing_dep.split("/").last().unwrap().split_inclusive(".so").next().unwrap()).unwrap_or(vec!["?".to_string()])
                                );
                                debug!("{:?} => missing_deps_tx", &to_send);
                                if missing_deps_tx.send(to_send).is_err() {
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            eprintln!(
                                "Failed to get missing dependencies for path {:?}: {}",
                                &exec_file_work.exec_filepath, err
                            );
                        }
                    }
                    if exec_file_work.package_last {
                        progress.inc(1);
                    }
                }
            });
        }

        // Drop this end of the channel, workers have their own clone
        drop(missing_deps_tx);

        cb_thread::scope(|scope| {
            // Package name channel
            let (package_tx, package_rx): CrossbeamChannel<Arc<String>> = crossbeam::unbounded();

            // Package name to executable files workers
            let worker_count = cmp::min(cpu_count, aur_packages.len());
            for _ in 0..worker_count {
                let package_rx = package_rx.clone();
                let exec_files_tx = exec_files_tx.clone();
                let progress = progress.clone();
                scope.spawn(move |_| {
                    while let Ok(package) = package_rx.recv() {
                        debug!("package_rx => {:?}", package);
                        let exec_files = match get_package_linked_files(&package) {
                            Ok(exec_files) => exec_files,
                            Err(err) => {
                                eprintln!(
                                    "Failed to get executable files of package {:?}: {}",
                                    &package, err
                                );
                                progress.inc(1);
                                continue;
                            }
                        };
                        if exec_files.is_empty() {
                            progress.inc(1);
                            continue;
                        }

                        // Exclude executables in commonly used non standard directories,
                        // likely to also use non standard library locations
                        const BLACKLISTED_EXE_DIRS: [&str; 2] = ["/opt/", "/usr/share/"];
                        for (i, exec_file) in exec_files
                            .iter()
                            .filter(|p| !BLACKLISTED_EXE_DIRS.iter().any(|d| p.starts_with(d)))
                            .enumerate()
                        {
                            let to_send = ExecFileWork {
                                package: Arc::clone(&package),
                                exec_filepath: Arc::new(exec_file.to_string()),
                                package_last: i == exec_files.len() - 1,
                            };
                            debug!("{:?} => exec_files_tx", &to_send);
                            if exec_files_tx.send(to_send).is_err() {
                                break;
                            }
                        }
                    }
                });
            }

            // Drop this end of the channel, workers have their own clone
            drop(exec_files_tx);

            // Send package names
            for aur_package in aur_packages {
                debug!("{:?} => package_tx", aur_package);
                package_tx.send(Arc::new(aur_package)).unwrap();
            }
        })
        .unwrap();

        // We don't bother to use a worker thread for this, the overhead is not worth it
        broken_sd_service_links = enabled_sd_service_links
            .iter()
            .filter(|s| !is_valid_link(s).unwrap_or(true))
            .map(|l| l.to_owned())
            .collect();
        progress.inc(enabled_sd_service_links.len() as u64);
    })
    .unwrap();

    progress.finish_and_clear();

    let mut libmap = HashMap::<String, HashMap<Arc<String>, BinaryHeap<Arc<String>>>>::new();
    let mut trans2 = HashSet::<String>::new();
    let mut pacmap = HashMap::<String, HashSet<String>>::new();
    let mut pacsourcemap = HashMap::<String, String>::new();
    for (package, file, missing_dep, pkg) in missing_deps_rx.iter() {
        println!("{} {} {} {}", pkg.join(" "), missing_dep.clone(), package.clone(), file.clone());
        if is_direct_dep(file.as_str(), &missing_dep.clone()).unwrap_or(true) {
            libmap.entry(missing_dep.clone()).or_default().entry(package.clone()).or_default().push(file);
            //pacmap.entry(package.to_string()).or_default().insert(pkg.join(", "));
            pacmap.entry(package.to_string()).or_default().insert(missing_dep.clone());
        } else {
            trans2.insert(package.to_string());
        }
        if !pkg.is_empty() {
            pacsourcemap.insert(missing_dep.clone(), pkg[0].clone());
        }
    }
    let mut trans = HashSet::<String>::new();
    for t in trans2 {
        if ! pacmap.contains_key(&t) {
            trans.insert(t);
        }
    }

    for missing_dep in libmap.keys() {
        //if libmap[missing_dep].keys().len() == 1 { continue }
        print!("package{} need rebuild because of missing {}:", if libmap[missing_dep].keys().len() > 1 { "s" } else { "" }, Yellow.paint(missing_dep));
        for package in libmap[missing_dep].keys() {
            print!(" {}", Red.paint(package.to_string()));
        }
        println!();
    }

    for pkg in pacmap.keys() {
        print!("package {} misses ", Red.paint(pkg));
        for (i, file) in pacmap[pkg].iter().enumerate() {
            print!("{}", Yellow.paint(file));
            if pacsourcemap.contains_key(file) {
                print!(" from {}", Cyan.paint(pacsourcemap[file].clone()));
            }
            if i+1 < pacmap[pkg].len() {
                print!(";");
            }
        }
        println!();
    }

    if !trans.is_empty() {
        let t3 = trans.clone();
        let mut x = t3.iter().map(|t| Yellow.paint(t));
        x.next().and_then(|t| { print!("{}", t); Some(t) } );
        x.for_each(|t| print!(", {}", t));
        println!();

        trans.clone().iter().map(|t| Yellow.paint(t)).take(1).for_each(|t| print!("{}", t));
        trans.clone().iter().map(|t| Yellow.paint(t)).skip(1).for_each(|t| print!(", {}", t));
        println!();

        trans.clone().iter().map(|t| Yellow.paint(t)).scan("", |sep,t|{ print!("{}{}", *sep, t); *sep=", "; Some(0) }).for_each(drop);
        println!();

        for (t, i) in trans.clone().iter().zip(std::iter::once("").chain(std::iter::repeat(", "))) {
            print!("{}{}", i, Yellow.paint(t));
        }
        println!();

        std::iter::once("").chain(std::iter::repeat(", ")).zip(trans.clone().iter().map(|t| Yellow.paint(t))).for_each(|t| print!("{}{}", t.0, t.1));
        println!();

        std::iter::once("").chain(std::iter::repeat(", ")).zip(trans.clone()).for_each(|t| print!("{}{}", t.0, Yellow.paint(t.1)));
        println!();

        for (i, t) in trans.iter().map(|t| Yellow.paint(t)).enumerate() {
            match i {
                0 => print!("{}", t),
                _ => print!(", {}", t)
            }
        }
        println!();

        for (i, t) in trans.iter().enumerate() {
            print!("{}{}", if i > 0 {", "} else {""}, Yellow.paint(t));
        }
        println!();

        for (i, t) in trans.iter().enumerate() {
            print!("{}{}", ["", ", "][(i>0) as usize], Yellow.paint(t));
        }
        println!();

        for (d, p) in [", ", ""].iter().zip(trans.iter().collect::<Vec<_>>().chunks(trans.len()-1)) {
            for e in p {
                print!("{}{}", Yellow.paint(*e), d);
            }
        }
        println!();

        let mut it = trans.iter().map(|t| Yellow.paint(t));
        if let Some(first) = it.next() {
            print!("{}", first);
            for e in it {
                print!(", {}", e);
            }
        }
        println!();

        let mut sep = "transitively broken packages: "; for t in trans { print!("{}{}", sep, Yellow.paint(t)); sep = ", "; }
        println!();
    }

    if env::args().any(|x| x.starts_with("-v") || x.starts_with("--v")) {
        println!("{:#?}", libmap);
        println!("{:#?}", pacmap);
    }

    if let Ok(broken_python_packages) = python_broken_packages_rx.recv() {
        for (broken_python_package, dir) in broken_python_packages {
            println!(
                "{}",
                Yellow.paint(format!(
                    "Package {:?} has files in directory {:?} that are ignored by the current Python interpreter",
                    broken_python_package, dir
                ))
            );
        }
    }

    for broken_sd_service_link in broken_sd_service_links {
        println!(
            "{}",
            Yellow.paint(format!(
                "Systemd enabled service has broken link in {:?}",
                &broken_sd_service_link,
            ))
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs::{File, Permissions};
    use std::io::Write;
    use std::path::PathBuf;

    use tempdir::TempDir;

    use super::*;

    fn update_path(dir: &str) -> std::ffi::OsString {
        let path_orig = env::var_os("PATH").unwrap();

        let mut paths_vec = env::split_paths(&path_orig).collect::<Vec<_>>();
        paths_vec.insert(0, PathBuf::from(dir));

        let paths = env::join_paths(paths_vec).unwrap();
        env::set_var("PATH", &paths);

        path_orig
    }

    #[test]
    fn test_get_missing_dependencies() {
        let ldd_output = "	linux-vdso.so.1 (0x00007ffea89a7000)
	libavdevice.so.57 => not found
	libavfilter.so.6 => not found
	libavformat.so.57 => not found
	libavcodec.so.57 => not found
	libavresample.so.3 => not found
	libpostproc.so.54 => not found
	libswresample.so.2 => not found
	libswscale.so.4 => not found
	libavutil.so.55 => not found
	libm.so.6 => /usr/lib/libm.so.6 (0x00007f4bd9cc3000)
	libpthread.so.0 => /usr/lib/libpthread.so.0 (0x00007f4bd9ca2000)
	libc.so.6 => /usr/lib/libc.so.6 (0x00007f4bd9add000)
	/lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2 (0x00007f4bda08d000)
";

        let tmp_dir = TempDir::new("").unwrap();

        let output_filepath = tmp_dir.path().join("output.txt");
        let mut output_file = File::create(&output_filepath).unwrap();
        output_file.write_all(ldd_output.as_bytes()).unwrap();
        drop(output_file);

        let fake_ldd_filepath = tmp_dir.path().join("ldd");
        let mut fake_ldd_file = File::create(fake_ldd_filepath).unwrap();
        write!(
            &mut fake_ldd_file,
            "#!/bin/sh\ncat {}",
            output_filepath.into_os_string().into_string().unwrap()
        )
        .unwrap();
        fake_ldd_file
            .set_permissions(Permissions::from_mode(0o700))
            .unwrap();
        drop(fake_ldd_file);

        let path_orig = update_path(tmp_dir.path().to_str().unwrap());

        let missing_deps = get_missing_dependencies("dummy");
        assert!(missing_deps.is_ok());
        assert_eq!(
            missing_deps.unwrap(),
            [
                "libavdevice.so.57",
                "libavfilter.so.6",
                "libavformat.so.57",
                "libavcodec.so.57",
                "libavresample.so.3",
                "libpostproc.so.54",
                "libswresample.so.2",
                "libswscale.so.4",
                "libavutil.so.55"
            ]
        );

        env::set_var("PATH", &path_orig);
    }
}
