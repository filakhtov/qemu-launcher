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
use process::{ChildProcess, Process};
use rlimit::{setrlimit, Resource, Rlim};
use std::{env, fs};

fn usage(name: &str) {
    eprintln!("Usage: {} [-v] [-d] [-h] <vm-name>", name);
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

fn handle_vcpu_pinning(
    child: &mut ChildProcess,
    cpuset: &mut cpuset::CpuSet,
    config: &config::Config,
) {
    let qmp_socket = match child.get_stdio() {
        Ok(io) => io,
        Err(e) => {
            eprintln!("Unable to obtain qemu process stdio descriptors: {}", e);
            return;
        }
    };

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

    let mut child = match Process::new(config.get_qemu_binary_path())
        .set_args(config.get_command_line_options())
        .set_effective_group_id(&config.get_group())
        .set_effective_user_id(&config.get_user())
        .should_clear_env(config.should_clear_env())
        .set_environment_variables(config.get_env_vars())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Failed to execute the `{}` child process: {}",
                config.get_qemu_binary_path(),
                e
            );
            return;
        }
    };

    if config.has_cpu_pinning() {
        handle_vcpu_pinning(&mut child, &mut cpuset, &config);
    }

    if let Err(e) = child.wait() {
        eprintln!(
            "The child process `{}` was terminated preliminarly: {}",
            config.get_qemu_binary_path(),
            e
        );
    }

    if let Err(e) = cpuset.release_threads() {
        eprintln!("Failed to release some pinned CPU threads: {}", e);
    }
}
