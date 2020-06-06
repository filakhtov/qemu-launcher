use proc_mounts::MountIter;
use std::{
    fs,
    io::{BufReader, Error, ErrorKind},
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
        for id in &self.isolated_threads {
            fs::remove_dir(format!("{}/{}", self.mount_path, id))?;
        }

        self.isolated_threads = vec![];
        Ok({})
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
