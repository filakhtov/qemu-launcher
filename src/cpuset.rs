use proc_mounts::MountIter;
use std::{
    fs,
    io::{prelude::*, BufReader, Error, ErrorKind},
    process::Command,
};

pub struct CpuSet {
    mount_path: String,
    isolated_threads: Vec<usize>,
}

pub enum PinResult {
    Ok,
    Warn(Error),
    Err(Error),
}

impl CpuSet {
    pub fn new(path: &str) -> Self {
        CpuSet {
            mount_path: String::from(path),
            isolated_threads: vec![],
        }
    }

    pub fn pin_task(&mut self, host_id: usize, guest_id: usize) -> PinResult {
        let result = self.isolate_thread(host_id);

        if let Err(e) = fs::write(
            format!("{}/{}/tasks", self.mount_path, host_id),
            guest_id.to_string(),
        ) {
            return PinResult::Err(e);
        }

        result
    }

    fn isolate_thread(&mut self, id: usize) -> PinResult {
        if let Err(e) = self.ensure_mounted() {
            return PinResult::Err(e);
        }

        let result = match self.is_thread_free(&id) {
            Ok(None) => PinResult::Ok,
            Ok(Some(pid)) => PinResult::Warn(Error::new(
                ErrorKind::Other,
                format!(
                    "cpuset `{}` already has at least one task pinned ({})",
                    id, pid
                ),
            )),
            Err(e) => match e.kind() {
                ErrorKind::NotFound => PinResult::Ok,
                _ => return PinResult::Err(e),
            },
        };

        if self.isolated_threads.contains(&id) {
            return result;
        }

        let path = format!("{}/{}", self.mount_path, id);
        if let Err(e) = fs::create_dir_all(&path) {
            return PinResult::Err(e);
        }

        if let Err(e) = fs::write(format!("{}/cpuset.mems", &path), "0") {
            return PinResult::Err(e);
        }

        if let Err(e) = fs::write(format!("{}/cpuset.cpu_exclusive", &path), "1") {
            return PinResult::Err(e);
        }

        if let Err(e) = fs::write(format!("{}/cpuset.cpus", &path), id.to_string()) {
            return PinResult::Err(e);
        }

        self.isolated_threads.push(id);

        result
    }

    pub fn release_threads(&mut self) -> Result<(), Error> {
        let mut errors = vec![];

        for id in &self.isolated_threads {
            match self.is_thread_free(id) {
                Ok(None) => match fs::remove_dir(format!("{}/{}", self.mount_path, id)) {
                    Ok(_) => {}
                    Err(e) => errors.push(format!("thread {}: {}", id, e)),
                },
                Ok(Some(task)) => errors.push(format!(
                    "thread {}: still busy with at least one task ({})",
                    id, task
                )),
                Err(e) => errors.push(format!("thread {}: status unknown, {}", id, e)),
            }
        }

        self.isolated_threads = vec![];

        if errors.len() > 0 {
            return Err(Error::new(ErrorKind::Other, errors.join("; ")));
        }

        Ok({})
    }

    fn is_thread_free(&self, id: &usize) -> Result<Option<String>, Error> {
        let tasks_file = fs::File::open(format!("{}/{}/tasks", self.mount_path, id))?;
        let mut tasks_reader = BufReader::new(tasks_file);
        let mut task = String::new();
        tasks_reader.read_line(&mut task)?;

        task = task.trim().to_owned();
        if task.len() > 0 {
            return Ok(Some(task));
        }

        Ok(None)
    }

    fn ensure_mounted(&self) -> Result<(), Error> {
        fs::create_dir_all(&self.mount_path)?;

        match MountIter::<BufReader<fs::File>>::source_mounted_at("cgroup", &self.mount_path) {
            Ok(true) => {}
            Ok(false) => self.mount_cpuset()?,
            Err(e) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("An error occurred while reading mounts: {}", e),
                ))
            }
        }

        Ok({})
    }

    fn mount_cpuset(&self) -> Result<(), Error> {
        let exit_status = Command::new("mount")
            .arg("-t")
            .arg("cgroup")
            .arg("-o")
            .arg("cpuset")
            .arg("cgroup")
            .arg(&self.mount_path)
            .spawn()?
            .wait()?;

        if !exit_status.success() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to mount cpuset to `{}`, mount command exited with non-zero status",
                    self.mount_path
                ),
            ));
        }
        Ok({})
    }
}
