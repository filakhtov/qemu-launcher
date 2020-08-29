
use nix::fcntl::{flock, FlockArg};
use proc_mounts::MountIter;
use std::{
    fs,
    io::{prelude::*, BufReader, Error, ErrorKind, SeekFrom},
    os::unix::io::AsRawFd,
    process::Command,
};

pub struct CpuSet {
    mount_path: String,
    isolated_threads: Vec<usize>,
    prefix: String,
}

pub enum PinResult {
    Ok,
    Warn(Error),
    Err(Error),
}

impl CpuSet {
    pub fn new(path: &str, prefix: &str) -> Self {
        CpuSet {
            mount_path: String::from(path),
            isolated_threads: vec![],
            prefix: String::from(prefix),
        }
    }

    fn cpuset_path(&self) -> String {
        format!("{}/{}", self.mount_path, self.prefix)
    }

    pub fn pin_task(&mut self, host_id: usize, guest_id: usize) -> PinResult {
        let result = self.isolate_thread(host_id);
        if let PinResult::Err(_) = &result {
            return result;
        }

        if let Err(e) = fs::write(
            format!("{}/{}/tasks", self.cpuset_path(), host_id),
            guest_id.to_string(),
        ) {
            return PinResult::Err(e);
        }

        result
    }

    fn isolate_thread(&mut self, id: usize) -> PinResult {
        if let Err(e) = self.prepare_cpuset() {
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

        if let Err(e) = self.split_thread_from_pool(&id) {
            return PinResult::Err(e);
        }

        let path = format!("{}/{}", self.cpuset_path(), id);
        if let Err(e) = fs::create_dir_all(&path) {
            return PinResult::Err(e);
        }

        let mems = match fs::read_to_string(format!("{}/cpuset.mems", self.cpuset_path())) {
            Ok(mems) => mems.trim().to_owned(),
            Err(e) => return PinResult::Err(e),
        };

        if let Err(e) = fs::write(format!("{}/cpuset.mems", &path), mems) {
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

    fn split_thread_from_pool(&self, id: &usize) -> Result<(), Error> {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/pool/cpuset.cpus", self.cpuset_path()))?;
        let fd = file.as_raw_fd();

        if let Err(e) = flock(fd, FlockArg::LockExclusive) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Failed to lock cpuset.cpus on pool: {}", e),
            ));
        }

        let mut cpus = read_cpus_from_file(&mut file)?;
        cpus.retain(|cpu| cpu != &id.to_string());
        write_cpus_to_file(&mut file, cpus)?;

        Ok({})
    }

    fn return_thread_to_pool(&self, id: &usize) -> Result<(), Error> {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/pool/cpuset.cpus", self.cpuset_path()))?;
        let fd = file.as_raw_fd();

        if let Err(e) = flock(fd, FlockArg::LockExclusive) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Failed to lock cpuset.cpus on pool: {}", e),
            ));
        }

        let mut cpus = read_cpus_from_file(&mut file)?;
        cpus.push(id.to_string());
        write_cpus_to_file(&mut file, cpus)?;

        Ok({})
    }

    fn prepare_cpuset(&self) -> Result<(), Error> {
        self.ensure_mounted()?;
        self.configure_cpuset()?;
        self.migrate_tasks()?;
        Ok({})
    }

    fn migrate_tasks(&self) -> Result<(), Error> {
        let file = fs::File::open(format!("{}/tasks", self.mount_path))?;
        let reader = BufReader::new(file);
        let path = format!("{}/pool/tasks", self.cpuset_path());
        for task in reader.lines() {
            if let Ok(task) = task {
                let _ = fs::write(&path, task);
            }
        }
        Ok({})
    }

    fn configure_cpuset(&self) -> Result<(), Error> {
        let path = self.cpuset_path();
        fs::create_dir_all(&path)?;
        fs::write(format!("{}/cpuset.cpu_exclusive", path), "1")?;

        let mems_path = format!("{}/cpuset.mems", path);
        let mut mems = fs::read_to_string(&mems_path)?.trim().to_owned();
        if mems.len() == 0 {
            mems = fs::read_to_string(format!("{}/cpuset.mems", self.mount_path))?
                .trim()
                .to_owned();
            fs::write(&mems_path, &mems)?;
        }

        let cpus_path = format!("{}/cpuset.cpus", path);
        let mut cpus = fs::read_to_string(&cpus_path)?.trim().to_owned();
        if cpus.len() == 0 {
            cpus = fs::read_to_string(format!("{}/cpuset.cpus", self.mount_path))?
                .trim()
                .to_owned();
            fs::write(&cpus_path, &cpus)?;
        }

        let path = format!("{}/{}", self.cpuset_path(), "pool");
        fs::create_dir_all(&path)?;
        fs::write(format!("{}/cpuset.cpu_exclusive", path), "1")?;

        let mems_path = format!("{}/cpuset.mems", path);
        if fs::read_to_string(&mems_path)?.trim().len() == 0 {
            fs::write(&mems_path, mems)?;
        }

        let cpus_path = format!("{}/cpuset.cpus", path);
        if fs::read_to_string(&cpus_path)?.trim().len() == 0 {
            fs::write(&cpus_path, cpus)?;
        }

        Ok({})
    }

    pub fn release_threads(&mut self) -> Result<(), Error> {
        let mut errors = vec![];

        for id in &self.isolated_threads {
            match self.is_thread_free(id) {
                Ok(None) => match fs::remove_dir(format!("{}/{}", self.cpuset_path(), id)) {
                    Ok(_) => match self.return_thread_to_pool(id) {
                        Ok(_) => {}
                        Err(e) => errors.push(format!(
                            "unable to return the host thread {} to the pool: {}",
                            id, e
                        )),
                    },
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
        let tasks_file = fs::File::open(format!("{}/{}/tasks", self.cpuset_path(), id))?;
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

fn parse_cpus_list(spec: &str) -> Vec<usize> {
    if spec.len() == 0 {
        return vec![];
    }

    let mut threads = vec![];
    for group in spec.split(',') {
        let cores: Vec<&str> = group.split('-').collect();
        match cores.len() {
            1 => threads.push(cores[0].parse::<usize>().unwrap()),
            2 => threads
                .extend(cores[0].parse::<usize>().unwrap()..cores[1].parse::<usize>().unwrap() + 1),
            _ => panic!("Malformed cpu core specification: {}", spec),
        }
    }

    threads
}

fn read_cpus_from_file(file: &mut fs::File) -> Result<Vec<String>, Error> {
    let mut cpus = String::new();
    file.read_to_string(&mut cpus)?;

    Ok(parse_cpus_list(cpus.trim())
        .iter()
        .map(|cpu| cpu.to_string())
        .collect())
}

fn write_cpus_to_file(file: &mut fs::File, cpus: Vec<String>) -> Result<(), Error> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write(cpus.join(",").as_bytes())?;

    Ok({})
}
