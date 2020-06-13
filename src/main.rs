use std::{
    env,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
};

mod config;
mod cpuset;
mod qmp;

fn usage(name: &str) {
    let programname = match Path::new(name).file_name() {
        Some(n) => match n.to_os_string().into_string() {
            Ok(string) => string,
            Err(_) => "qemu-launcher".to_string(),
        },
        None => "qemu-launcher".to_string(),
    };

    eprintln!("Usage: {} <vm-name>", programname);
    eprintln!("");
    eprintln!("Supported environment variables:");
    eprintln!("- QEMU_LAUNCHER_CONFIG_DIR - a path to the directory where virtual machine configuration files are stored.");
    eprintln!("- QEMU_LAUNCHER_CPUSET_MOUNT_PATH - a path to the directory where a cpuset cgroup tree will be mounted.");
    eprintln!("");
}

fn handle_vcpu_pinning(child: &mut Child, cpuset: &mut cpuset::CpuSet, config: &config::Config) {
    let vcpu_info = match qmp::read_vcpu_info(child) {
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

        if config.is_debug_enabled() {
            println!(
                "Successfully pinned the vCPU core `{}.{}.{}` with the task ID `{}` to the host CPU `{}`.",
                pin.0, pin.1, pin.2, task_id, pin.3
            )
        }
    }

    if config.has_scheduling() {
        let scheduler = config.get_scheduler().unwrap();
        let priority = config.get_priority().unwrap().to_string();

        for task_id in vcpu_info.get_task_ids() {
            match Command::new("chrt")
                .arg(format!("--{}", scheduler))
                .arg("--pid")
                .arg(&priority)
                .arg(task_id.to_string())
                .spawn()
            {
                Ok(mut c) => match c.wait() {
                    Ok(r) => {
                        if config.is_debug_enabled() && r.success() {
                            println!(
                                "vCPU thread `{}` priority successfully changed, scheduler: {}, priority: {}",
                                task_id,
                                scheduler,
                                priority
                            );
                        }

                        if !r.success() {
                            eprintln!(
                                "Failed to change vCPU thread `{}` priority: `chrt` call failed",
                                task_id
                            )
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to change vCPU thread `{}` priority: {}", task_id, e)
                    }
                },
                Err(e) => eprintln!("Failed to change vCPU thread `{}` priority: {}", task_id, e),
            }
        }
    }
}

fn main() {
    let config_dir = match env::var_os("QEMU_LAUNCHER_CONFIG_DIR") {
        Some(value) => match value.into_string() {
            Ok(value) => value,
            Err(_) => {
                eprintln!("Failed to parse the `QEMU_LAUNCHER_CONFIG_DIR` environment variable.");
                return;
            }
        },
        None => "/usr/local/etc/qemu-launcher".to_string(),
    };

    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        usage(&args[0]);
        return;
    }

    let machine_name = &args[1];

    let config = match config::Config::new(&format!("{}/{}.yml", config_dir, machine_name)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Configuration load error for `{}` machine: {}",
                machine_name, e
            );
            return;
        }
    };

    let cpuset_mountpoint = match env::var_os("QEMU_LAUNCHER_CPUSET_MOUNT_PATH") {
        Some(value) => {
            match value.into_string() {
                Ok(value) => value,
                Err(_) => {
                    eprintln!("Failed to parse the `QEMU_LAUNCHER_CPUSET_MOUNT_PATH` environment variable.");
                    return;
                }
            }
        }
        None => "/sys/fs/cgroup/cpuset".to_string(),
    };

    let mut cpuset = cpuset::CpuSet::new(&cpuset_mountpoint);

    let mut command = Command::new(config.get_qemu_binary_path());
    command
        .args(config.get_command_line_options())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());

    if config.should_clear_env() {
        command.env_clear();
    }

    if let Some(uid) = config.get_user() {
        command.uid(uid);
    }

    if let Some(gid) = config.get_group() {
        command.gid(gid);
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
        eprintln!("Failed to release pinned CPU threads: {}", e);
    }
}
