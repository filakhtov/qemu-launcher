use std::{
    io::{Error, ErrorKind},
    path::{Path, PathBuf},
};
#[cfg(not(test))]
use {
    nix::{
        fcntl::{flock, FlockArg},
        mount::{mount, MsFlags},
    },
    proc_mounts::MountIter,
    std::{
        fs,
        io::{prelude::*, BufReader, SeekFrom},
        os::unix::io::AsRawFd,
    },
};

macro_rules! path {
    ($path:expr) => (PathBuf::from(&$path));
    ($path:expr, $($part:expr), +) => {{
        let mut path = PathBuf::from(&$path);
        path.push(path!($($part),+));
        path
    }}
}

pub struct CpuSet {
    mount_path: PathBuf,
    isolated_threads: Vec<usize>,
    prefix: PathBuf,
}

impl CpuSet {
    pub fn new<D: AsRef<Path>, P: AsRef<Path>>(path: D, prefix: P) -> Result<Self, Error> {
        if !path.as_ref().has_root() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "A mount point path must be absolute, got: `{}`.",
                    path.as_ref().display()
                ),
            ));
        }

        if prefix.as_ref().iter().count() != 1 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "A mount point prefix can not contain path separators, got: `{}`.",
                    prefix.as_ref().display()
                ),
            ));
        }

        Ok(CpuSet {
            mount_path: PathBuf::from(path.as_ref()),
            isolated_threads: vec![],
            prefix: PathBuf::from(prefix.as_ref()),
        })
    }

    #[inline]
    fn cpuset_path(&self) -> PathBuf {
        path!(self.mount_path, self.prefix)
    }

    pub fn pin_task(&mut self, host_id: usize, guest_id: usize) -> Result<(), Error> {
        if let Err(e) = self.isolate_thread(host_id) {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to isolate the host cpu thread `{}` - {}",
                    host_id, e
                ),
            ));
        }

        if let Err(e) = fs_write(
            path!(self.cpuset_path(), host_id.to_string(), "tasks"),
            guest_id.to_string(),
        ) {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to pin the process id `{}` to the host cpu thread `{}` - {}",
                    guest_id, host_id, e
                ),
            ));
        }

        Ok({})
    }

    fn isolate_thread(&mut self, id: usize) -> Result<(), Error> {
        self.prepare_cpuset()?;

        // TODO: issue a warning if thread already busy
        if let Err(e) = self.is_thread_free(&id) {
            match e.kind() {
                ErrorKind::NotFound => {}
                _ => return Err(e),
            }
        };

        if self.isolated_threads.contains(&id) {
            return Ok({});
        }

        self.split_thread_from_pool(&id)?;

        let path = path!(self.cpuset_path(), id.to_string());
        fs_create_dir_all(&path)?;

        let mems = fs_read_to_string(path!(self.cpuset_path(), "cpuset.mems"))?;
        fs_write(path!(path, "cpuset.mems"), mems)?;
        fs_write(path!(path, "cpuset.cpu_exclusive"), "1")?;
        fs_write(path!(path, "cpuset.cpus"), id.to_string())?;

        self.isolated_threads.push(id);

        Ok({})
    }

    fn split_thread_from_pool(&self, id: &usize) -> Result<(), Error> {
        let mut file = File::open(path!(self.cpuset_path(), "pool", "cpuset.cpus"))?;
        file.lock()?;

        let mut cpus = read_cpus_from_file(&mut file)?;
        cpus.retain(|cpu| cpu != &id.to_string());
        write_cpus_to_file(&mut file, cpus)?;

        Ok({})
    }

    fn return_thread_to_pool(&self, id: &usize) -> Result<(), Error> {
        let mut file = File::open(path!(self.cpuset_path(), "pool", "cpuset.cpus"))?;
        file.lock()?;

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
        let pool_cpus_path = path!(self.cpuset_path(), "pool", "cpuset.cpus");
        let pool_cpus = parse_cpus_list(fs_read_to_string(pool_cpus_path)?.trim());

        let file = fs_read_to_string(path!(self.mount_path, "tasks"))?;
        let path = path!(self.cpuset_path(), "pool", "tasks");
        for task in file.lines() {
            let task_cpus = match get_task_cpus(task) {
                Ok(cpus) => cpus,
                Err(_) => {
                    // TODO: issue warning
                    continue;
                }
            };

            if pool_cpus == task_cpus {
                match fs_write(&path, task) {
                    Ok(_) => {}
                    Err(_) => {
                        // TODO: issue warning
                    }
                }
            }
        }
        Ok({})
    }

    fn configure_cpuset(&self) -> Result<(), Error> {
        let path = self.cpuset_path();
        fs_create_dir_all(&path)?;
        fs_write(path!(path, "cpuset.cpu_exclusive"), "1")?;

        let mems_path = path!(path, "cpuset.mems");
        let mut mems = fs_read_to_string(&mems_path)?.trim().to_owned();
        if mems.len() == 0 {
            mems = fs_read_to_string(path!(self.mount_path, "cpuset.mems"))?
                .trim()
                .to_owned();
            fs_write(&mems_path, &mems)?;
        }

        let cpus_path = path!(path, "cpuset.cpus");
        let mut cpus = fs_read_to_string(&cpus_path)?.trim().to_owned();
        if cpus.len() == 0 {
            cpus = fs_read_to_string(path!(self.mount_path, "cpuset.cpus"))?
                .trim()
                .to_owned();
            fs_write(&cpus_path, &cpus)?;
        }

        let path = path!(self.cpuset_path(), "pool");
        fs_create_dir_all(&path)?;
        fs_write(path!(path, "cpuset.cpu_exclusive"), "1")?;

        let mems_path = path!(path, "cpuset.mems");
        if fs_read_to_string(&mems_path)?.trim().len() == 0 {
            fs_write(&mems_path, mems)?;
        }

        let cpus_path = path!(path, "cpuset.cpus");
        if fs_read_to_string(&cpus_path)?.trim().len() == 0 {
            fs_write(&cpus_path, cpus)?;
        }

        Ok({})
    }

    pub fn release_threads(&mut self) -> Result<(), Error> {
        let mut errors = false;

        for id in &self.isolated_threads {
            match self.is_thread_free(id) {
                Ok(None) => match fs_remove_dir(path!(self.cpuset_path(), id.to_string())) {
                    Ok(_) => match self.return_thread_to_pool(id) {
                        Ok(_) => {}
                        Err(_) => errors = true, // TODO: emit warning
                    },
                    Err(_) => errors = true, // TODO: emit warning
                },
                Ok(Some(_)) => errors = true, // TODO: emit warning
                Err(_) => errors = true,      // TODO: emit warning
            }
        }

        self.isolated_threads = vec![];

        if errors {
            return Err(Error::new(
                ErrorKind::Other,
                "Failed to release some of the pinned threads.",
            ));
        }

        Ok({})
    }

    fn is_thread_free(&self, id: &usize) -> Result<Option<String>, Error> {
        let task = fs_read_line(path!(self.cpuset_path(), id.to_string(), "tasks"))?
            .trim()
            .to_owned();

        if task.len() > 0 {
            return Ok(Some(task));
        }

        Ok(None)
    }

    fn ensure_mounted(&self) -> Result<(), Error> {
        fs_create_dir_all(&self.mount_path)?;

        match source_mounted_at("cgroup", &self.mount_path) {
            Ok(true) => Ok({}),
            Ok(false) => self.mount_cpuset(),
            Err(e) => Err(Error::new(
                ErrorKind::Other,
                format!("An error occurred while reading mounts: {}", e),
            )),
        }
    }

    fn mount_cpuset(&self) -> Result<(), Error> {
        if let Err(e) = fs_mount(&self.mount_path) {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to mount cpuset to `{}`: {}",
                    self.mount_path.display(),
                    e
                ),
            ));
        }

        Ok({})
    }
}

fn parse_cpus_list<S: AsRef<str>>(spec: S) -> Vec<usize> {
    let spec = spec.as_ref();

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

fn read_cpus_from_file(file: &mut File) -> Result<Vec<String>, Error> {
    let cpus = file.read_string()?;

    Ok(parse_cpus_list(cpus.trim())
        .iter()
        .map(|cpu| cpu.to_string())
        .collect())
}

fn write_cpus_to_file(file: &mut File, cpus: Vec<String>) -> Result<(), Error> {
    file.trim()?;
    file.write(cpus.join(",").as_bytes())?;

    Ok({})
}

fn get_task_cpus(task: &str) -> Result<Vec<usize>, Error> {
    let task_status = fs_read_to_string(path!("/proc", task, "status"))?;
    match get_cpus_from_task_status(task_status) {
        Ok(cpus) => Ok(parse_cpus_list(&cpus)),
        Err(e) => {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Task ID {}: {}", task, e),
            ))
        }
    }
}

fn get_cpus_from_task_status<S: AsRef<str>>(status: S) -> Result<String, Error> {
    for line in status.as_ref().lines() {
        let fields = line.split(":\t").collect::<Vec<&str>>();
        if fields.len() != 2 {
            return Err(Error::new(
                ErrorKind::Other,
                format!("malformed process status field: {}", line),
            ));
        }

        if fields[0].trim() == "Cpus_allowed_list" {
            return Ok(fields[1].to_string());
        }
    }

    Err(Error::new(
        ErrorKind::Other,
        "process status does not contain the `Cpus_allowed_list` field.",
    ))
}

#[cfg(not(test))]
#[inline]
fn fs_write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, data: C) -> Result<(), Error> {
    fs::write(path, data)
}

#[cfg(test)]
fn fs_write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, data: C) -> Result<(), Error> {
    test::fs_write(path, data)
}

#[cfg(not(test))]
#[inline]
fn fs_create_dir_all<P: AsRef<Path>>(path: P) -> Result<(), Error> {
    fs::create_dir_all(path)
}

#[cfg(test)]
fn fs_create_dir_all<P: AsRef<Path>>(path: P) -> Result<(), Error> {
    test::fs_create_dir_all(path)
}

