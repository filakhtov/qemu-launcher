mod arguments;
mod config;
mod cpuset;
mod environment;
mod process;
mod qmp;
#[cfg(test)]
mod test;

use arguments::Arguments;
use environment::Environment;
use rlimit::{setrlimit, Resource, Rlim};
use std::{
    env, fs,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
};

fn usage(name: &str) {
    let programname = match Path::new(name).file_name() {
        Some(n) => match n.to_os_string().into_string() {
            Ok(string) => string,
            Err(_) => "qemu-launcher".to_string(),
        },
        None => "qemu-launcher".to_string(),
    };

    eprintln!("Usage: {} [-v] [-d] [-h] <vm-name>", programname);
    eprintln!("");
    eprintln!("-h  display this help message");
    eprintln!("-v  enable verbose mode. In this mode additional information about program execution flow will be \
        printed.");
    eprintln!("-d  enable debugging mode. In this mode a lot of information about pretty much every step taken by \
        the application will be printed.");
    eprintln!("");
    eprintln!("Supported environment variables:");
    eprintln!("- QEMU_LAUNCHER_CONFIG_DIR - a path to the directory where virtual machine configuration files are \
        stored.");
    eprintln!("- QEMU_LAUNCHER_CPUSET_MOUNT_PATH - a path to the directory where a cpuset cgroup tree will be \
        mounted.");
    eprintln!("                                    default: /sys/fs/cgroup/cpuset");
    eprintln!("- QEMU_LAUNCHER_CPUSET_PREFIX - a prefix (directory) under the mount path where qemu cpusets will \
        be created");
    eprintln!("                                default: qemu");
    eprintln!("");
}

fn handle_vcpu_pinning(child: &mut Child, cpuset: &mut cpuset::CpuSet, config: &config::Config) {
    let stdin = match child.stdin.as_mut() {
        Some(stdin) => stdin,
        None => {
            eprintln!("Unable to obtain qemu process stdin descriptor.");
            return;
        }
    };
    let stdout = match child.stdout.as_mut() {
        Some(stdout) => stdout,
        None => {
            eprintln!("Unable to obtain qemu process stdout descriptor.");
            return;
        }
    };
    let qmp_socket = qmp::StdioReadWrite::new(stdin, stdout);

    let vcpu_info = match qmp::read_vcpu_info_from_qmp_socket(qmp_socket) {
        Ok(vcpu_info) => vcpu_info,
        Err(e) => {
            eprintln!("Failed to obtain vCPU mapping info from QEMU: {}", e);
            return;
        }
    };

    for pin in config.get_cpu_pinning() {
        let task_id = match vcpu_info.get_thread_id(pin.0, pin.1, pin.2) {
            Some(tid) => tid,
            None => {
                eprintln!(
                    "The vCPU core `{}.{}.{}` does not exist, unable to pin.",
                    pin.0, pin.1, pin.2
                );
                continue;
            }
        };

        if let Err(e) = cpuset.pin_task(pin.3, task_id) {
            eprintln!(
                "Failed to pin the vCPU `{}.{}.{}` core task ID `{}` to the host CPU `{}`: {}",
                pin.0, pin.1, pin.2, pin.3, task_id, e
            );
        }
    }

    if config.has_scheduling() {
        let scheduler = config.get_scheduler().clone().unwrap();
        let priority = config.get_priority().unwrap().to_string();

        for task_id in vcpu_info.get_task_ids() {
            match process::Process::oneshot(
                "chrt",
                &[
                    format!("--{}", scheduler).as_str(),
                    "--pid",
                    &priority,
                    task_id.to_string().as_str(),
                ],
            ) {
                Ok(_) => {} // TODO: debug
                Err(e) => eprintln!("Failed to change vCPU thread `{}` priority: {}", task_id, e),
            }
        }
    }
}

fn main() {
    let env = match Environment::new(env::vars()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Unable to parse environment variables: {}", e);
            return;
        }
    };

    let args = match Arguments::new(&env::args().collect()) {
        Arguments::Empty => panic!("Could not parse arguments. Aborting."),
        Arguments::Usage(u) => {
            usage(&u.get_program_name());
            return;
        }
        Arguments::Invalid(i) => {
            eprintln!("Error parsing arguments: {}", i.get_error());
            eprintln!("");
            usage(&i.get_program_name());
            return;
        }
        Arguments::Valid(v) => v,
    };

    let config_file_path = format!(
        "{}/{}.yml",
        env.get_config_directory(),
        &args.get_machine_name()
    );
    let config_file = match fs::read_to_string(&config_file_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "Failed to read configuration file `{}`: {}",
                config_file_path, e
            );
            return;
        }
    };
    let config = match config::Config::new(&config_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Configuration load error for `{}` machine: {}",
                args.get_machine_name(),
                e
            );
            return;
        }
    };

    let mut cpuset = match cpuset::CpuSet::new(env.get_cpuset_mount_path(), env.get_cpuset_prefix())
    {
        Ok(cpuset) => cpuset,
        Err(e) => {
            eprintln!("{}", e);
            return;
        }
    };

    if config.rlimit_memlock() {
        if let Err(e) = setrlimit(Resource::MEMLOCK, Rlim::INFINITY, Rlim::INFINITY) {
            eprintln!("{}", e);
            return;
        }
    }

    let mut command = Command::new(config.get_qemu_binary_path());
    command
        .args(config.get_command_line_options())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());

    if config.should_clear_env() {
        command.env_clear();
    }

    if let Some(uid) = config.get_user() {
        command.uid(uid as u32);
    }

    if let Some(gid) = config.get_group() {
        command.gid(gid as u32);
    }

    if config.has_env_vars() {
        command.envs(config.get_env_vars());
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Failed to run the `{}` child process: {}",
                config.get_qemu_binary_path(),
                e
            );
            return;
        }
    };

    if config.has_cpu_pinning() {
        handle_vcpu_pinning(&mut child, &mut cpuset, &config);
    }

    match child.wait() {
        Ok(e) => {
            if !e.success() {
                eprintln!(
                    "The child process `{}` was terminated with non-zero status.",
                    config.get_qemu_binary_path()
                );
            }
        }
        Err(e) => {
            eprintln!(
                "The child process `{}` was terminated preliminarly: {}",
                config.get_qemu_binary_path(),
                e
            );
        }
    }

    if let Err(e) = cpuset.release_threads() {
        eprintln!("Failed to release some pinned CPU threads: {}", e);
    }
}
