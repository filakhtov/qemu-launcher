use nix::{fcntl::FlockArg, mount::MsFlags};
use std::{
    io::prelude::*,
    io::{BufRead, BufReader, Error, ErrorKind, SeekFrom},
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
};
#[cfg(test)]
use test::{
    nix::{fcntl::flock, mount::mount},
    proc_mounts::MountIter,
    std::fs,
};
#[cfg(not(test))]
use {
    nix::{fcntl::flock, mount::mount},
    proc_mounts::MountIter,
    std::fs,
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

        if let Err(e) = fs::write(
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
        fs::create_dir_all(&path)?;

        let mems = fs::read_to_string(path!(self.cpuset_path(), "cpuset.mems"))?;
        fs::write(path!(path, "cpuset.mems"), mems)?;
        fs::write(path!(path, "cpuset.cpu_exclusive"), "1")?;
        fs::write(path!(path, "cpuset.cpus"), id.to_string())?;

        self.isolated_threads.push(id);

        Ok({})
    }

    fn split_thread_from_pool(&self, id: &usize) -> Result<(), Error> {
        let mut file = self.open_pool_cpus_file()?;
        let mut cpus = read_cpus_from_file(&mut file)?;
        cpus.retain(|cpu| cpu != &id.to_string());
        write_cpus_to_file(&mut file, cpus)?;

        Ok({})
    }

    fn return_thread_to_pool(&self, id: &usize) -> Result<(), Error> {
        let mut file = self.open_pool_cpus_file()?;
        let mut cpus = read_cpus_from_file(&mut file)?;
        cpus.push(id.to_string());
        write_cpus_to_file(&mut file, cpus)?;

        Ok({})
    }

    fn open_pool_cpus_file(&self) -> Result<fs::File, Error> {
        let path = path!(self.cpuset_path(), "pool", "cpuset.cpus");
        let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;

        // TODO: add debug on success
        if let Err(e) = flock(file.as_raw_fd(), FlockArg::LockExclusive) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Failed to lock `{}`: {}", path.display(), e),
            ));
        }

        Ok(file)
    }

    fn prepare_cpuset(&self) -> Result<(), Error> {
        self.ensure_mounted()?;
        self.configure_cpuset()?;
        self.migrate_tasks()?;
        Ok({})
    }

    fn migrate_tasks(&self) -> Result<(), Error> {
        let pool_cpus_path = path!(self.cpuset_path(), "pool", "cpuset.cpus");
        let pool_cpus = parse_cpus_list(fs::read_to_string(pool_cpus_path)?.trim());

        let file = fs::read_to_string(path!(self.mount_path, "tasks"))?;
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
                match fs::write(&path, task) {
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
        fs::create_dir_all(&path)?;
        fs::write(path!(path, "cpuset.cpu_exclusive"), "1")?;

        let mems_path = path!(path, "cpuset.mems");
        let mut mems = fs::read_to_string(&mems_path)?.trim().to_owned();
        if mems.len() == 0 {
            mems = fs::read_to_string(path!(self.mount_path, "cpuset.mems"))?
                .trim()
                .to_owned();
            fs::write(&mems_path, &mems)?;
        }

        let cpus_path = path!(path, "cpuset.cpus");
        let mut cpus = fs::read_to_string(&cpus_path)?.trim().to_owned();
        if cpus.len() == 0 {
            cpus = fs::read_to_string(path!(self.mount_path, "cpuset.cpus"))?
                .trim()
                .to_owned();
            fs::write(&cpus_path, &cpus)?;
        }

        let path = path!(self.cpuset_path(), "pool");
        fs::create_dir_all(&path)?;
        fs::write(path!(path, "cpuset.cpu_exclusive"), "1")?;

        let mems_path = path!(path, "cpuset.mems");
        if fs::read_to_string(&mems_path)?.trim().len() == 0 {
            fs::write(&mems_path, mems)?;
        }

        let cpus_path = path!(path, "cpuset.cpus");
        if fs::read_to_string(&cpus_path)?.trim().len() == 0 {
            fs::write(&cpus_path, cpus)?;
        }

        Ok({})
    }

    pub fn release_threads(&mut self) -> Result<(), Error> {
        let mut errors = false;

        for id in &self.isolated_threads {
            match self.is_thread_free(id) {
                Ok(None) => match fs::remove_dir(path!(self.cpuset_path(), id.to_string())) {
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
        let tasks_file_path = path!(self.cpuset_path(), id.to_string(), "tasks");
        let mut reader = BufReader::new(fs::File::open(tasks_file_path)?);
        let mut data = String::new();
        reader.read_line(&mut data)?;

        let task = data.trim().to_owned();

        if task.len() > 0 {
            return Ok(Some(task));
        }

        Ok(None)
    }

    fn ensure_mounted(&self) -> Result<(), Error> {
        fs::create_dir_all(&self.mount_path)?;

        match MountIter::<BufReader<fs::File>>::source_mounted_at("cgroup", &self.mount_path) {
            Ok(true) => Ok({}),
            Ok(false) => self.mount_cpuset(),
            Err(e) => Err(Error::new(
                ErrorKind::Other,
                format!("An error occurred while reading mounts: {}", e),
            )),
        }
    }

    fn mount_cpuset(&self) -> Result<(), Error> {
        if let Err(e) = mount(
            Some("cgroup"),
            &self.mount_path,
            Some("cgroup"),
            MsFlags::empty(),
            Some("cpuset"),
        ) {
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

fn get_task_cpus(task: &str) -> Result<Vec<usize>, Error> {
    let task_status = fs::read_to_string(path!("/proc", task, "status"))?;
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

#[cfg(test)]
mod test {
    use super::CpuSet;
    use crate::{assert_error, vec_deq};
    use ::nix::{fcntl::FlockArg, mount::MsFlags};
    use ::std::{
        cell::RefCell,
        collections::VecDeque,
        io::{Error, ErrorKind, SeekFrom},
        os::unix::io::RawFd,
    };

    macro_rules! error {
        ($msg:expr) => {{
            Err(Error::new(ErrorKind::Other, format!("{}", $msg)))
        }};
    }

    struct TestExpectations {
        write: VecDeque<((String, String), Result<(), Error>)>,
        read_to_string: VecDeque<(String, Result<String, Error>)>,
        create_dir_all: VecDeque<(String, Result<(), Error>)>,
        source_mounted_at: VecDeque<(String, Result<bool, Error>)>,
        mount: VecDeque<((String, String, String, MsFlags, String), ::nix::Result<()>)>,
        file_open: VecDeque<(String, Result<std::fs::File, Error>)>,
        remove_dir: VecDeque<(String, Result<(), Error>)>,
        file_as_raw_fd: VecDeque<RawFd>,
        file_set_len: VecDeque<(u64, Result<(), Error>)>,
        openoptions_new: VecDeque<std::fs::OpenOptions>,
        openoptions_open: VecDeque<(String, Result<std::fs::File, Error>)>,
        flock: VecDeque<((RawFd, ::nix::fcntl::FlockArg), ::nix::Result<()>)>,
    }

    impl TestExpectations {
        fn new() -> Self {
            TestExpectations {
                write: vec_deq![],
                read_to_string: vec_deq![],
                create_dir_all: vec_deq![],
                source_mounted_at: vec_deq![],
                mount: vec_deq![],
                file_open: vec_deq![],
                file_as_raw_fd: vec_deq![],
                file_set_len: vec_deq![],
                remove_dir: vec_deq![],
                openoptions_new: vec_deq![],
                openoptions_open: vec_deq![],
                flock: vec_deq![],
            }
        }
    }

    thread_local! { static TEST_EXPECTATIONS: RefCell<TestExpectations> = RefCell::new(TestExpectations::new()) }

    macro_rules! expect {
        (std::fs::write: $( { $path:expr, $data:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().write
                    .push_back(((String::from($path), String::from($data)), $result)); )*
            });
        }};
        (std::fs::read_to_string: $( { $path:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().read_to_string.push_back((String::from($path), $result)); )*
            });
        }};
        (std::fs::create_dir_all: $( { $path:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().create_dir_all.push_back((String::from($path), $result)); )*
            });
        }};
        (source_mounted_at: $( { $path:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().source_mounted_at.push_back((String::from($path), $result)); )*
            });
        }};
        (std::fs::File::open: $( { $path:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().file_open.push_back((String::from($path), $result)); )*
            });
        }};
        (std::fs::File::as_raw_fd: $( { _ => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().file_as_raw_fd.push_back($result); )*
            })
        }};
        (std::fs::File::set_len: $( { $len:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().file_set_len.push_back(($len, $result)); )*
            })
        }};
        (std::fs::remove_dir: $( { $path:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().remove_dir.push_back((String::from($path), $result)); )*
            });
        }};
        (nix::mount::mount: $(
            { $source:expr, $target:expr, $fstype:expr, $flags:expr, $data:expr => $result:expr }
        ),+ $(,)? ) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().mount.push_back((
                    (
                        String::from($source),
                        String::from($target),
                        String::from($fstype),
                        $flags,
                        String::from($data),
                    ), (
                        $result
                    )
                )); )*
            });
        }};
        (std::fs::OpenOptions::new: $( { _ => $options:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().openoptions_new.push_back($options); )*
            })
        }};
        (std::fs::OpenOptions::open: $( { $path:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().openoptions_open.push_back((String::from($path), $result)); )*
            })
        }};
        (nix::fcntl::flock: $( { $fd:expr, $lock:expr => $result:expr } ),+ $(,)?) => {{
            TEST_EXPECTATIONS.with(|expectations| {
                $( expectations.borrow_mut().flock.push_back((($fd, $lock), $result)); )*
            })
        }};
    }

    macro_rules! verify_expectations {
        () => {
            TEST_EXPECTATIONS.with(|expectations| {
                let expectations = expectations.borrow();
                let len = expectations.write.len();
                if len > 0 {
                    panic!("{} more std::fs::write() call(s) expected.", len);
                }

                let len = expectations.read_to_string.len();
                if len > 0 {
                    panic!("{} more std::fs::read_to_string() call(s) expected.", len);
                }

                let len = expectations.create_dir_all.len();
                if len > 0 {
                    panic!("{} more std::fs::create_dir_all() call(s) expected.", len);
                }

                let len = expectations.mount.len();
                if len > 0 {
                    panic!("{} more nix::mount::mount() call(s) expected.", len);
                }

                let len = expectations.remove_dir.len();
                if len > 0 {
                    panic!("{} more std::fs::remove_dir() call(s) expected.", len);
                }

                let len = expectations.source_mounted_at.len();
                if len > 0 {
                    panic!(
                        "{} more MountIter::source_mounted_at() call(s) expected.",
                        len
                    );
                }

                let len = expectations.file_open.len();
                if len > 0 {
                    panic!("{} more std::fs::File::open() call(s) expected.", len);
                }

                let len = expectations.openoptions_new.len();
                if len > 0 {
                    panic!("{} more std::fs::OpenOptions::new() call(s) expected.", len);
                }

                let len = expectations.file_as_raw_fd.len();
                if len > 0 {
                    panic!("{} more std::fs::File::as_raw_fd() call(s) expected.", len);
                }

                let len = expectations.file_set_len.len();
                if len > 0 {
                    panic!("{} more std::fs::File::set_len() call(s) expected.", len);
                }

                let len = expectations.openoptions_open.len();
                if len > 0 {
                    panic!(
                        "{} more std::fs::OpenOptions::open() calls(s) expected",
                        len
                    );
                }

                let len = expectations.flock.len();
                if len > 0 {
                    panic!("{} more nix::fcntl::flock() calls(s) expected", len);
                }
            })
        };
    }

    pub mod std {
        pub mod fs {
            use super::super::TEST_EXPECTATIONS;
            use ::std::{
                collections::VecDeque,
                io::{Error, Read, Result, Seek, SeekFrom},
                os::unix::io::{AsRawFd, RawFd},
                path::Path,
                str::from_utf8,
            };

            pub struct OpenOptions {
                pub read: VecDeque<bool>,
                pub write: VecDeque<bool>,
            }

            impl OpenOptions {
                pub fn new() -> Self {
                    match TEST_EXPECTATIONS
                        .with(|expectations| expectations.borrow_mut().openoptions_new.pop_front())
                    {
                        Some(r) => r,
                        None => panic!("Unexpected call to std::fs::OpenOptions::new()"),
                    }
                }

                pub fn read(&mut self, read: bool) -> &mut Self {
                    match self.read.pop_front() {
                        Some(v) => {
                            if v != read {
                                panic!(
                                    "Unexpected call to std::fs::OpenOptions::read({:?}), \
                                    expected std::fs::OpenOptions::read({:?})",
                                    read, v
                                );
                            }
                        }
                        None => panic!("Unexpected call to std::fs::OpenOptions::read({:?})", read),
                    };

                    self
                }

                pub fn write(&mut self, write: bool) -> &mut Self {
                    match self.write.pop_front() {
                        Some(v) => {
                            if v != write {
                                panic!(
                                    "Unexpected call to std::fs::OpenOptions::write({:?}), \
                                    expected std::fs::OpenOptions::write({:?})",
                                    write, v
                                );
                            }
                        }
                        None => panic!(
                            "Unexpected call to std::fs::OpenOptions::write({:?})",
                            write
                        ),
                    };

                    self
                }

                pub fn open<P: AsRef<Path>>(&self, path: P) -> Result<File> {
                    let path = path.as_ref().to_str().unwrap();

                    let (argument, result) = match TEST_EXPECTATIONS
                        .with(|expectations| expectations.borrow_mut().openoptions_open.pop_front())
                    {
                        Some(s) => s,
                        None => panic!("Unexpected call to std::fs::OpenOptions::open({:?})", path),
                    };

                    if path != argument {
                        panic!(
                            "Unexpected call to std::fs::OpenOptions::open({:?}), \
                            expected std::fs::OpenOptions::open({:?})",
                            path, argument
                        );
                    }

                    result
                }
            }

            pub fn remove_dir<P: AsRef<Path>>(path: P) -> Result<()> {
                let path = path.as_ref().to_str().unwrap();

                let (argument, result) = match TEST_EXPECTATIONS
                    .with(|expectations| expectations.borrow_mut().remove_dir.pop_front())
                {
                    Some(arg) => arg,
                    None => panic!("Unexpected call to std::fs::remove_dir({:?})", path),
                };

                if argument != path.to_string() {
                    panic!(
                        "Unexpected call to std::fs::remove_dir({:?}), \
                            expected: std::fs::remove_dir({:?})",
                        path, argument
                    );
                }

                result
            }

            pub fn create_dir_all<P: AsRef<Path>>(path: P) -> Result<()> {
                let path = path.as_ref().to_str().unwrap();

                let (argument, result) = match TEST_EXPECTATIONS
                    .with(|expectations| expectations.borrow_mut().create_dir_all.pop_front())
                {
                    Some(arg) => arg,
                    None => panic!("Unexpected call to std::fs::create_dir_all({:?})", path),
                };

                if argument != path.to_string() {
                    panic!(
                        "Unexpected call to std::fs::create_dir_all({:?}), \
                            expected: std::fs::create_dir_all({:?})",
                        path, argument
                    );
                }

                result
            }

            pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, data: C) -> Result<()> {
                let path = path.as_ref().to_str().unwrap();
                let data = from_utf8(data.as_ref()).unwrap();

                let (arguments, result) = match TEST_EXPECTATIONS
                    .with(|expectations| expectations.borrow_mut().write.pop_front())
                {
                    Some(args) => args,
                    None => panic!("Unexpected call to std::fs::write({:?}, {:?})", path, data),
                };

                if arguments != (path.to_string(), data.to_string()) {
                    panic!(
                        "Unexpected call to std::fs::write({:?}, {:?}), expected: std::fs::write({:?}, {:?})",
                        path, data, arguments.0, arguments.1
                    );
                }

                result
            }

            pub fn read_to_string<P: AsRef<Path>>(path: P) -> Result<String> {
                let path = path.as_ref().to_str().unwrap();

                let (argument, result) = match TEST_EXPECTATIONS
                    .with(|expectations| expectations.borrow_mut().read_to_string.pop_front())
                {
                    Some(arg) => arg,
                    None => panic!("Unexpected call to std::fs::read_to_string({:?})", path),
                };

                if argument != path.to_string() {
                    panic!(
                        "Unexpected call to std::fs::read_to_string({:?}), \
                            expected: std::fs::read_to_string({:?})",
                        path, argument
                    );
                }

                result
            }

            pub struct File {
                pub lock: VecDeque<Result<()>>,
                pub trim: VecDeque<Result<()>>,
                pub read: VecDeque<Result<String>>,
                pub read_string: VecDeque<Result<String>>,
                pub seek: VecDeque<(SeekFrom, Result<u64>)>,
                pub write: VecDeque<(String, Option<Error>)>,
            }

            impl File {
                pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
                    let path = path.as_ref().to_str().unwrap();

                    let (argument, result) = match TEST_EXPECTATIONS
                        .with(|expectations| expectations.borrow_mut().file_open.pop_front())
                    {
                        Some(arg) => arg,
                        None => panic!("Unexpected call to std::fs::File::open({:?})", path),
                    };

                    if argument != path.to_string() {
                        panic!(
                            "Unexpected call to std::fs::File::open({:?}), \
                            expected: std::fs::File::open({:?})",
                            path, argument
                        );
                    }

                    result
                }

                pub fn lock(&mut self) -> Result<()> {
                    match self.lock.pop_front() {
                        Some(r) => r,
                        None => panic!("Unexpected call to std::fs::File::lock()"),
                    }
                }

                pub fn trim(&mut self) -> Result<()> {
                    match self.trim.pop_front() {
                        Some(r) => r,
                        None => panic!("Unexpected call to std::fs::File::trim()"),
                    }
                }

                pub fn read_string(&mut self) -> Result<String> {
                    match self.read_string.pop_front() {
                        Some(r) => r,
                        None => panic!("Unexpected call to std::fs::File::read_string()"),
                    }
                }

                pub fn write(&mut self, buf: &[u8]) -> Result<usize> {
                    let buf = from_utf8(buf.as_ref()).unwrap();

                    let (argument, result) = match self.write.pop_front() {
                        Some(arg) => arg,
                        None => panic!("Unexpected call to File::write({:?})", buf),
                    };

                    if argument != buf.to_string() {
                        panic!(
                            "Unexpected call to std::fs::File::write({:?}), \
                            expected: std::fs::File::write({:?})",
                            buf, argument
                        );
                    }

                    match result {
                        Some(e) => Err(e),
                        None => Ok(buf.len()),
                    }
                }

                pub fn set_len(&self, size: u64) -> Result<()> {
                    let (len, result) = match TEST_EXPECTATIONS
                        .with(|expectations| expectations.borrow_mut().file_set_len.pop_front())
                    {
                        Some(set_len) => set_len,
                        None => panic!("Unexpected call to std::fs::File::set_len({:?})", size),
                    };

                    if len != size {
                        panic!(
                            "Unexpected call to std::fs::File::set_len({:?}), \
                            expected std::fs::File::set_len({:?})",
                            size, len
                        );
                    }

                    result
                }
            }

            impl AsRawFd for File {
                fn as_raw_fd(&self) -> RawFd {
                    match TEST_EXPECTATIONS
                        .with(|expectations| expectations.borrow_mut().file_as_raw_fd.pop_front())
                    {
                        Some(v) => v,
                        None => panic!("Unexpected call to std::fs::File::as_raw_fd()"),
                    }
                }
            }

            impl Seek for File {
                fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
                    let (argument, result) = match self.seek.pop_front() {
                        Some(set_len) => set_len,
                        None => panic!("Unexpected call to std::fs::File::seek({:?})", pos),
                    };

                    if argument != pos {
                        panic!(
                            "Unexpected call to std::fs::File::set_len({:?}), \
                            expected: std::fs::File::set_len({:?})",
                            pos, argument
                        );
                    }

                    result
                }
            }

            impl Read for File {
                fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
                    match self.read.pop_front() {
                        Some(result) => match result {
                            Ok(r) => r.as_bytes().read(buf),
                            Err(e) => Err(e),
                        },
                        None => panic!("Unexpected call to File::read()"),
                    }
                }
            }

            impl Drop for File {
                fn drop(&mut self) {
                    if ::std::thread::panicking() {
                        return;
                    }

                    let len = self.read.len();
                    if len > 0 {
                        panic!("{} more call(s) expected for File::read()", len)
                    }

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

                    let len = self.seek.len();
                    if len > 0 {
                        panic!("{} more call(s) expected for File::seek()", len)
                    }
                }
            }
        }
    }

    pub mod proc_mounts {
        use super::TEST_EXPECTATIONS;
        use std::{
            io::{BufReader, Result},
            path::Path,
        };

        pub struct MountIter<T> {
            _inner: T,
        }

        impl MountIter<BufReader<super::std::fs::File>> {
            pub fn source_mounted_at<D: AsRef<Path>, P: AsRef<Path>>(
                source: D,
                path: P,
            ) -> Result<bool> {
                let path = path.as_ref().to_str().unwrap();
                let source = source.as_ref().to_str().unwrap();

                let (argument, result) = match TEST_EXPECTATIONS
                    .with(|expectations| expectations.borrow_mut().source_mounted_at.pop_front())
                {
                    Some(arg) => arg,
                    None => panic!(
                        "Unexpected call to MountIter::source_mounted_at({:?}, {:?})",
                        source, path
                    ),
                };

                if "cgroup" != source || argument != path.to_string() {
                    panic!(
                        "Unexpected call to MountIter::source_mounted_at({:?}, {:?}), \
                        expected MountIter::source_mounted_at(\"cgroup\", {:?})",
                        source, path, argument
                    );
                }

                result
            }
        }
    }

    pub mod nix {
        pub mod fcntl {
            use super::super::TEST_EXPECTATIONS;
            use ::nix::{fcntl::FlockArg, Result};
            use ::std::os::unix::io::RawFd;

            pub fn flock(fd: RawFd, arg: FlockArg) -> Result<()> {
                let (arguments, result) = match TEST_EXPECTATIONS
                    .with(|expectations| expectations.borrow_mut().flock.pop_front())
                {
                    Some(arg) => arg,
                    None => panic!("Unexpected call to nix::fcntl::flock({:?}, {:?})", fd, arg),
                };

                if fd != arguments.0 || arg != arguments.1 {
                    panic!(
                        "Unexpected call to nix::fcntl::flock({:?}, {:?}), \
                        expected nix::fcntl::flock({:?}, {:?})",
                        fd, arg, arguments.0, arguments.1
                    );
                }

                result
            }
        }

        pub mod mount {
            use super::super::TEST_EXPECTATIONS;
            use ::nix::{mount::MsFlags, NixPath, Result};
            use ::std::str::from_utf8;

            pub fn mount<
                P1: ?Sized + NixPath,
                P2: ?Sized + NixPath,
                P3: ?Sized + NixPath,
                P4: ?Sized + NixPath,
            >(
                source: Option<&P1>,
                target: &P2,
                fstype: Option<&P3>,
                flags: MsFlags,
                data: Option<&P4>,
            ) -> Result<()> {
                source.with_nix_path(|source| {
                    target.with_nix_path(|target| {
                        fstype.with_nix_path(|fstype| {
                            data.with_nix_path(|data| {
                                let source = from_utf8(source.to_bytes()).unwrap();
                                let target = from_utf8(target.to_bytes()).unwrap();
                                let fstype = from_utf8(fstype.to_bytes()).unwrap();
                                let data = from_utf8(data.to_bytes()).unwrap();


                                let (arguments, result) = match TEST_EXPECTATIONS
                                    .with(|expectations| expectations.borrow_mut().mount.pop_front()) {
                                        Some(arg) => arg,
                                        None => panic!(
                                            "Unexpected call to nix::mount::mount({:?}, {:?}, {:?}, {:?}, {:?})",
                                            source, target, fstype, flags, data
                                        ),
                                    };

                                let (
                                    expected_source,
                                    expected_target,
                                    expected_fstype,
                                    expected_flags,
                                    expected_data
                                ) = arguments;

                                if expected_source != source ||
                                    expected_target != target ||
                                    expected_fstype != fstype ||
                                    expected_flags != flags ||
                                    expected_data != data {
                                        panic!(
                                            "Unexpected call to nix::mount::mount({:?}, {:?}, {:?}, {:?}, {:?}),\
                                            expected nix::mount::mount({:?}, {:?}, {:?}, {:?}, {:?})",
                                            source, target, fstype, flags, data,
                                            expected_source, expected_target, expected_fstype, expected_flags,
                                            expected_data
                                        );
                                    }

                                result
                            })
                        })
                    })
                })????
            }
        }
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

        expect!(std::fs::create_dir_all: { "/test1/cgroups/cpuset" => error!("std::fs::create_dir_all(1)") });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `1` - std::fs::create_dir_all(1)",
            cpuset.pin_task(1, 32001)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_check_cpuset_cgroug_mount_status() {
        let mut cpuset = CpuSet::new("/test2/cgroups/cpuset", "prefix2").unwrap();

        expect!(std::fs::create_dir_all: { "/test2/cgroups/cpuset" => Ok({}) });
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

        expect!(std::fs::create_dir_all: { "/test3/cgroups/cpuset" => Ok({}) });
        expect!(source_mounted_at: { "/test3/cgroups/cpuset" => Ok(false) });
        expect!(nix::mount::mount: {
            "cgroup", "/test3/cgroups/cpuset", "cgroup", MsFlags::empty(), "cpuset" =>
                Err(::nix::Error::InvalidPath)
        });

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
            std::fs::create_dir_all:
            { "/test4/cgroups/cpuset" => Ok({}) },
            { "/test4/cgroups/cpuset/prefix4" => error!("std::fs::create_dir_all(4)") },
        );
        expect!(source_mounted_at: { "/test4/cgroups/cpuset" => Ok(false) });
        expect!(nix::mount::mount:
            { "cgroup", "/test4/cgroups/cpuset", "cgroup", MsFlags::empty(), "cpuset" => Ok({}) }
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `4` - std::fs::create_dir_all(4)",
            cpuset.pin_task(4, 32004)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_make_cpuset_cpu_exclusive() {
        let mut cpuset = CpuSet::new("/test5/cgroups/cpuset", "prefix5").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test5/cgroups/cpuset" => Ok({}) },
            { "/test5/cgroups/cpuset/prefix5" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test5/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test5/cgroups/cpuset/prefix5/cpuset.cpu_exclusive", "1" => error!("std::fs::write(5)") }
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `5` - std::fs::write(5)",
            cpuset.pin_task(5, 32005)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_prefix_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test6/cgroups/cpuset", "prefix6").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test6/cgroups/cpuset" => Ok({}) },
            { "/test6/cgroups/cpuset/prefix6" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test6/cgroups/cpuset" => Ok(true) });
        expect!(std::fs::write: { "/test6/cgroups/cpuset/prefix6/cpuset.cpu_exclusive", "1" => Ok({}) });
        expect!(
            std::fs::read_to_string:
            {"/test6/cgroups/cpuset/prefix6/cpuset.mems" => error!("std::fs::read_to_string(6)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `6` - std::fs::read_to_string(6)",
            cpuset.pin_task(6, 32006)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test7/cgroups/cpuset", "prefix7").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test7/cgroups/cpuset" => Ok({}) },
            { "/test7/cgroups/cpuset/prefix7" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test7/cgroups/cpuset" => Ok(true) });
        expect!(std::fs::write: { "/test7/cgroups/cpuset/prefix7/cpuset.cpu_exclusive", "1" => Ok({}) });
        expect!(
            std::fs::read_to_string:
            { "/test7/cgroups/cpuset/prefix7/cpuset.mems" => Ok(String::new()) },
            { "/test7/cgroups/cpuset/cpuset.mems" => error!("std::fs::read_to_string(7)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `7` - std::fs::read_to_string(7)",
            cpuset.pin_task(7, 32007)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_prefix_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test8/cgroups/cpuset", "prefix8").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test8/cgroups/cpuset" => Ok({}) },
            { "/test8/cgroups/cpuset/prefix8" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test8/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test8/cgroups/cpuset/prefix8/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test8/cgroups/cpuset/prefix8/cpuset.mems", "8" => error!("std::fs::write(8)") },
        );
        expect!(
            std::fs::read_to_string:
            { "/test8/cgroups/cpuset/prefix8/cpuset.mems" => Ok(String::new()) },
            { "/test8/cgroups/cpuset/cpuset.mems" => Ok("8".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `8` - std::fs::write(8)",
            cpuset.pin_task(8, 32008)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_prefix_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test9/cgroups/cpuset", "prefix9").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test9/cgroups/cpuset" => Ok({}) },
            { "/test9/cgroups/cpuset/prefix9" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test9/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test9/cgroups/cpuset/prefix9/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test9/cgroups/cpuset/prefix9/cpuset.mems", "9" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test9/cgroups/cpuset/prefix9/cpuset.mems" => Ok(String::new()) },
            { "/test9/cgroups/cpuset/cpuset.mems" => Ok("9".to_string()) },
            { "/test9/cgroups/cpuset/prefix9/cpuset.cpus" => error!("std::fs::read_to_string(9)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `9` - std::fs::read_to_string(9)",
            cpuset.pin_task(9, 32009)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test10/cgroups/cpuset", "prefix10").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test10/cgroups/cpuset" => Ok({}) },
            { "/test10/cgroups/cpuset/prefix10" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test10/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test10/cgroups/cpuset/prefix10/cpuset.cpu_exclusive", "1" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test10/cgroups/cpuset/prefix10/cpuset.mems" => Ok("10".to_string()) },
            { "/test10/cgroups/cpuset/prefix10/cpuset.cpus" => Ok(String::new()) },
            { "/test10/cgroups/cpuset/cpuset.cpus" => error!("std::fs::read_to_string(10)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `10` - std::fs::read_to_string(10)",
            cpuset.pin_task(10, 32010)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_prefix_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test11/cgroups/cpuset", "prefix11").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test11/cgroups/cpuset" => Ok({}) },
            { "/test11/cgroups/cpuset/prefix11" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test11/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test11/cgroups/cpuset/prefix11/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test11/cgroups/cpuset/prefix11/cpuset.cpus", "11" => error!("std::fs::write(11)") },
        );
        expect!(
            std::fs::read_to_string:
            { "/test11/cgroups/cpuset/prefix11/cpuset.mems" => Ok("11".to_string()) },
            { "/test11/cgroups/cpuset/prefix11/cpuset.cpus" => Ok(String::new()) },
            { "/test11/cgroups/cpuset/cpuset.cpus" => Ok("11".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `11` - std::fs::write(11)",
            cpuset.pin_task(11, 32011)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_cpu_pool_directory() {
        let mut cpuset = CpuSet::new("/test12/cgroups/cpuset", "prefix12").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test12/cgroups/cpuset" => Ok({}) },
            { "/test12/cgroups/cpuset/prefix12" => Ok({}) },
            { "/test12/cgroups/cpuset/prefix12/pool" => error!("std::fs::create_dir_all(12)") },
        );
        expect!(source_mounted_at: { "/test12/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test12/cgroups/cpuset/prefix12/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test12/cgroups/cpuset/prefix12/cpuset.cpus", "0-12" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test12/cgroups/cpuset/prefix12/cpuset.mems" => Ok("12".to_string()) },
            { "/test12/cgroups/cpuset/prefix12/cpuset.cpus" => Ok(String::new()) },
            { "/test12/cgroups/cpuset/cpuset.cpus" => Ok("0-12".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `12` - std::fs::create_dir_all(12)",
            cpuset.pin_task(12, 32012)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_make_cpuset_pool_cpu_exclusive() {
        let mut cpuset = CpuSet::new("/test13/cgroups/cpuset", "prefix13").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test13/cgroups/cpuset" => Ok({}) },
            { "/test13/cgroups/cpuset/prefix13" => Ok({}) },
            { "/test13/cgroups/cpuset/prefix13/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test13/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test13/cgroups/cpuset/prefix13/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test13/cgroups/cpuset/prefix13/pool/cpuset.cpu_exclusive", "1" => error!("std::fs::write(13)") },
        );
        expect!(
            std::fs::read_to_string:
            { "/test13/cgroups/cpuset/prefix13/cpuset.mems" => Ok("13".to_string()) },
            { "/test13/cgroups/cpuset/prefix13/cpuset.cpus" => Ok("0-13".to_string()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `13` - std::fs::write(13)",
            cpuset.pin_task(13, 32013)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_mems() {
        let mut cpuset = CpuSet::new("/test14/cgroups/cpuset", "prefix14").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test14/cgroups/cpuset" => Ok({}) },
            { "/test14/cgroups/cpuset/prefix14" => Ok({}) },
            { "/test14/cgroups/cpuset/prefix14/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test14/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test14/cgroups/cpuset/prefix14/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test14/cgroups/cpuset/prefix14/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test14/cgroups/cpuset/prefix14/cpuset.mems" => Ok("14".to_string()) },
            { "/test14/cgroups/cpuset/prefix14/cpuset.cpus" => Ok("0-14".to_string()) },
            { "/test14/cgroups/cpuset/prefix14/pool/cpuset.mems" => error!("std::fs::read_to_string(14)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `14` - std::fs::read_to_string(14)",
            cpuset.pin_task(14, 32014)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_mems() {
        let mut cpuset = CpuSet::new("/test15/cgroups/cpuset", "prefix15").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test15/cgroups/cpuset" => Ok({}) },
            { "/test15/cgroups/cpuset/prefix15" => Ok({}) },
            { "/test15/cgroups/cpuset/prefix15/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test15/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test15/cgroups/cpuset/prefix15/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test15/cgroups/cpuset/prefix15/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test15/cgroups/cpuset/prefix15/pool/cpuset.mems", "15" => error!("std::fs::write(15)") },
        );
        expect!(
            std::fs::read_to_string:
            { "/test15/cgroups/cpuset/prefix15/cpuset.mems" => Ok("15".to_string()) },
            { "/test15/cgroups/cpuset/prefix15/cpuset.cpus" => Ok("0-15".to_string()) },
            { "/test15/cgroups/cpuset/prefix15/pool/cpuset.mems" => Ok(String::new()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `15` - std::fs::write(15)",
            cpuset.pin_task(15, 32015)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_cpus() {
        let mut cpuset = CpuSet::new("/test16/cgroups/cpuset", "prefix16").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test16/cgroups/cpuset" => Ok({}) },
            { "/test16/cgroups/cpuset/prefix16" => Ok({}) },
            { "/test16/cgroups/cpuset/prefix16/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test16/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test16/cgroups/cpuset/prefix16/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test16/cgroups/cpuset/prefix16/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test16/cgroups/cpuset/prefix16/pool/cpuset.mems", "16" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test16/cgroups/cpuset/prefix16/cpuset.mems" => Ok("16".to_string()) },
            { "/test16/cgroups/cpuset/prefix16/cpuset.cpus" => Ok("0-16".to_string()) },
            { "/test16/cgroups/cpuset/prefix16/pool/cpuset.mems" => Ok(String::new()) },
            { "/test16/cgroups/cpuset/prefix16/pool/cpuset.cpus" => error!("std::fs::read_to_string(16)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `16` - std::fs::read_to_string(16)",
            cpuset.pin_task(16, 32016)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_cpus() {
        let mut cpuset = CpuSet::new("/test17/cgroups/cpuset", "prefix17").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test17/cgroups/cpuset" => Ok({}) },
            { "/test17/cgroups/cpuset/prefix17" => Ok({}) },
            { "/test17/cgroups/cpuset/prefix17/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test17/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test17/cgroups/cpuset/prefix17/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test17/cgroups/cpuset/prefix17/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test17/cgroups/cpuset/prefix17/pool/cpuset.cpus", "0-17" => error!("std::fs::write(17)") },
        );
        expect!(
            std::fs::read_to_string:
            { "/test17/cgroups/cpuset/prefix17/cpuset.mems" => Ok("17".to_string()) },
            { "/test17/cgroups/cpuset/prefix17/cpuset.cpus" => Ok("0-17".to_string()) },
            { "/test17/cgroups/cpuset/prefix17/pool/cpuset.mems" => Ok("17".to_string()) },
            { "/test17/cgroups/cpuset/prefix17/pool/cpuset.cpus" => Ok(String::new()) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `17` - std::fs::write(17)",
            cpuset.pin_task(17, 32017)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_tasks() {
        let mut cpuset = CpuSet::new("/test18/cgroups/cpuset", "prefix18").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test18/cgroups/cpuset" => Ok({}) },
            { "/test18/cgroups/cpuset/prefix18" => Ok({}) },
            { "/test18/cgroups/cpuset/prefix18/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test18/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test18/cgroups/cpuset/prefix18/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpus", "0-18" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test18/cgroups/cpuset/prefix18/cpuset.mems" => Ok("18".to_string()) },
            { "/test18/cgroups/cpuset/prefix18/cpuset.cpus" => Ok("0-18".to_string()) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.mems" => Ok("18".to_string()) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpus" => Ok(String::new()) },
            { "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpus" => Ok("0-18".to_string()) },
            { "/test18/cgroups/cpuset/tasks" => error!("std::fs::read_to_string(18)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `18` - std::fs::read_to_string(18)",
            cpuset.pin_task(18, 32018)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_open_thread_tasks_file() {
        let mut cpuset = CpuSet::new("/test19/cgroups/cpuset", "prefix19").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test19/cgroups/cpuset" => Ok({}) },
            { "/test19/cgroups/cpuset/prefix19" => Ok({}) },
            { "/test19/cgroups/cpuset/prefix19/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test19/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test19/cgroups/cpuset/prefix19/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test19/cgroups/cpuset/prefix19/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test19/cgroups/cpuset/prefix19/pool/tasks", "1019" => error!("std::fs::write(19)") },
            { "/test19/cgroups/cpuset/prefix19/pool/tasks", "1119" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test19/cgroups/cpuset/prefix19/cpuset.mems" => Ok("19\n".to_string()) },
            { "/test19/cgroups/cpuset/prefix19/cpuset.cpus" => Ok("0-19\n".to_string()) },
            { "/test19/cgroups/cpuset/prefix19/pool/cpuset.mems" => Ok("19\n".to_string()) },
            { "/test19/cgroups/cpuset/prefix19/pool/cpuset.cpus" => Ok("0-19\n".to_string()) },
            { "/test19/cgroups/cpuset/prefix19/pool/cpuset.cpus" => Ok("0-19\n".to_string()) },
            { "/test19/cgroups/cpuset/tasks" => Ok("1019\n1119\n1219\n1319\n".to_string()) },
            { "/proc/1019/status" => Ok("Cpus_allowed_list:\t0-19\n".to_string()) },
            { "/proc/1119/status" => Ok(
                "Name:\ttest\nNSsid:\t0\nCpus_allowed:\t0000800\nCpus_allowed_list:\t0-19\n".to_string()
            ) },
            { "/proc/1219/status" => Ok("Cpus_allowed_list:\t11\n".to_string()) },
            { "/proc/1319/status" => error!("std::fs::read_to_string(1319)".to_string()) },
        );
        expect!(
            std::fs::File::open:
            { "/test19/cgroups/cpuset/prefix19/19/tasks" => error!("File::open(19)") },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `19` - File::open(19)",
            cpuset.pin_task(19, 32019)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_thread_tasks_file() {
        let mut cpuset = CpuSet::new("/test40/cgroups/cpuset", "prefix40").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test40/cgroups/cpuset" => Ok({}) },
            { "/test40/cgroups/cpuset/prefix40" => Ok({}) },
            { "/test40/cgroups/cpuset/prefix40/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test40/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test40/cgroups/cpuset/prefix40/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test40/cgroups/cpuset/prefix40/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test40/cgroups/cpuset/prefix40/pool/tasks", "1040" => Ok({}) },
            { "/test40/cgroups/cpuset/prefix40/pool/tasks", "1140" => Ok({}) },
            { "/test40/cgroups/cpuset/prefix40/pool/tasks", "1240" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test40/cgroups/cpuset/prefix40/cpuset.mems" => Ok("40\n".to_string()) },
            { "/test40/cgroups/cpuset/prefix40/cpuset.cpus" => Ok("35-40\n".to_string()) },
            { "/test40/cgroups/cpuset/prefix40/pool/cpuset.mems" => Ok("40\n".to_string()) },
            { "/test40/cgroups/cpuset/prefix40/pool/cpuset.cpus" => Ok("35-40\n".to_string()) },
            { "/test40/cgroups/cpuset/prefix40/pool/cpuset.cpus" => Ok("35-40\n".to_string()) },
            { "/test40/cgroups/cpuset/tasks" => Ok("1040\n1140\n1240\n1340\n".to_string()) },
            { "/proc/1040/status" => Ok("Cpus_allowed_list:\t35-40\n".to_string()) },
            { "/proc/1140/status" => Ok("Cpus_allowed_list:\t35-40\n".to_string()) },
            { "/proc/1240/status" => Ok("Cpus_allowed_list:\t35-40\n".to_string()) },
            { "/proc/1340/status" => Ok("Cpus_allowed_list:\t35\n".to_string()) },
        );
        expect!(
            std::fs::File::open: { "/test40/cgroups/cpuset/prefix40/40/tasks" => Ok(std::fs::File {
                read: vec_deq![error!("File::read(40)")],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `40` - File::read(40)",
            cpuset.pin_task(40, 32040)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_open_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test20/cgroups/cpuset", "prefix20").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test20/cgroups/cpuset" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test20/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test20/cgroups/cpuset/prefix20/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20/pool/tasks", "1020" => Ok({}) },
            { "/test20/cgroups/cpuset/prefix20/pool/tasks", "2020" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open: { "/test20/cgroups/cpuset/prefix20/20/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok("1020\n".to_string())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test20/cgroups/cpuset/prefix20/pool/cpuset.cpus" => error!("std::fs::OpenOptions::new(20)") }
        );

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `20` - std::fs::OpenOptions::new(20)",
            cpuset.pin_task(20, 32020)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_lock_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test21/cgroups/cpuset", "prefix21").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test21/cgroups/cpuset" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test21/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test21/cgroups/cpuset/prefix21/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool/tasks", "1121" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool/tasks", "2121" => Ok({}) },
            { "/test21/cgroups/cpuset/prefix21/pool/tasks", "3121" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test21/cgroups/cpuset/prefix21/21/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) }
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } }
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test21/cgroups/cpuset/prefix21/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 21 });
        expect!(nix::fcntl::flock: { 21, FlockArg::LockExclusive => Err(::nix::Error::InvalidPath) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `21` - Failed to lock \
            `/test21/cgroups/cpuset/prefix21/pool/cpuset.cpus`: Invalid path",
            cpuset.pin_task(21, 32021)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test22/cgroups/cpuset", "prefix22").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test22/cgroups/cpuset" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test22/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test22/cgroups/cpuset/prefix22/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22/pool/tasks", "1222" => Ok({}) },
            { "/test22/cgroups/cpuset/prefix22/pool/tasks", "2222" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test22/cgroups/cpuset/prefix22/22/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(22)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test22/cgroups/cpuset/prefix22/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![error!("std::fs::File::read(22)")],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 22 });
        expect!(nix::fcntl::flock: { 22, FlockArg::LockExclusive => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `22` - std::fs::File::read(22)",
            cpuset.pin_task(22, 32022)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_seek_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test23/cgroups/cpuset", "prefix23").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test23/cgroups/cpuset" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test23/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test23/cgroups/cpuset/prefix23/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool/tasks", "1323" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool/tasks", "2323" => Ok({}) },
            { "/test23/cgroups/cpuset/prefix23/pool/tasks", "3323" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test23/cgroups/cpuset/prefix23/23/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(23)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test23/cgroups/cpuset/prefix23/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("0-23".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), error!("std::fs::File::seek(23)"))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 22 });
        expect!(nix::fcntl::flock: { 22, FlockArg::LockExclusive => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `23` - std::fs::File::seek(23)",
            cpuset.pin_task(23, 32023)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_set_len_on_cpuset_pool_cpus_file_to_isolate_thread(
    ) {
        let mut cpuset = CpuSet::new("/test41/cgroups/cpuset", "prefix41").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test41/cgroups/cpuset" => Ok({}) },
            { "/test41/cgroups/cpuset/prefix41" => Ok({}) },
            { "/test41/cgroups/cpuset/prefix41/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test41/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test41/cgroups/cpuset/prefix41/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test41/cgroups/cpuset/prefix41/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test41/cgroups/cpuset/prefix41/pool/tasks", "1341" => Ok({}) },
            { "/test41/cgroups/cpuset/prefix41/pool/tasks", "2341" => Ok({}) },
            { "/test41/cgroups/cpuset/prefix41/pool/tasks", "3341" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test41/cgroups/cpuset/prefix41/cpuset.mems" => Ok("41\n".to_string()) },
            { "/test41/cgroups/cpuset/prefix41/cpuset.cpus" => Ok("0-41\n".to_string()) },
            { "/test41/cgroups/cpuset/prefix41/pool/cpuset.mems" => Ok("41\n".to_string()) },
            { "/test41/cgroups/cpuset/prefix41/pool/cpuset.cpus" => Ok("0-41\n".to_string()) },
            { "/test41/cgroups/cpuset/prefix41/pool/cpuset.cpus" => Ok("0-41\n".to_string()) },
            { "/test41/cgroups/cpuset/tasks" => Ok("1341\n2341\n3341\n".to_string()) },
            { "/proc/1341/status" => Ok("Cpus_allowed_list:\t0-41\n".to_string()) },
            { "/proc/2341/status" => Ok("Cpus_allowed_list:\t0-41\n".to_string()) },
            { "/proc/3341/status" => Ok("Cpus_allowed_list:\t0-41\n".to_string()) },
        );
        expect!(
            std::fs::File::open:
            { "/test41/cgroups/cpuset/prefix41/41/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(41)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test41/cgroups/cpuset/prefix41/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("0-41".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 41 });
        expect!(nix::fcntl::flock: { 41, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => error!("std::fs::File::set_len(41)") });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `41` - std::fs::File::set_len(41)",
            cpuset.pin_task(41, 32041)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_cpus_file_with_isolated_thread()
    {
        let mut cpuset = CpuSet::new("/test24/cgroups/cpuset", "prefix24").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test24/cgroups/cpuset" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test24/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test24/cgroups/cpuset/prefix24/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool/tasks", "1024" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool/tasks", "2024" => Ok({}) },
            { "/test24/cgroups/cpuset/prefix24/pool/tasks", "3024" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test24/cgroups/cpuset/prefix24/24/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(24)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test24/cgroups/cpuset/prefix24/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("21-24".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![
                    ("21,22,23".to_string(), Some(Error::new(ErrorKind::Other, "std::fs::File::write(24)")))
                ],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 24 });
        expect!(nix::fcntl::flock: { 24, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `24` - std::fs::File::write(24)",
            cpuset.pin_task(24, 32024)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_a_cpuset_directory_for_isolated_thread() {
        let mut cpuset = CpuSet::new("/test25/cgroups/cpuset", "prefix25").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test25/cgroups/cpuset" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/25" => error!("std::fs::create_dir_all(25)") },
        );
        expect!(source_mounted_at: { "/test25/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test25/cgroups/cpuset/prefix25/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool/tasks", "2025" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool/tasks", "3025" => Ok({}) },
            { "/test25/cgroups/cpuset/prefix25/pool/tasks", "4025" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test25/cgroups/cpuset/prefix25/25/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(25)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test25/cgroups/cpuset/prefix25/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("21-25".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("21,22,23,24".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 25 });
        expect!(nix::fcntl::flock: { 25, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `25` - std::fs::create_dir_all(25)",
            cpuset.pin_task(25, 32025)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_mems_for_isloated_thread() {
        let mut cpuset = CpuSet::new("/test26/cgroups/cpuset", "prefix26").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test26/cgroups/cpuset" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/26" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test26/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test26/cgroups/cpuset/prefix26/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool/tasks", "2026" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool/tasks", "3026" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/pool/tasks", "4026" => Ok({}) },
            { "/test26/cgroups/cpuset/prefix26/26/cpuset.mems", "26" => error!("std::fs::write(26)") },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test26/cgroups/cpuset/prefix26/26/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(26)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test26/cgroups/cpuset/prefix26/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("23-26".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("23,24,25".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 26 });
        expect!(nix::fcntl::flock: { 26, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `26` - std::fs::write(26)",
            cpuset.pin_task(26, 32026)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_cpu_exclusive_for_isloated_thread() {
        let mut cpuset = CpuSet::new("/test27/cgroups/cpuset", "prefix27").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test27/cgroups/cpuset" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/27" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test27/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test27/cgroups/cpuset/prefix27/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool/tasks", "2027" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool/tasks", "3027" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/pool/tasks", "4027" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/27/cpuset.mems", "27" => Ok({}) },
            { "/test27/cgroups/cpuset/prefix27/27/cpuset.cpu_exclusive", "1" => error!("std::fs::write(27)") },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test27/cgroups/cpuset/prefix27/27/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(27)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test27/cgroups/cpuset/prefix27/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("23-27".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("23,24,25,26".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 27 });
        expect!(nix::fcntl::flock: { 27, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `27` - std::fs::write(27)",
            cpuset.pin_task(27, 32027)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_cpus_for_isolated_thread() {
        let mut cpuset = CpuSet::new("/test28/cgroups/cpuset", "prefix28").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test28/cgroups/cpuset" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/28" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test28/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test28/cgroups/cpuset/prefix28/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool/tasks", "2028" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool/tasks", "3028" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/pool/tasks", "4028" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/28/cpuset.mems", "28" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/28/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test28/cgroups/cpuset/prefix28/28/cpuset.cpus", "28" => error!("std::fs::write(28)") },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test28/cgroups/cpuset/prefix28/28/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(28)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test28/cgroups/cpuset/prefix28/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("23-28".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("23,24,25,26,27".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 28 });
        expect!(nix::fcntl::flock: { 28, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `28` - std::fs::write(28)",
            cpuset.pin_task(28, 32028)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_pin_task_to_isolated_thread() {
        let mut cpuset = CpuSet::new("/test29/cgroups/cpuset", "prefix29").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test29/cgroups/cpuset" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test29/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test29/cgroups/cpuset/prefix29/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool/tasks", "2029" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool/tasks", "3029" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/pool/tasks", "4029" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29/cpuset.mems", "29" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29/cpuset.cpus", "29" => Ok({}) },
            { "/test29/cgroups/cpuset/prefix29/29/tasks", "32029" => error!("std::fs::write(29)") },
        );
        expect!(
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test29/cgroups/cpuset/prefix29/29/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(29)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test29/cgroups/cpuset/prefix29/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("25-29".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("25,26,27,28".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 29 });
        expect!(nix::fcntl::flock: { 29, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to pin the process id `32029` to the host cpu thread `29` - std::fs::write(29)",
            cpuset.pin_task(29, 32029)
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_pin_task_isolates_the_thread_and_pins_the_task_to_it() {
        let mut cpuset = CpuSet::new("/test30/cgroups/cpuset", "prefix30").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test30/cgroups/cpuset" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/pool" => Ok({}) },
            { "/test30/cgroups/cpuset/prefix30/30" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test30/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test30/cgroups/cpuset/prefix30/30/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(30)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test30/cgroups/cpuset/prefix30/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("25-30".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("25,26,27,28,29".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 30 });
        expect!(nix::fcntl::flock: { 30, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        assert!(cpuset.pin_task(30, 3030).is_ok());

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_unable_to_open_pinned_thread_tasks_file() {
        let mut cpuset = CpuSet::new("/test31/cgroups/cpuset", "prefix31").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test31/cgroups/cpuset" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/pool" => Ok({}) },
            { "/test31/cgroups/cpuset/prefix31/31" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test31/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test31/cgroups/cpuset/prefix31/31/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(31)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test31/cgroups/cpuset/prefix31/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("30-31".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("30".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 31 });
        expect!(nix::fcntl::flock: { 31, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(31, 3031).unwrap();

        expect!(
            std::fs::File::open:
            { "/test31/cgroups/cpuset/prefix31/31/tasks" => error!("std::fs::File::open(31)") },
        );

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
            std::fs::create_dir_all:
            { "/test32/cgroups/cpuset" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/pool" => Ok({}) },
            { "/test32/cgroups/cpuset/prefix32/32" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test32/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test32/cgroups/cpuset/prefix32/32/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(32)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test32/cgroups/cpuset/prefix32/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("30-32".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("30,31".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 32 });
        expect!(nix::fcntl::flock: { 32, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(32, 2032).unwrap();

        expect!(
            std::fs::File::open:
            { "/test32/cgroups/cpuset/prefix32/32/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok("4032".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
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
    fn cpuset_release_threads_returns_error_if_unable_to_remove_thread_cpuset_cgroup_directory() {
        let mut cpuset = CpuSet::new("/test33/cgroups/cpuset", "prefix33").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test33/cgroups/cpuset" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/pool" => Ok({}) },
            { "/test33/cgroups/cpuset/prefix33/33" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test33/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test33/cgroups/cpuset/prefix33/33/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(33)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test33/cgroups/cpuset/prefix33/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("30-33".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("30,31,32".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 33 });
        expect!(nix::fcntl::flock: { 33, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(33, 2033).unwrap();

        expect!(
            std::fs::File::open:
            { "/test33/cgroups/cpuset/prefix33/33/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(
            std::fs::remove_dir:
            { "/test33/cgroups/cpuset/prefix33/33" => error!("std::fs::remove_dir(33)") }
        );

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
            std::fs::create_dir_all:
            { "/test34/cgroups/cpuset" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/pool" => Ok({}) },
            { "/test34/cgroups/cpuset/prefix34/34" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test34/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test34/cgroups/cpuset/prefix34/34/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(34)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("30-34".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("30,31,32,33".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 34 });
        expect!(nix::fcntl::flock: { 34, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(34, 2034).unwrap();

        expect!(
            std::fs::File::open:
            { "/test34/cgroups/cpuset/prefix34/34/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::remove_dir: { "/test34/cgroups/cpuset/prefix34/34" => Ok({}) });
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } }
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpus" => error!("File::open(34)") },
        );

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
            std::fs::create_dir_all:
            { "/test35/cgroups/cpuset" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/pool" => Ok({}) },
            { "/test35/cgroups/cpuset/prefix35/35" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test35/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test35/cgroups/cpuset/prefix35/35/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(35)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("30-35".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("30,31,32,33,34".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 35 });
        expect!(nix::fcntl::flock: { 35, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(35, 2035).unwrap();

        expect!(
            std::fs::File::open:
            { "/test35/cgroups/cpuset/prefix35/35/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::remove_dir: { "/test35/cgroups/cpuset/prefix35/35" => Ok({}) });
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } }
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            } ) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 3535 });
        expect!(nix::fcntl::flock: { 3535, FlockArg::LockExclusive => Err(::nix::Error::InvalidPath) });

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
            std::fs::create_dir_all:
            { "/test36/cgroups/cpuset" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/pool" => Ok({}) },
            { "/test36/cgroups/cpuset/prefix36/36" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test36/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test36/cgroups/cpuset/prefix36/36/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(36)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("32-36".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("32,33,34,35".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 36 });
        expect!(nix::fcntl::flock: { 36, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(36, 4036).unwrap();

        expect!(
            std::fs::File::open:
            { "/test36/cgroups/cpuset/prefix36/36/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::remove_dir: { "/test36/cgroups/cpuset/prefix36/36" => Ok({}) });
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } }
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![error!("std::fs::File::read(36)")],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            } ) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 3636 });
        expect!(nix::fcntl::flock: { 3636, FlockArg::LockExclusive => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_seek_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test37/cgroups/cpuset", "prefix37").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test37/cgroups/cpuset" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/pool" => Ok({}) },
            { "/test37/cgroups/cpuset/prefix37/37" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test37/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test37/cgroups/cpuset/prefix37/37/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(37)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("32-37".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("32,33,34,35,36".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 37 });
        expect!(nix::fcntl::flock: { 37, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(37, 4037).unwrap();

        expect!(
            std::fs::File::open:
            { "/test37/cgroups/cpuset/prefix37/37/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::remove_dir: { "/test37/cgroups/cpuset/prefix37/37" => Ok({}) });
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } }
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("32-36".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), error!("std::fs::File::seek(37)"))],
            } ) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 3737 });
        expect!(nix::fcntl::flock: { 3737, FlockArg::LockExclusive => Ok({}) });

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );

        verify_expectations!();
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_set_len_on_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test42/cgroups/cpuset", "prefix42").unwrap();

        expect!(
            std::fs::create_dir_all:
            { "/test42/cgroups/cpuset" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/pool" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/42" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test42/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
            { "/test42/cgroups/cpuset/prefix42/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/pool/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/pool/tasks", "2042" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/pool/tasks", "3042" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/pool/tasks", "4042" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/42/cpuset.mems", "42" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/42/cpuset.cpu_exclusive", "1" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/42/cpuset.cpus", "42" => Ok({}) },
            { "/test42/cgroups/cpuset/prefix42/42/tasks", "4042" => Ok({}) },
        );
        expect!(
            std::fs::read_to_string:
            { "/test42/cgroups/cpuset/prefix42/cpuset.mems" => Ok("42".to_string()) },
            { "/test42/cgroups/cpuset/prefix42/cpuset.cpus" => Ok("39-42".to_string()) },
            { "/test42/cgroups/cpuset/prefix42/pool/cpuset.mems" => Ok("42".to_string()) },
            { "/test42/cgroups/cpuset/prefix42/pool/cpuset.cpus" => Ok("39-42".to_string()) },
            { "/test42/cgroups/cpuset/prefix42/pool/cpuset.cpus" => Ok("39-42".to_string()) },
            { "/test42/cgroups/cpuset/tasks" => Ok("2042\n3042\n4042\n".to_string()) },
            { "/proc/2042/status" => Ok("Cpus_allowed_list:	39-42\n".to_string()) },
            { "/proc/3042/status" => Ok("Cpus_allowed_list:	39-42\n".to_string()) },
            { "/proc/4042/status" => Ok("Cpus_allowed_list:	39-42\n".to_string()) },
            { "/test42/cgroups/cpuset/prefix42/cpuset.mems" => Ok("42".to_string()) },
        );
        expect!(
            std::fs::File::open:
            { "/test42/cgroups/cpuset/prefix42/42/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(42)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test42/cgroups/cpuset/prefix42/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("39-42".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("39,40,41".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 42 });
        expect!(nix::fcntl::flock: { 42, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(42, 4042).unwrap();

        expect!(
            std::fs::File::open:
            { "/test42/cgroups/cpuset/prefix42/42/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::remove_dir: { "/test42/cgroups/cpuset/prefix42/42" => Ok({}) });
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } }
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test42/cgroups/cpuset/prefix42/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("39-41".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            } ) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 4242 });
        expect!(nix::fcntl::flock: { 4242, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => error!("std::fs::File::set_len(42)") });

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
            std::fs::create_dir_all:
            { "/test38/cgroups/cpuset" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/pool" => Ok({}) },
            { "/test38/cgroups/cpuset/prefix38/38" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test38/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test38/cgroups/cpuset/prefix38/38/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(38)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("32-38".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("32,33,34,35,36,37".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 38 });
        expect!(nix::fcntl::flock: { 38, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(38, 1038).unwrap();

        expect!(
            std::fs::File::open:
            { "/test38/cgroups/cpuset/prefix38/38/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::remove_dir: { "/test38/cgroups/cpuset/prefix38/38" => Ok({}) });
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } }
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("32-37".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![(
                    "32,33,34,35,36,37,38".to_string(),
                    Some(Error::new(ErrorKind::Other, "std::fs::File::write(38)"))
                )],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            } ) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 3838 });
        expect!(nix::fcntl::flock: { 3838, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

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
            std::fs::create_dir_all:
            { "/test39/cgroups/cpuset" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/pool" => Ok({}) },
            { "/test39/cgroups/cpuset/prefix39/39" => Ok({}) },
        );
        expect!(source_mounted_at: { "/test39/cgroups/cpuset" => Ok(true) });
        expect!(
            std::fs::write:
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
            std::fs::read_to_string:
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
            std::fs::File::open:
            { "/test39/cgroups/cpuset/prefix39/39/tasks" =>
                Err(Error::new(ErrorKind::NotFound, "std::fs::File::open(39)")) },
        );
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } },
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("35-39".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("35,36,37,38".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            }) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 39 });
        expect!(nix::fcntl::flock: { 39, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        cpuset.pin_task(39, 1039).unwrap();

        expect!(
            std::fs::File::open:
            { "/test39/cgroups/cpuset/prefix39/39/tasks" => Ok(std::fs::File {
                read: vec_deq![Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![],
            }) },
        );
        expect!(std::fs::remove_dir: { "/test39/cgroups/cpuset/prefix39/39" => Ok({}) });
        expect!(
            std::fs::OpenOptions::new:
            { _ => std::fs::OpenOptions { read: vec_deq![true], write: vec_deq![true] } }
        );
        expect!(
            std::fs::OpenOptions::open:
            { "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpus" => Ok(std::fs::File {
                read: vec_deq![Ok("35-38".to_string()), Ok(String::new())],
                lock: vec_deq![],
                write: vec_deq![("35,36,37,38,39".to_string(), None)],
                trim: vec_deq![],
                read_string: vec_deq![],
                seek: vec_deq![(SeekFrom::Start(0), Ok(0))],
            } ) },
        );
        expect!(std::fs::File::as_raw_fd: { _ => 3939 });
        expect!(nix::fcntl::flock: { 3939, FlockArg::LockExclusive => Ok({}) });
        expect!(std::fs::File::set_len: { 0 => Ok({}) });

        assert!(cpuset.release_threads().is_ok());

        verify_expectations!();
    }
}