#[cfg(not(test))]
#[inline]
fn fs_read_to_string<P: AsRef<Path>>(path: P) -> Result<String, Error> {
    fs::read_to_string(path)
}

#[cfg(test)]
fn fs_read_to_string<P: AsRef<Path>>(path: P) -> Result<String, Error> {
    test::fs_read_to_string(path)
}

#[cfg(not(test))]
#[inline]
fn fs_remove_dir<P: AsRef<Path>>(path: P) -> Result<(), Error> {
    fs::remove_dir(path)
}

#[cfg(test)]
fn fs_remove_dir<P: AsRef<Path>>(path: P) -> Result<(), Error> {
    test::fs_remove_dir(path)
}

#[cfg(not(test))]
#[inline]
fn source_mounted_at<S: AsRef<Path>, P: AsRef<Path>>(source: S, path: P) -> Result<bool, Error> {
    MountIter::<BufReader<fs::File>>::source_mounted_at(source, path)
}

#[cfg(test)]
fn source_mounted_at<S: AsRef<Path>, P: AsRef<Path>>(source: S, path: P) -> Result<bool, Error> {
    test::source_mounted_at(source, path)
}

#[cfg(not(test))]
#[inline]
fn fs_read_line<P: AsRef<Path>>(path: P) -> Result<String, Error> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut data = String::new();
    reader.read_line(&mut data)?;

    Ok(data)
}

#[cfg(test)]
fn fs_read_line<P: AsRef<Path>>(path: P) -> Result<String, Error> {
    test::fs_read_line(path)
}

#[cfg(not(test))]
#[inline]
fn fs_mount<P1: AsRef<Path>>(target: P1) -> Result<(), nix::Error> {
    mount(
        Some("cgroup"),
        target.as_ref(),
        Some("cgroup"),
        MsFlags::empty(),
        Some("cpuset"),
    )
}

#[cfg(test)]
fn fs_mount<P1: AsRef<Path>>(target: P1) -> Result<(), nix::Error> {
    test::fs_mount(target)
}

struct File {
    #[cfg(test)]
    inner: test::File,
    #[cfg(not(test))]
    inner: fs::File,
}

impl File {
    #[cfg(not(test))]
    fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Ok(Self {
            inner: fs::OpenOptions::new().read(true).write(true).open(path)?,
        })
    }

    #[cfg(test)]
    fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        match test::File::open(path) {
            Ok(f) => Ok(Self { inner: f }),
            Err(e) => Err(e),
        }
    }

    #[cfg(not(test))]
    fn lock(&mut self) -> Result<(), Error> {
        if let Err(e) = flock(self.inner.as_raw_fd(), FlockArg::LockExclusive) {
            return Err(Error::new(ErrorKind::Other, e));
        }

        Ok({})
    }

    #[cfg(test)]
    fn lock(&mut self) -> Result<(), Error> {
        self.inner.lock()
    }

    #[cfg(not(test))]
    fn trim(&mut self) -> Result<(), Error> {
        self.inner.seek(SeekFrom::Start(0))?;
        self.inner.set_len(0)
    }

    #[cfg(test)]
    fn trim(&mut self) -> Result<(), Error> {
        self.inner.trim()
    }

    #[cfg(not(test))]
    fn read_string(&mut self) -> Result<String, Error> {
        let mut data = String::new();
        self.inner.read_to_string(&mut data)?;

        Ok(data)
    }

    #[cfg(test)]
    fn read_string(&mut self) -> Result<String, Error> {
        self.inner.read_string()
    }

    #[cfg(not(test))]
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.inner.write(buf)
    }

    #[cfg(test)]
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.inner.write(buf)
    }
}

#[cfg(test)]
mod test {
    use super::CpuSet;
    use crate::assert_error;
    use std::{
        cell::RefCell,
        collections::VecDeque,
        io::{Error, ErrorKind},
        path::Path,
        str::from_utf8,
    };

    macro_rules! vec_deq {
        [] => {{
            VecDeque::new()
        }};
        [ $( $item:expr ),* $(,)? ] => {{
            let mut v = VecDeque::new();
            $( v.push_back($item); )*
            v
        }};
    }

    macro_rules! error {
        ($msg:expr) => {{
            Err(Error::new(ErrorKind::Other, format!("{}", $msg)))
        }};
    }

    struct TestExpectations {
        fs_write: VecDeque<((String, String), Result<(), Error>)>,
        fs_read_to_string: VecDeque<(String, Result<String, Error>)>,
        fs_create_dir_all: VecDeque<(String, Result<(), Error>)>,
        source_mounted_at: VecDeque<(String, Result<bool, Error>)>,
        fs_mount: VecDeque<(String, Result<(), nix::Error>)>,
        fs_read_line: VecDeque<(String, Result<String, Error>)>,
        file_open: VecDeque<(String, Result<File, Error>)>,
        fs_remove_dir: VecDeque<(String, Result<(), Error>)>,
    }

    impl TestExpectations {
        fn new() -> Self {
            TestExpectations {
                fs_write: vec_deq![],
                fs_read_to_string: vec_deq![],
                fs_create_dir_all: vec_deq![],
                source_mounted_at: vec_deq![],
                fs_mount: vec_deq![],
                fs_read_line: vec_deq![],
                file_open: vec_deq![],
                fs_remove_dir: vec_deq![],
            }
        }
    }

    thread_local! { static TEST_EXPECTATIONS: RefCell<TestExpectations> = RefCell::new(TestExpectations::new()) }

