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

impl CpuSet {
    pub fn new(path: &str) -> Self {
        CpuSet {
            mount_path: String::from(path),
            isolated_threads: vec![],
        }
    }

    pub fn pin_task(&mut self, host_id: usize, guest_id: usize) -> Result<(), Error> {
        self.isolate_thread(host_id)?;
        fs::write(
            format!("{}/{}/tasks", self.mount_path, host_id),
            guest_id.to_string(),
        )?;
        Ok({})
    }

    fn isolate_thread(&mut self, id: usize) -> Result<(), Error> {
        self.ensure_mounted()?;

        let path = format!("{}/{}", self.mount_path, id);
        fs::create_dir_all(&path)?;
        fs::write(format!("{}/cpuset.mems", &path), "0")?;
        fs::write(format!("{}/cpuset.cpu_exclusive", &path), "1")?;
        fs::write(format!("{}/cpuset.cpus", &path), id.to_string())?;
        self.isolated_threads.push(id);
        Ok({})
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
                    id,
                    task.trim()
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
