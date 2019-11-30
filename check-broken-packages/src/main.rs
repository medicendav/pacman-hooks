use std::cmp;
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::BufRead;
use std::iter::FromIterator;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;

use ansi_term::Colour::*;
use crossbeam::thread as cb_thread;
use glob::glob;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use log::debug;

type CrossbeamChannel<T> = (
    crossbeam::channel::Sender<T>,
    crossbeam::channel::Receiver<T>,
);

/// Executable file work unit for a worker thread to process
#[derive(Debug)]
struct ExecFileWork {
    /// AUR package name
    package: Arc<String>,

    // Executable filepath
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

impl fmt::Debug for PythonPackageVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}-{}",
            self.major, self.minor, self.release, self.package
        )
    }
}

fn get_python_version() -> PythonPackageVersion {
    let output = Command::new("pacman")
        .args(&["-Qi", "python"])
        .env("LANG", "C")
        .output()
        .unwrap();

    if !output.status.success() {
        panic!();
    }

    let version_str = output
        .stdout
        .lines()
        .map(std::result::Result::unwrap)
        .filter(|l| l.starts_with("Version"))
        .map(|l| l.split(':').nth(1).unwrap().trim_start().to_string())
        .next()
        .unwrap();

    let mut dot_iter = version_str.split('.');
    let major = u8::from_str(dot_iter.next().unwrap()).unwrap();
    let minor = u8::from_str(dot_iter.next().unwrap()).unwrap();
    let mut dash_iter = dot_iter.next().unwrap().split('-');
    let release = u8::from_str(dash_iter.next().unwrap()).unwrap();
    let package = u8::from_str(dash_iter.next().unwrap()).unwrap();

    PythonPackageVersion {
        major,
        minor,
        release,
        package,
    }
}

fn get_package_owning_path(path: &str) -> VecDeque<String> {
    let output = Command::new("pacman")
        .args(&["-Qoq", path])
        .output()
        .unwrap();

    if !output.status.success() {
        panic!();
    }

    output
        .stdout
        .lines()
        .map(std::result::Result::unwrap)
        .collect()
}

fn get_broken_python_packages(
    current_python_version: &PythonPackageVersion,
) -> VecDeque<(String, String)> {
    let mut packages = VecDeque::new();

    let current_python_dir = format!(
        "/usr/lib/python{}.{}",
        current_python_version.major, current_python_version.minor
    );

    for python_dir_entry in
        glob(&format!("/usr/lib/python{}*", current_python_version.major)).unwrap()
    {
        let python_dir = python_dir_entry
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap();

        if python_dir != current_python_dir {
            let dir_packages = get_package_owning_path(&python_dir);
            for package in dir_packages {
                let couple = (package, python_dir.clone());
                if !packages.contains(&couple) {
                    packages.push_back(couple);
                }
            }
        }
    }

    packages
}

fn get_aur_packages() -> Vec<String> {
    let output = Command::new("pacman").args(&["-Qqm"]).output().unwrap();

    if !output.status.success() {
        panic!();
    }

    Vec::from_iter(output.stdout.lines().map(std::result::Result::unwrap))
}

fn get_package_executable_files(package: &str) -> VecDeque<String> {
    let mut files = VecDeque::new();

    let output = Command::new("pacman")
        .args(&["-Ql", package])
        .output()
        .unwrap();

    if !output.status.success() {
        panic!();
    }

    for line in output.stdout.lines() {
        let line = line.unwrap();
        let path = line.split(' ').nth(1).unwrap().to_string();
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_e) => continue,
        };
        if metadata.file_type().is_file() && ((metadata.permissions().mode() & 0o111) != 0) {
            files.push_back(path);
        }
    }

    files
}

fn get_missing_dependencies(exec_file: &str) -> VecDeque<String> {
    let mut missing_deps = VecDeque::new();

    let output = Command::new("ldd").args(&[exec_file]).output().unwrap();

    if output.status.success() {
        for missing_dep in output
            .stdout
            .lines()
            .map(std::result::Result::unwrap)
            .filter(|l| l.ends_with("=> not found"))
            .map(|l| l.split(' ').next().unwrap().trim_start().to_string())
        {
            missing_deps.push_back(missing_dep);
        }
    }

    missing_deps
}

fn main() {
    // Init logger
    simple_logger::init().unwrap();

    // Python broken packages channel
    let (python_broken_packages_tx, python_broken_packages_rx) = crossbeam::unbounded();
    thread::Builder::new()
        .spawn(move || {
            let current_python_version = get_python_version();
            debug!("Python version: {:?}", current_python_version);

            let broken_python_packages = get_broken_python_packages(&current_python_version);
            python_broken_packages_tx
                .send(broken_python_packages)
                .unwrap();
        })
        .unwrap();

    // Get usable core count
    let cpu_count = num_cpus::get();

    // Get package names
    let aur_packages = get_aur_packages();

    // Init progressbar
    let progress =
        ProgressBar::with_draw_target(aur_packages.len() as u64, ProgressDrawTarget::stderr());
    progress.set_style(
        ProgressStyle::default_bar().template("Analyzing packages {wide_bar} {pos}/{len}"),
    );

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
                    for missing_dep in missing_deps {
                        let to_send = (
                            Arc::clone(&exec_file_work.package),
                            Arc::clone(&exec_file_work.exec_filepath),
                            missing_dep,
                        );
                        debug!("{:?} => missing_deps_tx", &to_send);
                        if missing_deps_tx.send(to_send).is_err() {
                            break;
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
                        let exec_files = get_package_executable_files(&package);
                        if exec_files.is_empty() {
                            progress.inc(1);
                            continue;
                        }
                        for (i, exec_file) in exec_files.iter().enumerate() {
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
    })
    .unwrap();

    progress.finish_and_clear();

    for (package, file, missing_dep) in missing_deps_rx.iter() {
        println!(
            "{}",
            Yellow.paint(format!(
                "File '{}' from package '{}' is missing dependency '{}'",
                file, package, missing_dep
            ))
        );
    }

    let broken_python_packages = python_broken_packages_rx.recv().unwrap();
    for (broken_python_package, dir) in broken_python_packages {
        println!(
            "{}",
            Yellow.paint(format!(
                "Package '{}' has files in directory '{}' that are ignored by the current Python interpreter",
                broken_python_package, dir
            ))
        );
    }
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
            .set_permissions(Permissions::from_mode(0o777))
            .unwrap();
        drop(fake_ldd_file);

        let path_orig = update_path(tmp_dir.path().to_str().unwrap());

        assert_eq!(
            get_missing_dependencies("dummy"),
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