    macro_rules! expect {
        (fs_write: $( { $path:expr, $data:expr => $result:expr } ),* $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().fs_write
                    .push_back(((String::from($path), String::from($data)), $result)); )*
            });
        }};
        (fs_read_to_string: $( { $path: expr => $result:expr } ),* $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().fs_read_to_string.push_back((String::from($path), $result)); )*
            });
        }};
        (fs_create_dir_all: $( { $path:expr => $result:expr } ),* $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().fs_create_dir_all.push_back((String::from($path), $result)); )*
            });
        }};
        (source_mounted_at: $( { $path: expr => $result:expr } ), *$(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().source_mounted_at.push_back((String::from($path), $result)); )*
            });
        }};
        (fs_mount: $( { $path:expr => $result:expr } ),* $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().fs_mount.push_back((String::from($path), $result)); )*
            });
        }};
        (fs_read_line: $( { $path:expr => $result:expr } ),* $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().fs_read_line.push_back((String::from($path), $result)); )*
            });
        }};
        (File::open: $( { $path:expr => $result:expr } ),* $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().file_open.push_back((String::from($path), $result)); )*
            });
        }};
        (fs_remove_dir: $( { $path:expr => $result:expr } ),* $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().fs_remove_dir.push_back((String::from($path), $result)); )*
            });
        }};
    }

    macro_rules! verify_expectations {
        () => {
            TEST_EXPECTATIONS.with(|expectations| {
                let expectations = expectations.borrow();
                let len = expectations.fs_write.len();
                if len > 0 {
                    panic!("{} more fs_write() call(s) expected.", len);
                }

                let len = expectations.fs_read_to_string.len();
                if len > 0 {
                    panic!("{} more fs_read_to_string() call(s) expected.", len);
                }

                let len = expectations.fs_create_dir_all.len();
                if len > 0 {
                    panic!("{} more fs_create_dir_all() call(s) expected.", len);
                }

                let len = expectations.fs_mount.len();
                if len > 0 {
                    panic!("{} more fs_mount() call(s) expected.", len);
                }

                let len = expectations.fs_read_line.len();
                if len > 0 {
                    panic!("{} more fs_read_line() call(s) expected.", len);
                }

                let len = expectations.fs_remove_dir.len();
                if len > 0 {
                    panic!("{} more fs_remove_dir() call(s) expected.", len);
                }

                let len = expectations.source_mounted_at.len();
                if len > 0 {
                    panic!("{} more source_mounted_at() call(s) expected.", len);
                }

                let len = expectations.file_open.len();
                if len > 0 {
                    panic!("{} more file_open() call(s) expected.", len);
                }
            })
        };
    }

    pub struct File {
        lock: VecDeque<Result<(), Error>>,
        trim: VecDeque<Result<(), Error>>,
        read_string: VecDeque<Result<String, Error>>,
        write: VecDeque<(String, Option<Error>)>,
    }

    impl File {
        pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
            let path = path.as_ref().to_str().unwrap();

            let (argument, result) = match TEST_EXPECTATIONS
                .with(|expectations| expectations.borrow_mut().file_open.pop_front())
            {
                Some(arg) => arg,
                None => panic!("Unexpected call to File::open({})", path),
            };

            if argument != path.to_string() {
                panic!(
                    "Unexpected call to File::open({}), expected: File::open({})",
                    path, argument
                );
            }

            result
        }

        pub fn lock(&mut self) -> Result<(), Error> {
            match self.lock.pop_front() {
                Some(r) => r,
                None => panic!("Unexpected call to File::lock()"),
            }
        }

        pub fn trim(&mut self) -> Result<(), Error> {
            match self.trim.pop_front() {
                Some(r) => r,
                None => panic!("Unexpected call to File::trim()"),
            }
        }

        pub fn read_string(&mut self) -> Result<String, Error> {
            match self.read_string.pop_front() {
                Some(r) => r,
                None => panic!("Unexpected call to File::read_string()"),
            }
        }

        pub fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
            let buf = from_utf8(buf.as_ref()).unwrap();

            let (argument, result) = match self.write.pop_front() {
                Some(arg) => arg,
                None => panic!("Unexpected call to File::write({})", buf),
            };

            if argument != buf.to_string() {
                panic!(
                    "Unexpected call to File::write({}), expected: File::write({})",
                    buf, argument
                );
            }

            match result {
                Some(e) => Err(e),
                None => Ok(buf.len()),
            }
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            let len = self.lock.len();
            if len > 0 {
                panic!("{} more call(s) expected for File::lock()", len)
            }

            let len = self.trim.len();
            if len > 0 {
                panic!("{} more call(s) expected for File::trim()", len)
            }

            let len = self.read_string.len();
            if len > 0 {
                panic!("{} more call(s) expected for File::read_string()", len)
            }

            let len = self.write.len();
            if len > 0 {
                panic!("{} more call(s) expected for File::write()", len)
            }
        }
    }

    pub fn fs_mount<P1: AsRef<Path>>(target: P1) -> Result<(), nix::Error> {
        let path = target.as_ref().to_str().unwrap();

        let (argument, result) = match TEST_EXPECTATIONS
            .with(|expectations| expectations.borrow_mut().fs_mount.pop_front())
        {
            Some(arg) => arg,
            None => panic!("Unexpected call to fs_mount({})", path),
        };

        if argument != path.to_string() {
            panic!(
                "Unexpected call to fs_mount({}), expected: fs_mount({})",
                path, argument
            );
        }

        result
    }

    pub fn fs_read_line<P: AsRef<Path>>(path: P) -> Result<String, Error> {
        let path = path.as_ref().to_str().unwrap();

        let (argument, result) = match TEST_EXPECTATIONS
            .with(|expectations| expectations.borrow_mut().fs_read_line.pop_front())
        {
            Some(arg) => arg,
            None => panic!("Unexpected call to fs_read_line({})", path),
        };

        if argument != path.to_string() {
            panic!(
                "Unexpected call to fs_read_line({}), expected: fs_read_line({})",
                path, argument
            );
        }

        result
    }

    pub fn source_mounted_at<S: AsRef<Path>, P: AsRef<Path>>(
        source: S,
        path: P,
    ) -> Result<bool, Error> {
        if "cgroup" != source.as_ref().to_str().unwrap() {
            panic!("Unexpected call to source_mounted_at(): source must be `cgroup`.");
        }

        let path = path.as_ref().to_str().unwrap();
        let (argument, result) = match TEST_EXPECTATIONS
            .with(|expectations| expectations.borrow_mut().source_mounted_at.pop_front())
        {
            Some(arg) => arg,
            None => panic!("Unexpected call to source_mounted_at({})", path),
        };

        if argument != path.to_string() {
            panic!(
                "Unexpected call to source_mounted_at({}), expected: source_mounted_at({})",
                path, argument
            );
        }

        result
    }

    pub fn fs_remove_dir<P: AsRef<Path>>(path: P) -> Result<(), Error> {
        let path = path.as_ref().to_str().unwrap();

        let (argument, result) = match TEST_EXPECTATIONS
            .with(|expectations| expectations.borrow_mut().fs_remove_dir.pop_front())
        {
            Some(arg) => arg,
            None => panic!("Unexpected call to fs_remove_dir({})", path),
        };

        if argument != path.to_string() {
            panic!(
                "Unexpected call to fs_remove_dir({}), expected: fs_remove_dir({})",
                path, argument
            );
        }

        result
    }

    pub fn fs_read_to_string<P: AsRef<Path>>(path: P) -> Result<String, Error> {
        let path = path.as_ref().to_str().unwrap();

        let (argument, result) = match TEST_EXPECTATIONS
            .with(|expectations| expectations.borrow_mut().fs_read_to_string.pop_front())
        {
            Some(arg) => arg,
            None => panic!("Unexpected call to fs_read_to_string({})", path),
        };

        if argument != path.to_string() {
            panic!(
                "Unexpected call to fs_read_to_string({}), expected: fs_read_to_string({})",
                path, argument
            );
        }

        result
    }

    pub fn fs_create_dir_all<P: AsRef<Path>>(path: P) -> Result<(), Error> {
        let path = path.as_ref().to_str().unwrap();

        let (argument, result) = match TEST_EXPECTATIONS
            .with(|expectations| expectations.borrow_mut().fs_create_dir_all.pop_front())
        {
            Some(arg) => arg,
            None => panic!("Unexpected call to fs_create_dir_all({})", path),
        };

        if argument != path.to_string() {
            panic!(
                "Unexpected call to fs_create_dir_all({}), expected: fs_create_dir_all({})",
                path, argument
            );
        }

        result
    }

    pub fn fs_write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, data: C) -> Result<(), Error> {
        let path = path.as_ref().to_str().unwrap();
        let data = from_utf8(data.as_ref()).unwrap();

        let (arguments, result) = match TEST_EXPECTATIONS
            .with(|expectations| expectations.borrow_mut().fs_write.pop_front())
        {
            Some(args) => args,
            None => panic!("Unexpected call to fs_write({}, {})", path, data),
        };

        if arguments != (path.to_string(), data.to_string()) {
            panic!(
                "Unexpected call to fs_write({}, {}), expected: fs_write({}, {})",
                path, data, arguments.0, arguments.1
            );
        }

        result
    }

    #[test]
    fn parse_cpus_list_handles_single_core_specification() {
        assert_eq!(vec![1], super::parse_cpus_list("1"));
    }

    #[test]
    fn parse_cpus_list_handles_range_core_specification() {
        assert_eq!(vec![2, 3, 4, 5], super::parse_cpus_list("2-5"));
    }

    #[test]
    fn parse_cpus_list_handles_multiple_single_core_specifications() {
        assert_eq!(vec![0, 2], super::parse_cpus_list("0,2"));
    }

    #[test]
    fn parse_cpus_list_handles_multiple_range_core_specifications() {
        assert_eq!(
            vec![0, 1, 2, 3, 6, 7, 8, 9],
            super::parse_cpus_list("0-3,6-9")
        );
    }

    #[test]
    fn parse_cpus_list_handles_mixed_specification() {
        assert_eq!(
            vec![0, 1, 2, 3, 5, 7, 8, 9, 11],
            super::parse_cpus_list("0-3,5,7-9,11")
        );
    }

    #[test]
    #[should_panic]
    fn parse_cpus_list_panics_if_core_id_is_malformed() {
        super::parse_cpus_list("h");
    }

    #[test]
    #[should_panic]
    fn parse_cpus_list_panics_if_core_id_in_range_is_malformed() {
        super::parse_cpus_list("0-x");
    }

    #[test]
    #[should_panic]
    fn parse_cpus_list_panics_if_core_id_missing_from_range() {
        super::parse_cpus_list("-1");
    }

    #[test]
    #[should_panic]
    fn parse_cpus_list_panics_if_range_is_malformed() {
        super::parse_cpus_list("0-1-2");
    }

    #[test]
    fn parse_cpus_list_handles_empty_specification() {
        assert_eq!(Vec::<usize>::new(), super::parse_cpus_list(""));
    }

    #[test]
    fn cpuset_instantiation_fails_if_mount_path_is_not_absolute() {
        assert_error!(
            ErrorKind::InvalidInput,
            "A mount point path must be absolute, got: `not/an/absolute/path`.",
            CpuSet::new("not/an/absolute/path", "prefix")
        );
    }

    #[test]
    fn cpuset_instantiation_fails_if_prefix_contains_path_separators() {
        assert_error!(
            ErrorKind::InvalidInput,
            "A mount point prefix can not contain path separators, got: `prefix/with/separators`.",
            CpuSet::new("/absolute/path", "prefix/with/separators")
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_destination_mounting_directory() {
        let mut cpuset = CpuSet::new("/test1/cgroups/cpuset", "prefix1").unwrap();

        expect!(fs_create_dir_all: { "/test1/cgroups/cpuset" => error!("fs_create_dir_all(1)") });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `1` - fs_create_dir_all(1)",
            cpuset.pin_task(1, 32001)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_check_cpuset_cgroug_mount_status() {
        let mut cpuset = CpuSet::new("/test2/cgroups/cpuset", "prefix2").unwrap();

        expect!(fs_create_dir_all: { "/test2/cgroups/cpuset" => Ok({}) });
        expect!(source_mounted_at: { "/test2/cgroups/cpuset" => error!("source_mounted_at(2)") });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `2` - An error \
            occurred while reading mounts: source_mounted_at(2)",
            cpuset.pin_task(2, 32002)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_cpuset_cgroup_mount_fails() {
        let mut cpuset = CpuSet::new("/test3/cgroups/cpuset", "prefix3").unwrap();

        expect!(fs_create_dir_all: { "/test3/cgroups/cpuset" => Ok({}) });
        expect!(source_mounted_at: { "/test3/cgroups/cpuset" => Ok(false) });
        expect!(fs_mount: { "/test3/cgroups/cpuset" => Err(nix::Error::InvalidPath) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `3` - Failed to \
            mount cpuset to `/test3/cgroups/cpuset`: Invalid path",
            cpuset.pin_task(3, 32003)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_cpuset_prefix_directory() {
        let mut cpuset = CpuSet::new("/test4/cgroups/cpuset", "prefix4").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test4/cgroups/cpuset" => Ok({}) },
            { "/test4/cgroups/cpuset/prefix4" => error!("fs_create_dir_all(4)") },
        );
        expect!(source_mounted_at: { "/test4/cgroups/cpuset" => Ok(false) });
        expect!(fs_mount: { "/test4/cgroups/cpuset" => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `4` - fs_create_dir_all(4)",
            cpuset.pin_task(4, 32004)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_make_cpuset_cpu_exclusive() {
        let mut cpuset = CpuSet::new("/test5/cgroups/cpuset", "prefix5").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test5/cgroups/cpuset" => Ok({}) },
            { "/test5/cgroups/cpuset/prefix5" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test5/cgroups/cpuset" => Ok(true) });
        expect!(fs_write: { "/test5/cgroups/cpuset/prefix5/cpuset.cpu_exclusive", "1" => error!("fs_write(5)") });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `5` - fs_write(5)",
            cpuset.pin_task(5, 32005)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_prefix_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test6/cgroups/cpuset", "prefix6").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test6/cgroups/cpuset" => Ok({}) },
            { "/test6/cgroups/cpuset/prefix6" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test6/cgroups/cpuset" => Ok(true) });
        expect!(fs_write: { "/test6/cgroups/cpuset/prefix6/cpuset.cpu_exclusive", "1" => Ok({}) });
        expect!(
            fs_read_to_string:
            {"/test6/cgroups/cpuset/prefix6/cpuset.mems" => error!("fs_read_to_string(6)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `6` - fs_read_to_string(6)",
            cpuset.pin_task(6, 32006)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test7/cgroups/cpuset", "prefix7").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test7/cgroups/cpuset" => Ok({}) },
            { "/test7/cgroups/cpuset/prefix7" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test7/cgroups/cpuset" => Ok(true) });
        expect!(fs_write: { "/test7/cgroups/cpuset/prefix7/cpuset.cpu_exclusive", "1" => Ok({}) });
        expect!(
            fs_read_to_string:
            { "/test7/cgroups/cpuset/prefix7/cpuset.mems" => Ok(String::new()) },
            { "/test7/cgroups/cpuset/cpuset.mems" => error!("fs_read_to_string(7)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `7` - fs_read_to_string(7)",
            cpuset.pin_task(7, 32007)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_prefix_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test8/cgroups/cpuset", "prefix8").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test8/cgroups/cpuset" => Ok({}) },
            { "/test8/cgroups/cpuset/prefix8" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test8/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test8/cgroups/cpuset/prefix8/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test8/cgroups/cpuset/prefix8/cpuset.mems", "8" => error!("fs_write(8)") },
        );
        expect!(
            fs_read_to_string:
            { "/test8/cgroups/cpuset/prefix8/cpuset.mems" => Ok(String::new()) },
            { "/test8/cgroups/cpuset/cpuset.mems" => Ok("8".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `8` - fs_write(8)",
            cpuset.pin_task(8, 32008)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_prefix_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test9/cgroups/cpuset", "prefix9").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test9/cgroups/cpuset" => Ok({}) },
            { "/test9/cgroups/cpuset/prefix9" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test9/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test9/cgroups/cpuset/prefix9/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test9/cgroups/cpuset/prefix9/cpuset.mems", "9" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test9/cgroups/cpuset/prefix9/cpuset.mems" => Ok(String::new()) },
            { "/test9/cgroups/cpuset/cpuset.mems" => Ok("9".to_string()) },
            { "/test9/cgroups/cpuset/prefix9/cpuset.cpus" => error!("fs_read_to_string(9)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `9` - fs_read_to_string(9)",
            cpuset.pin_task(9, 32009)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test10/cgroups/cpuset", "prefix10").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test10/cgroups/cpuset" => Ok({}) },
            { "/test10/cgroups/cpuset/prefix10" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test10/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test10/cgroups/cpuset/prefix10/cpuset.cpu_exclusive", "1" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test10/cgroups/cpuset/prefix10/cpuset.mems" => Ok("10".to_string()) },
            { "/test10/cgroups/cpuset/prefix10/cpuset.cpus" => Ok(String::new()) },
            { "/test10/cgroups/cpuset/cpuset.cpus" => error!("fs_read_to_string(10)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `10` - fs_read_to_string(10)",
            cpuset.pin_task(10, 32010)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_prefix_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test11/cgroups/cpuset", "prefix11").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test11/cgroups/cpuset" => Ok({}) },
            { "/test11/cgroups/cpuset/prefix11" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test11/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test11/cgroups/cpuset/prefix11/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test11/cgroups/cpuset/prefix11/cpuset.cpus", "11" => error!("fs_write(11)") },
        );
        expect!(
            fs_read_to_string:
            { "/test11/cgroups/cpuset/prefix11/cpuset.mems" => Ok("11".to_string()) },
            { "/test11/cgroups/cpuset/prefix11/cpuset.cpus" => Ok(String::new()) },
            { "/test11/cgroups/cpuset/cpuset.cpus" => Ok("11".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `11` - fs_write(11)",
            cpuset.pin_task(11, 32011)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_cpu_pool_directory() {
        let mut cpuset = CpuSet::new("/test12/cgroups/cpuset", "prefix12").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test12/cgroups/cpuset" => Ok({}) },
            { "/test12/cgroups/cpuset/prefix12" => Ok({}) },
            { "/test12/cgroups/cpuset/prefix12/pool" => error!("fs_create_dir_all(12)") },
        );
        expect!(source_mounted_at: { "/test12/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test12/cgroups/cpuset/prefix12/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test12/cgroups/cpuset/prefix12/cpuset.cpus", "0-12" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test12/cgroups/cpuset/prefix12/cpuset.mems" => Ok("12".to_string()) },
            { "/test12/cgroups/cpuset/prefix12/cpuset.cpus" => Ok(String::new()) },
            { "/test12/cgroups/cpuset/cpuset.cpus" => Ok("0-12".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `12` - fs_create_dir_all(12)",
            cpuset.pin_task(12, 32012)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_make_cpuset_pool_cpu_exclusive() {
        let mut cpuset = CpuSet::new("/test13/cgroups/cpuset", "prefix13").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test13/cgroups/cpuset" => Ok({}) },
            { "/test13/cgroups/cpuset/prefix13" => Ok({}) },
            { "/test13/cgroups/cpuset/prefix13/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test13/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test13/cgroups/cpuset/prefix13/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test13/cgroups/cpuset/prefix13/pool/cpuset.cpu_exclusive", "1" => error!("fs_write(13)") },
        );
        expect!(
            fs_read_to_string:
            { "/test13/cgroups/cpuset/prefix13/cpuset.mems" => Ok("13".to_string()) },
            { "/test13/cgroups/cpuset/prefix13/cpuset.cpus" => Ok("0-13".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `13` - fs_write(13)",
            cpuset.pin_task(13, 32013)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_mems() {
        let mut cpuset = CpuSet::new("/test14/cgroups/cpuset", "prefix14").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test14/cgroups/cpuset" => Ok({}) },
            { "/test14/cgroups/cpuset/prefix14" => Ok({}) },
            { "/test14/cgroups/cpuset/prefix14/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test14/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test14/cgroups/cpuset/prefix14/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test14/cgroups/cpuset/prefix14/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test14/cgroups/cpuset/prefix14/cpuset.mems" => Ok("14".to_string()) },
            { "/test14/cgroups/cpuset/prefix14/cpuset.cpus" => Ok("0-14".to_string()) },
            { "/test14/cgroups/cpuset/prefix14/pool/cpuset.mems" => error!("fs_read_to_string(14)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `14` - fs_read_to_string(14)",
            cpuset.pin_task(14, 32014)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_mems() {
        let mut cpuset = CpuSet::new("/test15/cgroups/cpuset", "prefix15").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test15/cgroups/cpuset" => Ok({}) },
            { "/test15/cgroups/cpuset/prefix15" => Ok({}) },
            { "/test15/cgroups/cpuset/prefix15/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test15/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test15/cgroups/cpuset/prefix15/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test15/cgroups/cpuset/prefix15/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test15/cgroups/cpuset/prefix15/pool/cpuset.mems", "15" => error!("fs_write(15)") },
        );
        expect!(
            fs_read_to_string:
            { "/test15/cgroups/cpuset/prefix15/cpuset.mems" => Ok("15".to_string()) },
            { "/test15/cgroups/cpuset/prefix15/cpuset.cpus" => Ok("0-15".to_string()) },
            { "/test15/cgroups/cpuset/prefix15/pool/cpuset.mems" => Ok(String::new()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `15` - fs_write(15)",
            cpuset.pin_task(15, 32015)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_cpus() {
        let mut cpuset = CpuSet::new("/test16/cgroups/cpuset", "prefix16").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test16/cgroups/cpuset" => Ok({}) },
            { "/test16/cgroups/cpuset/prefix16" => Ok({}) },
            { "/test16/cgroups/cpuset/prefix16/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test16/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test16/cgroups/cpuset/prefix16/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test16/cgroups/cpuset/prefix16/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test16/cgroups/cpuset/prefix16/pool/cpuset.mems", "16" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test16/cgroups/cpuset/prefix16/cpuset.mems" => Ok("16".to_string()) },
            { "/test16/cgroups/cpuset/prefix16/cpuset.cpus" => Ok("0-16".to_string()) },
            { "/test16/cgroups/cpuset/prefix16/pool/cpuset.mems" => Ok(String::new()) },
            { "/test16/cgroups/cpuset/prefix16/pool/cpuset.cpus" => error!("fs_read_to_string(16)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `16` - fs_read_to_string(16)",
            cpuset.pin_task(16, 32016)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_cpus() {
        let mut cpuset = CpuSet::new("/test17/cgroups/cpuset", "prefix17").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test17/cgroups/cpuset" => Ok({}) },
            { "/test17/cgroups/cpuset/prefix17" => Ok({}) },
            { "/test17/cgroups/cpuset/prefix17/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test17/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test17/cgroups/cpuset/prefix17/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test17/cgroups/cpuset/prefix17/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test17/cgroups/cpuset/prefix17/pool/cpuset.cpus", "0-17" => error!("fs_write(17)") },
        );
        expect!(
            fs_read_to_string:
            { "/test17/cgroups/cpuset/prefix17/cpuset.mems" => Ok("17".to_string()) },
            { "/test17/cgroups/cpuset/prefix17/cpuset.cpus" => Ok("0-17".to_string()) },
            { "/test17/cgroups/cpuset/prefix17/pool/cpuset.mems" => Ok("17".to_string()) },
            { "/test17/cgroups/cpuset/prefix17/pool/cpuset.cpus" => Ok(String::new()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `17` - fs_write(17)",
            cpuset.pin_task(17, 32017)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_tasks() {
        let mut cpuset = CpuSet::new("/test18/cgroups/cpuset", "prefix18").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test18/cgroups/cpuset" => Ok({}) },
            { "/test18/cgroups/cpuset/prefix18" => Ok({}) },
            { "/test18/cgroups/cpuset/prefix18/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test18/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test18/cgroups/cpuset/prefix18/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpus", "0-18" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test18/cgroups/cpuset/prefix18/cpuset.mems" => Ok("18".to_string()) },
            { "/test18/cgroups/cpuset/prefix18/cpuset.cpus" => Ok("0-18".to_string()) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.mems" => Ok("18".to_string()) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpus" => Ok(String::new()) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpus" => Ok("0-18".to_string()) },
            { "/test18/cgroups/cpuset/tasks" => error!("fs_read_to_string(18)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `18` - fs_read_to_string(18)",
            cpuset.pin_task(18, 32018)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_thread_tasks() {
        let mut cpuset = CpuSet::new("/test19/cgroups/cpuset", "prefix19").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test19/cgroups/cpuset" => Ok({}) },
            { "/test19/cgroups/cpuset/prefix19" => Ok({}) },
            { "/test19/cgroups/cpuset/prefix19/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test19/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test19/cgroups/cpuset/prefix19/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test19/cgroups/cpuset/prefix19/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test19/cgroups/cpuset/prefix19/pool/tasks", "1019" => error!("fs_write(19)") },
            { "/test19/cgroups/cpuset/prefix19/pool/tasks", "1119" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test19/cgroups/cpuset/prefix19/cpuset.mems" => Ok("19\n".to_string()) },
            { "/test19/cgroups/cpuset/prefix19/cpuset.cpus" => Ok("0-19\n".to_string()) },
            { "/test19/cgroups/cpuset/prefix19/pool/cpuset.mems" => Ok("19\n".to_string()) },
            { "/test19/cgroups/cpuset/prefix19/pool/cpuset.cpus" => Ok("0-19\n".to_string()) },
            { "/test19/cgroups/cpuset/prefix19/pool/cpuset.cpus" => Ok("0-19\n".to_string()) },
            { "/test19/cgroups/cpuset/tasks" => Ok("1019\n1119\n1219\n1319\n".to_string()) },
            { "/proc/1019/status" => Ok("Cpus_allowed_list:	0-19\n".to_string()) },
            { "/proc/1119/status" => Ok(
                "Name:\ttest\nNSsid:\t0\nCpus_allowed:\t0000800\nCpus_allowed_list:\t0-19\n".to_string()
            ) },
            { "/proc/1219/status" => Ok("Cpus_allowed_list:\t11\n".to_string()) },
            { "/proc/1319/status" => error!("fs_read_to_string(1319)".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test19/cgroups/cpuset/prefix19/19/tasks" => error!("fs_read_line(19)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `19` - fs_read_line(19)",
            cpuset.pin_task(19, 32019)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_open_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test20/cgroups/cpuset", "prefix20").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test20/cgroups/cpuset" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test20/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test20/cgroups/cpuset/prefix20/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20/pool/tasks", "1020" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20/pool/tasks", "2020" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test20/cgroups/cpuset/prefix20/cpuset.mems" => Ok("20\n".to_string()) },
            { "/test20/cgroups/cpuset/prefix20/cpuset.cpus" => Ok("0-20\n".to_string()) },
            { "/test20/cgroups/cpuset/prefix20/pool/cpuset.mems" => Ok("20\n".to_string()) },
            { "/test20/cgroups/cpuset/prefix20/pool/cpuset.cpus" => Ok("0-20\n".to_string()) },
            { "/test20/cgroups/cpuset/prefix20/pool/cpuset.cpus" => Ok("0-20\n".to_string()) },
            { "/test20/cgroups/cpuset/tasks" => Ok("1020\n2020\n3020\n".to_string()) },
            { "/proc/1020/status" => Ok("Cpus_allowed_list:\t0-20\n".to_string()) },
            { "/proc/2020/status" => Ok("Cpus_allowed_list:\t0-20\n".to_string()) },
            { "/proc/3020/status" => Ok("Cpus_allowed_list:\t10\n".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test20/cgroups/cpuset/prefix20/20/tasks" => Ok("1020\n".to_string()) },
        );
        expect!(
            File::open:
            { "/test20/cgroups/cpuset/prefix20/pool/cpuset.cpus" => error!("File::open(20)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `20` - File::open(20)",
            cpuset.pin_task(20, 32020)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_lock_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test21/cgroups/cpuset", "prefix21").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test21/cgroups/cpuset" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test21/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test21/cgroups/cpuset/prefix21/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool/tasks", "1121" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool/tasks", "2121" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool/tasks", "3121" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test21/cgroups/cpuset/prefix21/cpuset.mems" => Ok("21\n".to_string()) },
            { "/test21/cgroups/cpuset/prefix21/cpuset.cpus" => Ok("0-21\n".to_string()) },
            { "/test21/cgroups/cpuset/prefix21/pool/cpuset.mems" => Ok("21\n".to_string()) },
            { "/test21/cgroups/cpuset/prefix21/pool/cpuset.cpus" => Ok("0-21\n".to_string()) },
            { "/test21/cgroups/cpuset/prefix21/pool/cpuset.cpus" => Ok("0-21\n".to_string()) },
            { "/test21/cgroups/cpuset/tasks" => Ok("1121\n2121\n3121\n".to_string()) },
            { "/proc/1121/status" => Ok("Cpus_allowed_list:\t0-21\n".to_string()) },
            { "/proc/2121/status" => Ok("Cpus_allowed_list:\t0-21\n".to_string()) },
            { "/proc/3121/status" => Ok("Cpus_allowed_list:\t0-21\n".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test21/cgroups/cpuset/prefix21/21/tasks" => Ok(String::new()) },
        );
        expect!(
            File::open:
            { "/test21/cgroups/cpuset/prefix21/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![error!("File::lock(21)")],
                trim: vec_deq![],
                read_string: vec_deq![],
                write: vec_deq![],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `21` - File::lock(21)",
            cpuset.pin_task(21, 32021)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test22/cgroups/cpuset", "prefix22").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test22/cgroups/cpuset" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test22/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test22/cgroups/cpuset/prefix22/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22/pool/tasks", "1222" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22/pool/tasks", "2222" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test22/cgroups/cpuset/prefix22/cpuset.mems" => Ok("22\n".to_string()) },
            { "/test22/cgroups/cpuset/prefix22/cpuset.cpus" => Ok("0-22\n".to_string()) },
            { "/test22/cgroups/cpuset/prefix22/pool/cpuset.mems" => Ok("22\n".to_string()) },
            { "/test22/cgroups/cpuset/prefix22/pool/cpuset.cpus" => Ok("0-22\n".to_string()) },
            { "/test22/cgroups/cpuset/prefix22/pool/cpuset.cpus" => Ok("0-22\n".to_string()) },
            { "/test22/cgroups/cpuset/tasks" => Ok("1222\n2222\n3222\n".to_string()) },
            { "/proc/1222/status" => Ok("Cpus_allowed_list:\t0-22\n".to_string()) },
            { "/proc/2222/status" => Ok("Cpus_allowed_list:\t0-22\n".to_string()) },
            { "/proc/3222/status" => Ok("Cpus_allowed_list:\t1,3,5,7\n".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test22/cgroups/cpuset/prefix22/22/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test22/cgroups/cpuset/prefix22/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![],
                read_string: vec_deq![error!("File::read_string(22)")],
                write: vec_deq![],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `22` - File::read_string(22)",
            cpuset.pin_task(22, 32022)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_trim_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test23/cgroups/cpuset", "prefix23").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test23/cgroups/cpuset" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test23/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test23/cgroups/cpuset/prefix23/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool/tasks", "1323" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool/tasks", "2323" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool/tasks", "3323" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test23/cgroups/cpuset/prefix23/cpuset.mems" => Ok("23\n".to_string()) },
            { "/test23/cgroups/cpuset/prefix23/cpuset.cpus" => Ok("0-23\n".to_string()) },
            { "/test23/cgroups/cpuset/prefix23/pool/cpuset.mems" => Ok("23\n".to_string()) },
            { "/test23/cgroups/cpuset/prefix23/pool/cpuset.cpus" => Ok("0-23\n".to_string()) },
            { "/test23/cgroups/cpuset/prefix23/pool/cpuset.cpus" => Ok("0-23\n".to_string()) },
            { "/test23/cgroups/cpuset/tasks" => Ok("1323\n2323\n3323\n".to_string()) },
            { "/proc/1323/status" => Ok("Cpus_allowed_list:\t0-23\n".to_string()) },
            { "/proc/2323/status" => Ok("Cpus_allowed_list:\t0-23\n".to_string()) },
            { "/proc/3323/status" => Ok("Cpus_allowed_list:\t0-23\n".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test23/cgroups/cpuset/prefix23/23/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test23/cgroups/cpuset/prefix23/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![error!("File::trim(23)")],
                read_string: vec_deq![Ok("0-23".to_string())],
                write: vec_deq![],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `23` - File::trim(23)",
            cpuset.pin_task(23, 32023)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_cpus_file_with_isolated_thread()
    {
        let mut cpuset = CpuSet::new("/test24/cgroups/cpuset", "prefix24").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test24/cgroups/cpuset" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test24/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test24/cgroups/cpuset/prefix24/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool/tasks", "1024" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool/tasks", "2024" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool/tasks", "3024" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test24/cgroups/cpuset/prefix24/cpuset.mems" => Ok("24\n".to_string()) },
            { "/test24/cgroups/cpuset/prefix24/cpuset.cpus" => Ok("0-24\n".to_string()) },
            { "/test24/cgroups/cpuset/prefix24/pool/cpuset.mems" => Ok("24\n".to_string()) },
            { "/test24/cgroups/cpuset/prefix24/pool/cpuset.cpus" => Ok("21-24\n".to_string()) },
            { "/test24/cgroups/cpuset/prefix24/pool/cpuset.cpus" => Ok("21-24\n".to_string()) },
            { "/test24/cgroups/cpuset/tasks" => Ok("1024\n2024\n3024\n".to_string()) },
            { "/proc/1024/status" => Ok("Cpus_allowed_list:\t21-24\n".to_string()) },
            { "/proc/2024/status" => Ok("Cpus_allowed_list:\t21-24\n".to_string()) },
            { "/proc/3024/status" => Ok("Cpus_allowed_list:\t21-24\n".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test24/cgroups/cpuset/prefix24/24/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test24/cgroups/cpuset/prefix24/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("21-24".to_string())],
                write: vec_deq![
                    ("21,22,23".to_string(), Some(Error::new(ErrorKind::Other, "File::write(24)")))
                ],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `24` - File::write(24)",
            cpuset.pin_task(24, 32024)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_a_cpuset_directory_for_isolated_thread() {
        let mut cpuset = CpuSet::new("/test25/cgroups/cpuset", "prefix25").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test25/cgroups/cpuset" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/25" => error!("fs_create_dir_all(25)") },
        );
        expect!(source_mounted_at: { "/test25/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test25/cgroups/cpuset/prefix25/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool/tasks", "2025" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool/tasks", "3025" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool/tasks", "4025" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test25/cgroups/cpuset/prefix25/cpuset.mems" => Ok("25".to_string()) },
            { "/test25/cgroups/cpuset/prefix25/cpuset.cpus" => Ok("23-25".to_string()) },
            { "/test25/cgroups/cpuset/prefix25/pool/cpuset.mems" => Ok("25".to_string()) },
            { "/test25/cgroups/cpuset/prefix25/pool/cpuset.cpus" => Ok("23-25".to_string()) },
            { "/test25/cgroups/cpuset/prefix25/pool/cpuset.cpus" => Ok("22-25".to_string()) },
            { "/test25/cgroups/cpuset/tasks" => Ok("2025\n3025\n4025\n".to_string()) },
            { "/proc/2025/status" => Ok("Cpus_allowed_list:	22-25\n".to_string()) },
            { "/proc/3025/status" => Ok("Cpus_allowed_list:	22-25\n".to_string()) },
            { "/proc/4025/status" => Ok("Cpus_allowed_list:	22-25\n".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test25/cgroups/cpuset/prefix25/25/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test25/cgroups/cpuset/prefix25/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("22-25".to_string())],
                write: vec_deq![("22,23,24".to_string(), None)],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `25` - fs_create_dir_all(25)",
            cpuset.pin_task(25, 32025)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_mems_for_isloated_thread() {
        let mut cpuset = CpuSet::new("/test26/cgroups/cpuset", "prefix26").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test26/cgroups/cpuset" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/26" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test26/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test26/cgroups/cpuset/prefix26/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool/tasks", "2026" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool/tasks", "3026" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool/tasks", "4026" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/26/cpuset.mems", "26" => error!("fs_write(26)") },
        );
        expect!(
            fs_read_to_string:
            { "/test26/cgroups/cpuset/prefix26/cpuset.mems" => Ok("26".to_string()) },
            { "/test26/cgroups/cpuset/prefix26/cpuset.cpus" => Ok("23-26".to_string()) },
            { "/test26/cgroups/cpuset/prefix26/pool/cpuset.mems" => Ok("26".to_string()) },
            { "/test26/cgroups/cpuset/prefix26/pool/cpuset.cpus" => Ok("23-26".to_string()) },
            { "/test26/cgroups/cpuset/prefix26/pool/cpuset.cpus" => Ok("23-26".to_string()) },
            { "/test26/cgroups/cpuset/tasks" => Ok("2026\n3026\n4026\n".to_string()) },
            { "/proc/2026/status" => Ok("Cpus_allowed_list:	23-26\n".to_string()) },
            { "/proc/3026/status" => Ok("Cpus_allowed_list:	23-26\n".to_string()) },
            { "/proc/4026/status" => Ok("Cpus_allowed_list:	23-26\n".to_string()) },
            { "/test26/cgroups/cpuset/prefix26/cpuset.mems" => Ok("26".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test26/cgroups/cpuset/prefix26/26/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test26/cgroups/cpuset/prefix26/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("23-26".to_string())],
                write: vec_deq![("23,24,25".to_string(), None)],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `26` - fs_write(26)",
            cpuset.pin_task(26, 32026)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_cpu_exclusive_for_isloated_thread() {
        let mut cpuset = CpuSet::new("/test27/cgroups/cpuset", "prefix27").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test27/cgroups/cpuset" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/27" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test27/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test27/cgroups/cpuset/prefix27/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool/tasks", "2027" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool/tasks", "3027" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool/tasks", "4027" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/27/cpuset.mems", "27" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/27/cpuset.cpu_exclusive", "1" => error!("fs_write(27)") },
        );
        expect!(
            fs_read_to_string:
            { "/test27/cgroups/cpuset/prefix27/cpuset.mems" => Ok("27".to_string()) },
            { "/test27/cgroups/cpuset/prefix27/cpuset.cpus" => Ok("23-27".to_string()) },
            { "/test27/cgroups/cpuset/prefix27/pool/cpuset.mems" => Ok("27".to_string()) },
            { "/test27/cgroups/cpuset/prefix27/pool/cpuset.cpus" => Ok("23-27".to_string()) },
            { "/test27/cgroups/cpuset/prefix27/pool/cpuset.cpus" => Ok("23-27".to_string()) },
            { "/test27/cgroups/cpuset/tasks" => Ok("2027\n3027\n4027\n".to_string()) },
            { "/proc/2027/status" => Ok("Cpus_allowed_list:	23-27\n".to_string()) },
            { "/proc/3027/status" => Ok("Cpus_allowed_list:	23-27\n".to_string()) },
            { "/proc/4027/status" => Ok("Cpus_allowed_list:	23-27\n".to_string()) },
            { "/test27/cgroups/cpuset/prefix27/cpuset.mems" => Ok("27".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test27/cgroups/cpuset/prefix27/27/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test27/cgroups/cpuset/prefix27/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("23-27".to_string())],
                write: vec_deq![("23,24,25,26".to_string(), None)],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `27` - fs_write(27)",
            cpuset.pin_task(27, 32027)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_cpus_for_isolated_thread() {
        let mut cpuset = CpuSet::new("/test28/cgroups/cpuset", "prefix28").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test28/cgroups/cpuset" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/28" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test28/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test28/cgroups/cpuset/prefix28/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool/tasks", "2028" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool/tasks", "3028" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool/tasks", "4028" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/28/cpuset.mems", "28" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/28/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/28/cpuset.cpus", "28" => error!("fs_write(28)") },
        );
        expect!(
            fs_read_to_string:
            { "/test28/cgroups/cpuset/prefix28/cpuset.mems" => Ok("28".to_string()) },
            { "/test28/cgroups/cpuset/prefix28/cpuset.cpus" => Ok("23-28".to_string()) },
            { "/test28/cgroups/cpuset/prefix28/pool/cpuset.mems" => Ok("28".to_string()) },
            { "/test28/cgroups/cpuset/prefix28/pool/cpuset.cpus" => Ok("23-28".to_string()) },
            { "/test28/cgroups/cpuset/prefix28/pool/cpuset.cpus" => Ok("23-28".to_string()) },
            { "/test28/cgroups/cpuset/tasks" => Ok("2028\n3028\n4028\n".to_string()) },
            { "/proc/2028/status" => Ok("Cpus_allowed_list:	23-28\n".to_string()) },
            { "/proc/3028/status" => Ok("Cpus_allowed_list:	23-28\n".to_string()) },
            { "/proc/4028/status" => Ok("Cpus_allowed_list:	23-28\n".to_string()) },
            { "/test28/cgroups/cpuset/prefix28/cpuset.mems" => Ok("28".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test28/cgroups/cpuset/prefix28/28/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test28/cgroups/cpuset/prefix28/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("23-28".to_string())],
                write: vec_deq![("23,24,25,26,27".to_string(), None)],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `28` - fs_write(28)",
            cpuset.pin_task(28, 32028)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_pin_task_to_isolated_thread() {
        let mut cpuset = CpuSet::new("/test29/cgroups/cpuset", "prefix29").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test29/cgroups/cpuset" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test29/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test29/cgroups/cpuset/prefix29/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool/tasks", "2029" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool/tasks", "3029" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool/tasks", "4029" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29/cpuset.mems", "29" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29/cpuset.cpus", "29" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29/tasks", "32029" => error!("fs_write(29)") },
        );
        expect!(
            fs_read_to_string:
            { "/test29/cgroups/cpuset/prefix29/cpuset.mems" => Ok("29".to_string()) },
            { "/test29/cgroups/cpuset/prefix29/cpuset.cpus" => Ok("25-29".to_string()) },
            { "/test29/cgroups/cpuset/prefix29/pool/cpuset.mems" => Ok("29".to_string()) },
            { "/test29/cgroups/cpuset/prefix29/pool/cpuset.cpus" => Ok("25-29".to_string()) },
            { "/test29/cgroups/cpuset/prefix29/pool/cpuset.cpus" => Ok("25-29".to_string()) },
            { "/test29/cgroups/cpuset/tasks" => Ok("2029\n3029\n4029\n".to_string()) },
            { "/proc/2029/status" => Ok("Cpus_allowed_list:	25-29\n".to_string()) },
            { "/proc/3029/status" => Ok("Cpus_allowed_list:	25-29\n".to_string()) },
            { "/proc/4029/status" => Ok("Cpus_allowed_list:	25-29\n".to_string()) },
            { "/test29/cgroups/cpuset/prefix29/cpuset.mems" => Ok("29".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test29/cgroups/cpuset/prefix29/29/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test29/cgroups/cpuset/prefix29/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("25-29".to_string())],
                write: vec_deq![("25,26,27,28".to_string(), None)],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to pin the process id `32029` to the host cpu thread `29` - fs_write(29)",
            cpuset.pin_task(29, 32029)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_isolates_the_thread_and_pins_the_task_to_it() {
        let mut cpuset = CpuSet::new("/test30/cgroups/cpuset", "prefix30").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test30/cgroups/cpuset" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/pool" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/30" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test30/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test30/cgroups/cpuset/prefix30/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/pool/tasks", "2030" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/pool/tasks", "3030" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/pool/tasks", "4030" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/30/cpuset.mems", "30" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/30/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/30/cpuset.cpus", "30" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/30/tasks", "3030" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test30/cgroups/cpuset/prefix30/cpuset.mems" => Ok("30".to_string()) },
            { "/test30/cgroups/cpuset/prefix30/cpuset.cpus" => Ok("25-30".to_string()) },
            { "/test30/cgroups/cpuset/prefix30/pool/cpuset.mems" => Ok("30".to_string()) },
            { "/test30/cgroups/cpuset/prefix30/pool/cpuset.cpus" => Ok("25-30".to_string()) },
            { "/test30/cgroups/cpuset/prefix30/pool/cpuset.cpus" => Ok("25-30".to_string()) },
            { "/test30/cgroups/cpuset/tasks" => Ok("2030\n3030\n4030\n".to_string()) },
            { "/proc/2030/status" => Ok("Cpus_allowed_list:	25-30\n".to_string()) },
            { "/proc/3030/status" => Ok("Cpus_allowed_list:	25-30\n".to_string()) },
            { "/proc/4030/status" => Ok("Cpus_allowed_list:	25-30\n".to_string()) },
            { "/test30/cgroups/cpuset/prefix30/cpuset.mems" => Ok("30".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test30/cgroups/cpuset/prefix30/30/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test30/cgroups/cpuset/prefix30/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("25-30".to_string())],
                write: vec_deq![("25,26,27,28,29".to_string(), None)],
            }) },
        );

        assert!(cpuset.pin_task(30, 3030).is_ok());

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_unable_to_read_pinned_thread_tasks() {
        let mut cpuset = CpuSet::new("/test31/cgroups/cpuset", "prefix31").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test31/cgroups/cpuset" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/pool" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/31" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test31/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test31/cgroups/cpuset/prefix31/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/pool/tasks", "2031" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/pool/tasks", "3031" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/pool/tasks", "4031" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/31/cpuset.mems", "31" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/31/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/31/cpuset.cpus", "31" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/31/tasks", "3031" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test31/cgroups/cpuset/prefix31/cpuset.mems" => Ok("31".to_string()) },
            { "/test31/cgroups/cpuset/prefix31/cpuset.cpus" => Ok("30-31".to_string()) },
            { "/test31/cgroups/cpuset/prefix31/pool/cpuset.mems" => Ok("31".to_string()) },
            { "/test31/cgroups/cpuset/prefix31/pool/cpuset.cpus" => Ok("30-31".to_string()) },
            { "/test31/cgroups/cpuset/prefix31/pool/cpuset.cpus" => Ok("30-31".to_string()) },
            { "/test31/cgroups/cpuset/tasks" => Ok("2031\n3031\n4031\n".to_string()) },
            { "/proc/2031/status" => Ok("Cpus_allowed_list:	30-31\n".to_string()) },
            { "/proc/3031/status" => Ok("Cpus_allowed_list:	30-31\n".to_string()) },
            { "/proc/4031/status" => Ok("Cpus_allowed_list:	30-31\n".to_string()) },
            { "/test31/cgroups/cpuset/prefix31/cpuset.mems" => Ok("31".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test31/cgroups/cpuset/prefix31/31/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test31/cgroups/cpuset/prefix31/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("30-31".to_string())],
                write: vec_deq![("30".to_string(), None)],
            }) },
        );

        cpuset.pin_task(31, 3031).unwrap();

        expect!(fs_read_line: { "/test31/cgroups/cpuset/prefix31/31/tasks" => error!("fs_read_line(31)") });

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_thread_still_busy_with_at_least_one_process() {
        let mut cpuset = CpuSet::new("/test32/cgroups/cpuset", "prefix32").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test32/cgroups/cpuset" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/pool" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/32" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test32/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test32/cgroups/cpuset/prefix32/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/pool/tasks", "2032" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/pool/tasks", "3032" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/pool/tasks", "4032" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/32/cpuset.mems", "32" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/32/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/32/cpuset.cpus", "32" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/32/tasks", "2032" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test32/cgroups/cpuset/prefix32/cpuset.mems" => Ok("32".to_string()) },
            { "/test32/cgroups/cpuset/prefix32/cpuset.cpus" => Ok("30-32".to_string()) },
            { "/test32/cgroups/cpuset/prefix32/pool/cpuset.mems" => Ok("32".to_string()) },
            { "/test32/cgroups/cpuset/prefix32/pool/cpuset.cpus" => Ok("30-32".to_string()) },
            { "/test32/cgroups/cpuset/prefix32/pool/cpuset.cpus" => Ok("30-32".to_string()) },
            { "/test32/cgroups/cpuset/tasks" => Ok("2032\n3032\n4032\n".to_string()) },
            { "/proc/2032/status" => Ok("Cpus_allowed_list:	30-32\n".to_string()) },
            { "/proc/3032/status" => Ok("Cpus_allowed_list:	30-32\n".to_string()) },
            { "/proc/4032/status" => Ok("Cpus_allowed_list:	30-32\n".to_string()) },
            { "/test32/cgroups/cpuset/prefix32/cpuset.mems" => Ok("32".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test32/cgroups/cpuset/prefix32/32/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test32/cgroups/cpuset/prefix32/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("30-32".to_string())],
                write: vec_deq![("30,31".to_string(), None)],
            }) },
        );

        cpuset.pin_task(32, 2032).unwrap();

        expect!(
            fs_read_line:
            { "/test32/cgroups/cpuset/prefix32/32/tasks" => Ok("4032".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_unable_to_remove_thread_cpuset_cgroup_directory() {
        let mut cpuset = CpuSet::new("/test33/cgroups/cpuset", "prefix33").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test33/cgroups/cpuset" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/pool" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/33" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test33/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test33/cgroups/cpuset/prefix33/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/pool/tasks", "2033" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/pool/tasks", "3033" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/pool/tasks", "4033" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/33/cpuset.mems", "33" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/33/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/33/cpuset.cpus", "33" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/33/tasks", "2033" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test33/cgroups/cpuset/prefix33/cpuset.mems" => Ok("33".to_string()) },
            { "/test33/cgroups/cpuset/prefix33/cpuset.cpus" => Ok("30-33".to_string()) },
            { "/test33/cgroups/cpuset/prefix33/pool/cpuset.mems" => Ok("33".to_string()) },
            { "/test33/cgroups/cpuset/prefix33/pool/cpuset.cpus" => Ok("30-33".to_string()) },
            { "/test33/cgroups/cpuset/prefix33/pool/cpuset.cpus" => Ok("30-33".to_string()) },
            { "/test33/cgroups/cpuset/tasks" => Ok("2033\n3033\n4033\n".to_string()) },
            { "/proc/2033/status" => Ok("Cpus_allowed_list:	30-33\n".to_string()) },
            { "/proc/3033/status" => Ok("Cpus_allowed_list:	30-33\n".to_string()) },
            { "/proc/4033/status" => Ok("Cpus_allowed_list:	30-33\n".to_string()) },
            { "/test33/cgroups/cpuset/prefix33/cpuset.mems" => Ok("33".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test33/cgroups/cpuset/prefix33/33/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test33/cgroups/cpuset/prefix33/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("30-33".to_string())],
                write: vec_deq![("30,31,32".to_string(), None)],
            }) },
        );

        cpuset.pin_task(33, 2033).unwrap();

        expect!(fs_read_line: { "/test33/cgroups/cpuset/prefix33/33/tasks" => Ok(String::new()) });
        expect!(fs_remove_dir: { "/test33/cgroups/cpuset/prefix33/33" => error!("fs_remove_dir(33)") });

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_open_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test34/cgroups/cpuset", "prefix34").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test34/cgroups/cpuset" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/pool" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/34" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test34/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test34/cgroups/cpuset/prefix34/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/pool/tasks", "2034" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/pool/tasks", "3034" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/pool/tasks", "4034" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/34/cpuset.mems", "34" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/34/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/34/cpuset.cpus", "34" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/34/tasks", "2034" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test34/cgroups/cpuset/prefix34/cpuset.mems" => Ok("34".to_string()) },
            { "/test34/cgroups/cpuset/prefix34/cpuset.cpus" => Ok("30-34".to_string()) },
            { "/test34/cgroups/cpuset/prefix34/pool/cpuset.mems" => Ok("34".to_string()) },
            { "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpus" => Ok("30-34".to_string()) },
            { "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpus" => Ok("30-34".to_string()) },
            { "/test34/cgroups/cpuset/tasks" => Ok("2034\n3034\n4034\n".to_string()) },
            { "/proc/2034/status" => Ok("Cpus_allowed_list:	30-34\n".to_string()) },
            { "/proc/3034/status" => Ok("Cpus_allowed_list:	30-34\n".to_string()) },
            { "/proc/4034/status" => Ok("Cpus_allowed_list:	30-34\n".to_string()) },
            { "/test34/cgroups/cpuset/prefix34/cpuset.mems" => Ok("34".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test34/cgroups/cpuset/prefix34/34/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("30-34".to_string())],
                write: vec_deq![("30,31,32,33".to_string(), None)],
            }) },
        );

        cpuset.pin_task(34, 2034).unwrap();

        expect!(fs_read_line: { "/test34/cgroups/cpuset/prefix34/34/tasks" => Ok(String::new()) });
        expect!(fs_remove_dir: { "/test34/cgroups/cpuset/prefix34/34" => Ok({}) });
        expect!(File::open: { "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpus" => error!("File::open(34)") });

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_lock_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test35/cgroups/cpuset", "prefix35").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test35/cgroups/cpuset" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/pool" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/35" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test35/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test35/cgroups/cpuset/prefix35/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/pool/tasks", "2035" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/pool/tasks", "3035" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/pool/tasks", "4035" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/35/cpuset.mems", "35" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/35/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/35/cpuset.cpus", "35" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/35/tasks", "2035" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test35/cgroups/cpuset/prefix35/cpuset.mems" => Ok("35".to_string()) },
            { "/test35/cgroups/cpuset/prefix35/cpuset.cpus" => Ok("30-35".to_string()) },
            { "/test35/cgroups/cpuset/prefix35/pool/cpuset.mems" => Ok("35".to_string()) },
            { "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpus" => Ok("30-35".to_string()) },
            { "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpus" => Ok("30-35".to_string()) },
            { "/test35/cgroups/cpuset/tasks" => Ok("2035\n3035\n4035\n".to_string()) },
            { "/proc/2035/status" => Ok("Cpus_allowed_list:	30-35\n".to_string()) },
            { "/proc/3035/status" => Ok("Cpus_allowed_list:	30-35\n".to_string()) },
            { "/proc/4035/status" => Ok("Cpus_allowed_list:	30-35\n".to_string()) },
            { "/test35/cgroups/cpuset/prefix35/cpuset.mems" => Ok("35".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test35/cgroups/cpuset/prefix35/35/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("30-35".to_string())],
                write: vec_deq![("30,31,32,33,34".to_string(), None)],
            }) },
        );

        cpuset.pin_task(35, 2035).unwrap();

        expect!(fs_read_line: { "/test35/cgroups/cpuset/prefix35/35/tasks" => Ok(String::new()) });
        expect!(fs_remove_dir: { "/test35/cgroups/cpuset/prefix35/35" => Ok({}) });
        expect!(File::open: { "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpus" => Ok(File {
            lock: vec_deq![error!("File::lock(35)")],
            trim: vec_deq![],
            read_string: vec_deq![],
            write: vec_deq![],
        }) });

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_read_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test36/cgroups/cpuset", "prefix36").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test36/cgroups/cpuset" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/pool" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/36" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test36/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test36/cgroups/cpuset/prefix36/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/pool/tasks", "2036" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/pool/tasks", "3036" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/pool/tasks", "4036" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/36/cpuset.mems", "36" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/36/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/36/cpuset.cpus", "36" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/36/tasks", "4036" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test36/cgroups/cpuset/prefix36/cpuset.mems" => Ok("36".to_string()) },
            { "/test36/cgroups/cpuset/prefix36/cpuset.cpus" => Ok("30-36".to_string()) },
            { "/test36/cgroups/cpuset/prefix36/pool/cpuset.mems" => Ok("36".to_string()) },
            { "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpus" => Ok("30-36".to_string()) },
            { "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpus" => Ok("32-36".to_string()) },
            { "/test36/cgroups/cpuset/tasks" => Ok("2036\n3036\n4036\n".to_string()) },
            { "/proc/2036/status" => Ok("Cpus_allowed_list:	32-36\n".to_string()) },
            { "/proc/3036/status" => Ok("Cpus_allowed_list:	32-36\n".to_string()) },
            { "/proc/4036/status" => Ok("Cpus_allowed_list:	32-36\n".to_string()) },
            { "/test36/cgroups/cpuset/prefix36/cpuset.mems" => Ok("36".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test36/cgroups/cpuset/prefix36/36/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("32-36".to_string())],
                write: vec_deq![("32,33,34,35".to_string(), None)],
            }) },
        );

        cpuset.pin_task(36, 4036).unwrap();

        expect!(fs_read_line: { "/test36/cgroups/cpuset/prefix36/36/tasks" => Ok(String::new()) });
        expect!(fs_remove_dir: { "/test36/cgroups/cpuset/prefix36/36" => Ok({}) });
        expect!(
            File::open:
            { "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![],
                read_string: vec_deq![error!("File::read_string(36)")],
                write: vec_deq![],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_trim_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test37/cgroups/cpuset", "prefix37").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test37/cgroups/cpuset" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/pool" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/37" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test37/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test37/cgroups/cpuset/prefix37/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/pool/tasks", "2037" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/pool/tasks", "3037" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/pool/tasks", "4037" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/37/cpuset.mems", "37" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/37/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/37/cpuset.cpus", "37" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/37/tasks", "4037" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test37/cgroups/cpuset/prefix37/cpuset.mems" => Ok("37".to_string()) },
            { "/test37/cgroups/cpuset/prefix37/cpuset.cpus" => Ok("32-37".to_string()) },
            { "/test37/cgroups/cpuset/prefix37/pool/cpuset.mems" => Ok("37".to_string()) },
            { "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpus" => Ok("32-37".to_string()) },
            { "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpus" => Ok("32-37".to_string()) },
            { "/test37/cgroups/cpuset/tasks" => Ok("2037\n3037\n4037\n".to_string()) },
            { "/proc/2037/status" => Ok("Cpus_allowed_list:	32-37\n".to_string()) },
            { "/proc/3037/status" => Ok("Cpus_allowed_list:	32-37\n".to_string()) },
            { "/proc/4037/status" => Ok("Cpus_allowed_list:	32-37\n".to_string()) },
            { "/test37/cgroups/cpuset/prefix37/cpuset.mems" => Ok("37".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test37/cgroups/cpuset/prefix37/37/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("32-37".to_string())],
                write: vec_deq![("32,33,34,35,36".to_string(), None)],
            }) },
        );

        cpuset.pin_task(37, 4037).unwrap();

        expect!(fs_read_line: { "/test37/cgroups/cpuset/prefix37/37/tasks" => Ok("\n".to_string()) });
        expect!(fs_remove_dir: { "/test37/cgroups/cpuset/prefix37/37" => Ok({}) });
        expect!(
            File::open:
            { "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![error!("File::trim(37)")],
                read_string: vec_deq![Ok("32-36".to_string())],
                write: vec_deq![],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_write_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test38/cgroups/cpuset", "prefix38").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test38/cgroups/cpuset" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/pool" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/38" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test38/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test38/cgroups/cpuset/prefix38/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/pool/tasks", "2038" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/pool/tasks", "3038" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/pool/tasks", "4038" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/38/cpuset.mems", "38" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/38/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/38/cpuset.cpus", "38" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/38/tasks", "1038" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test38/cgroups/cpuset/prefix38/cpuset.mems" => Ok("38".to_string()) },
            { "/test38/cgroups/cpuset/prefix38/cpuset.cpus" => Ok("32-38".to_string()) },
            { "/test38/cgroups/cpuset/prefix38/pool/cpuset.mems" => Ok("38".to_string()) },
            { "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpus" => Ok("32-38".to_string()) },
            { "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpus" => Ok("32-38".to_string()) },
            { "/test38/cgroups/cpuset/tasks" => Ok("2038\n3038\n4038\n".to_string()) },
            { "/proc/2038/status" => Ok("Cpus_allowed_list:	32-38\n".to_string()) },
            { "/proc/3038/status" => Ok("Cpus_allowed_list:	32-38\n".to_string()) },
            { "/proc/4038/status" => Ok("Cpus_allowed_list:	32-38\n".to_string()) },
            { "/test38/cgroups/cpuset/prefix38/cpuset.mems" => Ok("38".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test38/cgroups/cpuset/prefix38/38/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("32-38".to_string())],
                write: vec_deq![("32,33,34,35,36,37".to_string(), None)],
            }) },
        );

        cpuset.pin_task(38, 1038).unwrap();

        expect!(fs_read_line: { "/test38/cgroups/cpuset/prefix38/38/tasks" => Ok("\n".to_string()) });
        expect!(fs_remove_dir: { "/test38/cgroups/cpuset/prefix38/38" => Ok({}) });
        expect!(
            File::open:
            { "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("32-37".to_string())],
                write: vec_deq![
                    ("32,33,34,35,36,37,38".to_string(), Some(Error::new(ErrorKind::Other, "File::write(38)"))),
                ],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_all_pinned_threads_back_to_pool() {
        let mut cpuset = CpuSet::new("/test39/cgroups/cpuset", "prefix39").unwrap();

        expect!(
            fs_create_dir_all:
            { "/test39/cgroups/cpuset" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/pool" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/39" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test39/cgroups/cpuset" => Ok(true) });
        expect!(
            fs_write:
            { "/test39/cgroups/cpuset/prefix39/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/pool/tasks", "2039" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/pool/tasks", "3039" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/pool/tasks", "4039" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/39/cpuset.mems", "39" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/39/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/39/cpuset.cpus", "39" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/39/tasks", "1039" => Ok({}) },
        );
        expect!(
            fs_read_to_string:
            { "/test39/cgroups/cpuset/prefix39/cpuset.mems" => Ok("39".to_string()) },
            { "/test39/cgroups/cpuset/prefix39/cpuset.cpus" => Ok("35-39".to_string()) },
            { "/test39/cgroups/cpuset/prefix39/pool/cpuset.mems" => Ok("39".to_string()) },
            { "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpus" => Ok("35-39".to_string()) },
            { "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpus" => Ok("35-39".to_string()) },
            { "/test39/cgroups/cpuset/tasks" => Ok("2039\n3039\n4039\n".to_string()) },
            { "/proc/2039/status" => Ok("Cpus_allowed_list:	35-39\n".to_string()) },
            { "/proc/3039/status" => Ok("Cpus_allowed_list:	35-39\n".to_string()) },
            { "/proc/4039/status" => Ok("Cpus_allowed_list:	35-39\n".to_string()) },
            { "/test39/cgroups/cpuset/prefix39/cpuset.mems" => Ok("39".to_string()) },
        );
        expect!(
            fs_read_line:
            { "/test39/cgroups/cpuset/prefix39/39/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "fs_read_line()")) },
        );
        expect!(
            File::open:
            { "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("35-39".to_string())],
                write: vec_deq![("35,36,37,38".to_string(), None)],
            }) },
        );

        cpuset.pin_task(39, 1039).unwrap();

        expect!(fs_read_line: { "/test39/cgroups/cpuset/prefix39/39/tasks" => Ok(String::new()) });
        expect!(fs_remove_dir: { "/test39/cgroups/cpuset/prefix39/39" => Ok({}) });
        expect!(
            File::open:
            { "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpus" => Ok(File {
                lock: vec_deq![Ok({})],
                trim: vec_deq![Ok({})],
                read_string: vec_deq![Ok("35-38".to_string())],
                write: vec_deq![("35,36,37,38,39".to_string(), None)],
            }) },
        );

        assert!(cpuset.release_threads().is_ok());

        verify_expectations!();
    }
}
