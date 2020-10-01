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
#[cfg(test)]
use {std::cell::RefCell, std::str::from_utf8};

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

#[cfg(not(test))]
struct File {
    inner: fs::File,
}

#[cfg(not(test))]
impl File {
    fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Ok(Self {
            inner: fs::OpenOptions::new().read(true).write(true).open(path)?,
        })
    }

    fn lock(&mut self) -> Result<(), Error> {
        if let Err(e) = flock(self.inner.as_raw_fd(), FlockArg::LockExclusive) {
            return Err(Error::new(ErrorKind::Other, e));
        }

        Ok({})
    }

    fn trim(&mut self) -> Result<(), Error> {
        self.inner.seek(SeekFrom::Start(0))?;
        self.inner.set_len(0)
    }

    fn read_string(&mut self) -> Result<String, Error> {
        let mut data = String::new();
        self.inner.read_to_string(&mut data)?;

        Ok(data)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.inner.write(buf)
    }
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
        let file = fs_read_to_string(path!(self.mount_path, "tasks"))?;
        let path = path!(self.cpuset_path(), "pool", "tasks");
        for task in file.lines() {
            let _ = fs_write(&path, task);
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

#[cfg(not(test))]
#[inline]
fn fs_write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, data: C) -> Result<(), Error> {
    fs::write(path, data)
}

#[cfg(not(test))]
#[inline]
fn fs_create_dir_all<P: AsRef<Path>>(path: P) -> Result<(), Error> {
    fs::create_dir_all(path)
}

#[cfg(not(test))]
#[inline]
fn fs_read_to_string<P: AsRef<Path>>(path: P) -> Result<String, Error> {
    fs::read_to_string(path)
}

#[cfg(not(test))]
#[inline]
fn fs_remove_dir<P: AsRef<Path>>(path: P) -> Result<(), Error> {
    fs::remove_dir(path)
}

#[cfg(not(test))]
#[inline]
fn source_mounted_at<S: AsRef<Path>, P: AsRef<Path>>(source: S, path: P) -> Result<bool, Error> {
    MountIter::<BufReader<fs::File>>::source_mounted_at(source, path)
}

#[cfg(not(test))]
#[inline]
fn fs_read_line<P: AsRef<Path>>(path: P) -> Result<String, Error> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut data = String::new();
    reader.read_line(&mut data)?;

    Ok(data)
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

#[cfg(test)]
macro_rules! error {
    ($msg:expr) => {{
        Err(Error::new(ErrorKind::Other, format!("{}", $msg)))
    }};
}

#[cfg(test)]
macro_rules! pinned {
    ($if_pinned:expr ; $if_not:expr) => {{
        if is_thread_pinned() {
            return $if_pinned;
        }

        return $if_not;
    }};
}

#[cfg(test)]
thread_local! { static THREAD_PINNED: RefCell<bool> = RefCell::new(false); }

#[cfg(test)]
fn is_thread_pinned() -> bool {
    THREAD_PINNED.with(|pinned| *pinned.borrow())
}

#[cfg(test)]
fn set_thread_pinned() {
    THREAD_PINNED.with(|pinned| {
        *pinned.borrow_mut() = true;
    })
}

#[cfg(test)]
struct File {
    test_case: usize,
}

#[cfg(test)]
impl File {
    fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        match path.as_ref().to_str().unwrap() {
            "/test20/cgroups/cpuset/prefix20/pool/cpuset.cpus" => error!("File::open(20)"),
            "/test21/cgroups/cpuset/prefix21/pool/cpuset.cpus" => Ok(Self { test_case: 21 }),
            "/test22/cgroups/cpuset/prefix22/pool/cpuset.cpus" => Ok(Self { test_case: 22 }),
            "/test23/cgroups/cpuset/prefix23/pool/cpuset.cpus" => Ok(Self { test_case: 23 }),
            "/test24/cgroups/cpuset/prefix24/pool/cpuset.cpus" => Ok(Self { test_case: 24 }),
            "/test25/cgroups/cpuset/prefix25/pool/cpuset.cpus" => Ok(Self { test_case: 25 }),
            "/test26/cgroups/cpuset/prefix26/pool/cpuset.cpus" => Ok(Self { test_case: 26 }),
            "/test27/cgroups/cpuset/prefix27/pool/cpuset.cpus" => Ok(Self { test_case: 27 }),
            "/test28/cgroups/cpuset/prefix28/pool/cpuset.cpus" => Ok(Self { test_case: 28 }),
            "/test29/cgroups/cpuset/prefix29/pool/cpuset.cpus" => Ok(Self { test_case: 29 }),
            "/test30/cgroups/cpuset/prefix30/pool/cpuset.cpus" => Ok(Self { test_case: 30 }),
            "/test31/cgroups/cpuset/prefix31/pool/cpuset.cpus" => Ok(Self { test_case: 31 }),
            "/test32/cgroups/cpuset/prefix32/pool/cpuset.cpus" => Ok(Self { test_case: 32 }),
            "/test33/cgroups/cpuset/prefix33/pool/cpuset.cpus" => Ok(Self { test_case: 33 }),
            "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpus" => pinned!(
                error!("File::open(34)");
                Ok(Self { test_case: 34 })
            ),
            "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpus" => Ok(Self { test_case: 35 }),
            "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpus" => Ok(Self { test_case: 36 }),
            "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpus" => Ok(Self { test_case: 37 }),
            "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpus" => Ok(Self { test_case: 38 }),
            "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpus" => Ok(Self { test_case: 39 }),
            p => panic!("Unexpected call to File::open({})", p),
        }
    }

    fn lock(&mut self) -> Result<(), Error> {
        match self.test_case {
            21 => error!("File::lock(21)"),
            22 | 23 | 24 | 25 | 26 | 27 | 28 | 29 | 30 | 31 | 32 | 33 | 34 | 36 | 37 | 38 | 39 => {
                Ok({})
            }
            35 => pinned!(error!("File::lock(35)"); Ok({})),
            test => panic!(
                "Unexpected call to File::lock({}) while pinning a thread",
                test
            ),
        }
    }

    fn trim(&mut self) -> Result<(), Error> {
        match self.test_case {
            23 => error!("File::trim(23)"),
            24 | 25 | 26 | 27 | 28 | 29 | 30 | 31 | 32 | 33 | 34 | 35 | 36 | 38 | 39 => Ok({}),
            37 => pinned!(error!("File::trim(37)"); Ok({})),
            test => panic!(
                "Unexpected call to File::trim({}) while pinning a thread",
                test
            ),
        }
    }

    fn read_string(&mut self) -> Result<String, Error> {
        match self.test_case {
            22 => error!("File::read_string(22)"),
            23 => Ok("10-23".to_string()),
            24 => Ok("10-24".to_string()),
            25 => Ok("10-25".to_string()),
            26 => Ok("20-26".to_string()),
            27 => Ok("20-27".to_string()),
            28 => Ok("20-28".to_string()),
            29 => Ok("25-29".to_string()),
            30 => Ok("25-30".to_string()),
            31 => Ok("26-31".to_string()),
            32 => Ok("26-32".to_string()),
            33 => Ok("32-32".to_string()),
            34 => Ok("30-34".to_string()),
            35 => Ok("30-35".to_string()),
            36 => pinned!(error!("File::read_string(36)"); Ok("30-36".to_string())),
            37 => Ok("30-37".to_string()),
            38 => pinned!(Ok("30-37".to_string()); Ok("30-38".to_string())),
            39 => pinned!(Ok("30-38".to_string()); Ok("30-39".to_string())),
            test => panic!(
                "Unexpected call to File::read_string({}) while pinning a thread",
                test
            ),
        }
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        let buf = from_utf8(buf.as_ref()).unwrap();

        match (self.test_case, buf) {
            (24, "10,11,12,13,14,15,16,17,18,19,20,21,22,23") => error!("File::write(24)"),
            (25, "10,11,12,13,14,15,16,17,18,19,20,21,22,23,24")
            | (26, "20,21,22,23,24,25")
            | (27, "20,21,22,23,24,25,26")
            | (28, "20,21,22,23,24,25,26,27")
            | (29, "25,26,27,28")
            | (30, "25,26,27,28,29")
            | (31, "26,27,28,29,30")
            | (32, "26,27,28,29,30,31")
            | (33, "32")
            | (34, "30,31,32,33")
            | (35, "30,31,32,33,34")
            | (36, "30,31,32,33,34,35")
            | (37, "30,31,32,33,34,35,36")
            | (38, "30,31,32,33,34,35,36,37")
            | (39, "30,31,32,33,34,35,36,37,38")
            | (39, "30,31,32,33,34,35,36,37,38,39") => Ok(buf.len()),
            (38, "30,31,32,33,34,35,36,37,38") => pinned!(error!("File::write(38)"); Ok(buf.len())),
            _ => panic!(
                "Unexpected call to File::write({}) while pinning a thread",
                buf
            ),
        }
    }
}

#[cfg(test)]
fn fs_write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, data: C) -> Result<(), Error> {
    let path = path.as_ref().to_str().unwrap();
    let data = from_utf8(data.as_ref()).unwrap();

    match (path, data) {
        ("/test5/cgroups/cpuset/prefix5/cpuset.cpu_exclusive", "1") => error!("fs_write(5)"),
        ("/test6/cgroups/cpuset/prefix6/cpuset.cpu_exclusive", "1")
        | ("/test7/cgroups/cpuset/prefix7/cpuset.cpu_exclusive", "1")
        | ("/test8/cgroups/cpuset/prefix8/cpuset.cpu_exclusive", "1")
        | ("/test9/cgroups/cpuset/prefix9/cpuset.cpu_exclusive", "1")
        | ("/test9/cgroups/cpuset/prefix9/cpuset.mems", "9")
        | ("/test10/cgroups/cpuset/prefix10/cpuset.cpu_exclusive", "1")
        | ("/test11/cgroups/cpuset/prefix11/cpuset.cpu_exclusive", "1")
        | ("/test12/cgroups/cpuset/prefix12/cpuset.cpu_exclusive", "1")
        | ("/test12/cgroups/cpuset/prefix12/cpuset.cpus", "0-12")
        | ("/test13/cgroups/cpuset/prefix13/cpuset.cpu_exclusive", "1")
        | ("/test14/cgroups/cpuset/prefix14/cpuset.cpu_exclusive", "1")
        | ("/test14/cgroups/cpuset/prefix14/pool/cpuset.cpu_exclusive", "1")
        | ("/test15/cgroups/cpuset/prefix15/cpuset.cpu_exclusive", "1")
        | ("/test15/cgroups/cpuset/prefix15/pool/cpuset.cpu_exclusive", "1")
        | ("/test16/cgroups/cpuset/prefix16/cpuset.cpu_exclusive", "1")
        | ("/test16/cgroups/cpuset/prefix16/pool/cpuset.cpu_exclusive", "1")
        | ("/test16/cgroups/cpuset/prefix16/pool/cpuset.mems", "16")
        | ("/test17/cgroups/cpuset/prefix17/cpuset.cpu_exclusive", "1")
        | ("/test17/cgroups/cpuset/prefix17/pool/cpuset.cpu_exclusive", "1")
        | ("/test18/cgroups/cpuset/prefix18/cpuset.cpu_exclusive", "1")
        | ("/test18/cgroups/cpuset/prefix18/pool/cpuset.cpu_exclusive", "1")
        | ("/test18/cgroups/cpuset/prefix18/pool/cpuset.cpus", "0-18")
        | ("/test19/cgroups/cpuset/prefix19/cpuset.cpu_exclusive", "1")
        | ("/test19/cgroups/cpuset/prefix19/pool/cpuset.cpu_exclusive", "1")
        | ("/test19/cgroups/cpuset/prefix19/pool/tasks", "1119")
        | ("/test19/cgroups/cpuset/prefix19/pool/tasks", "1319")
        | ("/test20/cgroups/cpuset/prefix20/cpuset.cpu_exclusive", "1")
        | ("/test20/cgroups/cpuset/prefix20/pool/cpuset.cpu_exclusive", "1")
        | ("/test20/cgroups/cpuset/prefix20/pool/tasks", "1020")
        | ("/test20/cgroups/cpuset/prefix20/pool/tasks", "2020")
        | ("/test20/cgroups/cpuset/prefix20/pool/tasks", "3020")
        | ("/test21/cgroups/cpuset/prefix21/cpuset.cpu_exclusive", "1")
        | ("/test21/cgroups/cpuset/prefix21/pool/cpuset.cpu_exclusive", "1")
        | ("/test21/cgroups/cpuset/prefix21/pool/tasks", "1121")
        | ("/test21/cgroups/cpuset/prefix21/pool/tasks", "2121")
        | ("/test21/cgroups/cpuset/prefix21/pool/tasks", "3121")
        | ("/test22/cgroups/cpuset/prefix22/cpuset.cpu_exclusive", "1")
        | ("/test22/cgroups/cpuset/prefix22/pool/cpuset.cpu_exclusive", "1")
        | ("/test22/cgroups/cpuset/prefix22/pool/tasks", "1222")
        | ("/test22/cgroups/cpuset/prefix22/pool/tasks", "2222")
        | ("/test22/cgroups/cpuset/prefix22/pool/tasks", "3222")
        | ("/test23/cgroups/cpuset/prefix23/cpuset.cpu_exclusive", "1")
        | ("/test23/cgroups/cpuset/prefix23/pool/cpuset.cpu_exclusive", "1")
        | ("/test23/cgroups/cpuset/prefix23/pool/tasks", "1323")
        | ("/test23/cgroups/cpuset/prefix23/pool/tasks", "2323")
        | ("/test23/cgroups/cpuset/prefix23/pool/tasks", "3323")
        | ("/test24/cgroups/cpuset/prefix24/cpuset.cpu_exclusive", "1")
        | ("/test24/cgroups/cpuset/prefix24/pool/cpuset.cpu_exclusive", "1")
        | ("/test24/cgroups/cpuset/prefix24/pool/tasks", "1024")
        | ("/test24/cgroups/cpuset/prefix24/pool/tasks", "2024")
        | ("/test24/cgroups/cpuset/prefix24/pool/tasks", "3024")
        | ("/test25/cgroups/cpuset/prefix25/cpuset.cpu_exclusive", "1")
        | ("/test25/cgroups/cpuset/prefix25/pool/cpuset.cpu_exclusive", "1")
        | ("/test25/cgroups/cpuset/prefix25/pool/tasks", "1025")
        | ("/test25/cgroups/cpuset/prefix25/pool/tasks", "1125")
        | ("/test25/cgroups/cpuset/prefix25/pool/tasks", "1225")
        | ("/test26/cgroups/cpuset/prefix26/cpuset.cpu_exclusive", "1")
        | ("/test26/cgroups/cpuset/prefix26/pool/cpuset.cpu_exclusive", "1")
        | ("/test26/cgroups/cpuset/prefix26/pool/tasks", "1026")
        | ("/test26/cgroups/cpuset/prefix26/pool/tasks", "2026")
        | ("/test26/cgroups/cpuset/prefix26/pool/tasks", "3026")
        | ("/test27/cgroups/cpuset/prefix27/cpuset.cpu_exclusive", "1")
        | ("/test27/cgroups/cpuset/prefix27/pool/cpuset.cpu_exclusive", "1")
        | ("/test27/cgroups/cpuset/prefix27/pool/tasks", "1027")
        | ("/test27/cgroups/cpuset/prefix27/pool/tasks", "2027")
        | ("/test27/cgroups/cpuset/prefix27/pool/tasks", "3027")
        | ("/test27/cgroups/cpuset/prefix27/27/cpuset.mems", "27")
        | ("/test28/cgroups/cpuset/prefix28/cpuset.cpu_exclusive", "1")
        | ("/test28/cgroups/cpuset/prefix28/pool/cpuset.cpu_exclusive", "1")
        | ("/test28/cgroups/cpuset/prefix28/pool/tasks", "3028")
        | ("/test28/cgroups/cpuset/prefix28/pool/tasks", "4028")
        | ("/test28/cgroups/cpuset/prefix28/pool/tasks", "5028")
        | ("/test28/cgroups/cpuset/prefix28/28/cpuset.mems", "28")
        | ("/test28/cgroups/cpuset/prefix28/28/cpuset.cpu_exclusive", "1")
        | ("/test29/cgroups/cpuset/prefix29/cpuset.cpu_exclusive", "1")
        | ("/test29/cgroups/cpuset/prefix29/pool/cpuset.cpu_exclusive", "1")
        | ("/test29/cgroups/cpuset/prefix29/pool/tasks", "5029")
        | ("/test29/cgroups/cpuset/prefix29/pool/tasks", "4029")
        | ("/test29/cgroups/cpuset/prefix29/pool/tasks", "3029")
        | ("/test29/cgroups/cpuset/prefix29/29/cpuset.mems", "29")
        | ("/test29/cgroups/cpuset/prefix29/29/cpuset.cpu_exclusive", "1")
        | ("/test29/cgroups/cpuset/prefix29/29/cpuset.cpus", "29")
        | ("/test30/cgroups/cpuset/prefix30/cpuset.cpu_exclusive", "1")
        | ("/test30/cgroups/cpuset/prefix30/pool/cpuset.cpu_exclusive", "1")
        | ("/test30/cgroups/cpuset/prefix30/pool/tasks", "2030")
        | ("/test30/cgroups/cpuset/prefix30/pool/tasks", "3030")
        | ("/test30/cgroups/cpuset/prefix30/pool/tasks", "4030")
        | ("/test30/cgroups/cpuset/prefix30/30/cpuset.mems", "30")
        | ("/test30/cgroups/cpuset/prefix30/30/cpuset.cpu_exclusive", "1")
        | ("/test30/cgroups/cpuset/prefix30/30/cpuset.cpus", "30")
        | ("/test30/cgroups/cpuset/prefix30/30/tasks", "3030")
        | ("/test31/cgroups/cpuset/prefix31/cpuset.cpu_exclusive", "1")
        | ("/test31/cgroups/cpuset/prefix31/pool/cpuset.cpu_exclusive", "1")
        | ("/test31/cgroups/cpuset/prefix31/pool/tasks", "1031")
        | ("/test31/cgroups/cpuset/prefix31/pool/tasks", "2031")
        | ("/test31/cgroups/cpuset/prefix31/pool/tasks", "3031")
        | ("/test31/cgroups/cpuset/prefix31/31/cpuset.mems", "31")
        | ("/test31/cgroups/cpuset/prefix31/31/cpuset.cpu_exclusive", "1")
        | ("/test31/cgroups/cpuset/prefix31/31/cpuset.cpus", "31")
        | ("/test31/cgroups/cpuset/prefix31/31/tasks", "3031")
        | ("/test32/cgroups/cpuset/prefix32/cpuset.cpu_exclusive", "1")
        | ("/test32/cgroups/cpuset/prefix32/pool/cpuset.cpu_exclusive", "1")
        | ("/test32/cgroups/cpuset/prefix32/pool/tasks", "1032")
        | ("/test32/cgroups/cpuset/prefix32/pool/tasks", "2032")
        | ("/test32/cgroups/cpuset/prefix32/pool/tasks", "3032")
        | ("/test32/cgroups/cpuset/prefix32/32/cpuset.mems", "32")
        | ("/test32/cgroups/cpuset/prefix32/32/cpuset.cpu_exclusive", "1")
        | ("/test32/cgroups/cpuset/prefix32/32/cpuset.cpus", "32")
        | ("/test32/cgroups/cpuset/prefix32/32/tasks", "2032")
        | ("/test33/cgroups/cpuset/prefix33/cpuset.cpu_exclusive", "1")
        | ("/test33/cgroups/cpuset/prefix33/pool/cpuset.cpu_exclusive", "1")
        | ("/test33/cgroups/cpuset/prefix33/pool/tasks", "1033")
        | ("/test33/cgroups/cpuset/prefix33/pool/tasks", "2033")
        | ("/test33/cgroups/cpuset/prefix33/pool/tasks", "3033")
        | ("/test33/cgroups/cpuset/prefix33/33/cpuset.mems", "33")
        | ("/test33/cgroups/cpuset/prefix33/33/cpuset.cpu_exclusive", "1")
        | ("/test33/cgroups/cpuset/prefix33/33/cpuset.cpus", "33")
        | ("/test33/cgroups/cpuset/prefix33/33/tasks", "1033")
        | ("/test34/cgroups/cpuset/prefix34/cpuset.cpu_exclusive", "1")
        | ("/test34/cgroups/cpuset/prefix34/pool/cpuset.cpu_exclusive", "1")
        | ("/test34/cgroups/cpuset/prefix34/pool/tasks", "1034")
        | ("/test34/cgroups/cpuset/prefix34/pool/tasks", "2034")
        | ("/test34/cgroups/cpuset/prefix34/pool/tasks", "3034")
        | ("/test34/cgroups/cpuset/prefix34/34/cpuset.mems", "34")
        | ("/test34/cgroups/cpuset/prefix34/34/cpuset.cpu_exclusive", "1")
        | ("/test34/cgroups/cpuset/prefix34/34/cpuset.cpus", "34")
        | ("/test34/cgroups/cpuset/prefix34/34/tasks", "1034")
        | ("/test35/cgroups/cpuset/prefix35/cpuset.cpu_exclusive", "1")
        | ("/test35/cgroups/cpuset/prefix35/pool/cpuset.cpu_exclusive", "1")
        | ("/test35/cgroups/cpuset/prefix35/pool/tasks", "1035")
        | ("/test35/cgroups/cpuset/prefix35/pool/tasks", "2035")
        | ("/test35/cgroups/cpuset/prefix35/pool/tasks", "3035")
        | ("/test35/cgroups/cpuset/prefix35/35/cpuset.mems", "35")
        | ("/test35/cgroups/cpuset/prefix35/35/cpuset.cpu_exclusive", "1")
        | ("/test35/cgroups/cpuset/prefix35/35/cpuset.cpus", "35")
        | ("/test35/cgroups/cpuset/prefix35/35/tasks", "1035")
        | ("/test36/cgroups/cpuset/prefix36/cpuset.cpu_exclusive", "1")
        | ("/test36/cgroups/cpuset/prefix36/pool/cpuset.cpu_exclusive", "1")
        | ("/test36/cgroups/cpuset/prefix36/pool/tasks", "1036")
        | ("/test36/cgroups/cpuset/prefix36/pool/tasks", "2036")
        | ("/test36/cgroups/cpuset/prefix36/pool/tasks", "3036")
        | ("/test36/cgroups/cpuset/prefix36/36/cpuset.mems", "36")
        | ("/test36/cgroups/cpuset/prefix36/36/cpuset.cpu_exclusive", "1")
        | ("/test36/cgroups/cpuset/prefix36/36/cpuset.cpus", "36")
        | ("/test36/cgroups/cpuset/prefix36/36/tasks", "1036")
        | ("/test37/cgroups/cpuset/prefix37/cpuset.cpu_exclusive", "1")
        | ("/test37/cgroups/cpuset/prefix37/pool/cpuset.cpu_exclusive", "1")
        | ("/test37/cgroups/cpuset/prefix37/pool/tasks", "1037")
        | ("/test37/cgroups/cpuset/prefix37/pool/tasks", "2037")
        | ("/test37/cgroups/cpuset/prefix37/pool/tasks", "3037")
        | ("/test37/cgroups/cpuset/prefix37/37/cpuset.mems", "37")
        | ("/test37/cgroups/cpuset/prefix37/37/cpuset.cpu_exclusive", "1")
        | ("/test37/cgroups/cpuset/prefix37/37/cpuset.cpus", "37")
        | ("/test37/cgroups/cpuset/prefix37/37/tasks", "1037")
        | ("/test38/cgroups/cpuset/prefix38/cpuset.cpu_exclusive", "1")
        | ("/test38/cgroups/cpuset/prefix38/pool/cpuset.cpu_exclusive", "1")
        | ("/test38/cgroups/cpuset/prefix38/pool/tasks", "1038")
        | ("/test38/cgroups/cpuset/prefix38/pool/tasks", "2038")
        | ("/test38/cgroups/cpuset/prefix38/pool/tasks", "3038")
        | ("/test38/cgroups/cpuset/prefix38/38/cpuset.mems", "38")
        | ("/test38/cgroups/cpuset/prefix38/38/cpuset.cpu_exclusive", "1")
        | ("/test38/cgroups/cpuset/prefix38/38/cpuset.cpus", "38")
        | ("/test38/cgroups/cpuset/prefix38/38/tasks", "1038")
        | ("/test39/cgroups/cpuset/prefix39/cpuset.cpu_exclusive", "1")
        | ("/test39/cgroups/cpuset/prefix39/pool/cpuset.cpu_exclusive", "1")
        | ("/test39/cgroups/cpuset/prefix39/pool/tasks", "1039")
        | ("/test39/cgroups/cpuset/prefix39/pool/tasks", "2039")
        | ("/test39/cgroups/cpuset/prefix39/pool/tasks", "3039")
        | ("/test39/cgroups/cpuset/prefix39/39/cpuset.mems", "39")
        | ("/test39/cgroups/cpuset/prefix39/39/cpuset.cpu_exclusive", "1")
        | ("/test39/cgroups/cpuset/prefix39/39/cpuset.cpus", "39")
        | ("/test39/cgroups/cpuset/prefix39/39/tasks", "1039") => Ok({}),
        ("/test8/cgroups/cpuset/prefix8/cpuset.mems", "8") => error!("fs_write(8)"),
        ("/test11/cgroups/cpuset/prefix11/cpuset.cpus", "11") => error!("fs_write(11)"),
        ("/test13/cgroups/cpuset/prefix13/pool/cpuset.cpu_exclusive", "1") => {
            error!("fs_write(13)")
        }
        ("/test15/cgroups/cpuset/prefix15/pool/cpuset.mems", "15") => error!("fs_write(15)"),
        ("/test17/cgroups/cpuset/prefix17/pool/cpuset.cpus", "0-17") => error!("fs_write(17)"),
        ("/test19/cgroups/cpuset/prefix19/pool/tasks", "1019")
        | ("/test19/cgroups/cpuset/prefix19/pool/tasks", "1219") => error!("fs_write(19)"),
        ("/test26/cgroups/cpuset/prefix26/26/cpuset.mems", "26") => error!("fs_write(26)"),
        ("/test27/cgroups/cpuset/prefix27/27/cpuset.cpu_exclusive", "1") => error!("fs_write(27)"),
        ("/test28/cgroups/cpuset/prefix28/28/cpuset.cpus", "28") => error!("fs_write(28)"),
        ("/test29/cgroups/cpuset/prefix29/29/tasks", "32029") => error!("fs_write(29)"),
        _ => panic!("Unexpected call to fs_write({}, {})", path, data),
    }
}

#[cfg(test)]
fn fs_create_dir_all<P: AsRef<Path>>(path: P) -> Result<(), Error> {
    match path.as_ref().to_str().unwrap() {
        "/test1/cgroups/cpuset" => error!("fs_create_dir_all(1)"),
        "/test4/cgroups/cpuset/prefix4" => error!("fs_create_dir_all(4)"),
        "/test2/cgroups/cpuset"
        | "/test3/cgroups/cpuset"
        | "/test4/cgroups/cpuset"
        | "/test5/cgroups/cpuset"
        | "/test5/cgroups/cpuset/prefix5"
        | "/test6/cgroups/cpuset"
        | "/test6/cgroups/cpuset/prefix6"
        | "/test7/cgroups/cpuset"
        | "/test7/cgroups/cpuset/prefix7"
        | "/test8/cgroups/cpuset"
        | "/test8/cgroups/cpuset/prefix8"
        | "/test9/cgroups/cpuset"
        | "/test9/cgroups/cpuset/prefix9"
        | "/test10/cgroups/cpuset"
        | "/test10/cgroups/cpuset/prefix10"
        | "/test11/cgroups/cpuset"
        | "/test11/cgroups/cpuset/prefix11"
        | "/test12/cgroups/cpuset"
        | "/test12/cgroups/cpuset/prefix12"
        | "/test13/cgroups/cpuset"
        | "/test13/cgroups/cpuset/prefix13"
        | "/test13/cgroups/cpuset/prefix13/pool"
        | "/test14/cgroups/cpuset"
        | "/test14/cgroups/cpuset/prefix14"
        | "/test14/cgroups/cpuset/prefix14/pool"
        | "/test15/cgroups/cpuset"
        | "/test15/cgroups/cpuset/prefix15"
        | "/test15/cgroups/cpuset/prefix15/pool"
        | "/test16/cgroups/cpuset"
        | "/test16/cgroups/cpuset/prefix16"
        | "/test16/cgroups/cpuset/prefix16/pool"
        | "/test17/cgroups/cpuset"
        | "/test17/cgroups/cpuset/prefix17"
        | "/test17/cgroups/cpuset/prefix17/pool"
        | "/test18/cgroups/cpuset"
        | "/test18/cgroups/cpuset/prefix18"
        | "/test18/cgroups/cpuset/prefix18/pool"
        | "/test19/cgroups/cpuset"
        | "/test19/cgroups/cpuset/prefix19"
        | "/test19/cgroups/cpuset/prefix19/pool"
        | "/test20/cgroups/cpuset"
        | "/test20/cgroups/cpuset/prefix20"
        | "/test20/cgroups/cpuset/prefix20/pool"
        | "/test21/cgroups/cpuset"
        | "/test21/cgroups/cpuset/prefix21"
        | "/test21/cgroups/cpuset/prefix21/pool"
        | "/test22/cgroups/cpuset"
        | "/test22/cgroups/cpuset/prefix22"
        | "/test22/cgroups/cpuset/prefix22/pool"
        | "/test23/cgroups/cpuset"
        | "/test23/cgroups/cpuset/prefix23"
        | "/test23/cgroups/cpuset/prefix23/pool"
        | "/test24/cgroups/cpuset"
        | "/test24/cgroups/cpuset/prefix24"
        | "/test24/cgroups/cpuset/prefix24/pool"
        | "/test25/cgroups/cpuset"
        | "/test25/cgroups/cpuset/prefix25"
        | "/test25/cgroups/cpuset/prefix25/pool"
        | "/test26/cgroups/cpuset"
        | "/test26/cgroups/cpuset/prefix26"
        | "/test26/cgroups/cpuset/prefix26/pool"
        | "/test26/cgroups/cpuset/prefix26/26"
        | "/test27/cgroups/cpuset"
        | "/test27/cgroups/cpuset/prefix27"
        | "/test27/cgroups/cpuset/prefix27/pool"
        | "/test27/cgroups/cpuset/prefix27/27"
        | "/test28/cgroups/cpuset"
        | "/test28/cgroups/cpuset/prefix28"
        | "/test28/cgroups/cpuset/prefix28/pool"
        | "/test28/cgroups/cpuset/prefix28/28"
        | "/test29/cgroups/cpuset"
        | "/test29/cgroups/cpuset/prefix29"
        | "/test29/cgroups/cpuset/prefix29/pool"
        | "/test29/cgroups/cpuset/prefix29/29"
        | "/test30/cgroups/cpuset"
        | "/test30/cgroups/cpuset/prefix30"
        | "/test30/cgroups/cpuset/prefix30/pool"
        | "/test30/cgroups/cpuset/prefix30/30"
        | "/test31/cgroups/cpuset"
        | "/test31/cgroups/cpuset/prefix31"
        | "/test31/cgroups/cpuset/prefix31/pool"
        | "/test31/cgroups/cpuset/prefix31/31"
        | "/test32/cgroups/cpuset"
        | "/test32/cgroups/cpuset/prefix32"
        | "/test32/cgroups/cpuset/prefix32/pool"
        | "/test32/cgroups/cpuset/prefix32/32"
        | "/test33/cgroups/cpuset"
        | "/test33/cgroups/cpuset/prefix33"
        | "/test33/cgroups/cpuset/prefix33/pool"
        | "/test33/cgroups/cpuset/prefix33/33"
        | "/test34/cgroups/cpuset"
        | "/test34/cgroups/cpuset/prefix34"
        | "/test34/cgroups/cpuset/prefix34/pool"
        | "/test34/cgroups/cpuset/prefix34/34"
        | "/test35/cgroups/cpuset"
        | "/test35/cgroups/cpuset/prefix35"
        | "/test35/cgroups/cpuset/prefix35/pool"
        | "/test35/cgroups/cpuset/prefix35/35"
        | "/test36/cgroups/cpuset"
        | "/test36/cgroups/cpuset/prefix36"
        | "/test36/cgroups/cpuset/prefix36/pool"
        | "/test36/cgroups/cpuset/prefix36/36"
        | "/test37/cgroups/cpuset"
        | "/test37/cgroups/cpuset/prefix37"
        | "/test37/cgroups/cpuset/prefix37/pool"
        | "/test37/cgroups/cpuset/prefix37/37"
        | "/test38/cgroups/cpuset"
        | "/test38/cgroups/cpuset/prefix38"
        | "/test38/cgroups/cpuset/prefix38/pool"
        | "/test38/cgroups/cpuset/prefix38/38"
        | "/test39/cgroups/cpuset"
        | "/test39/cgroups/cpuset/prefix39"
        | "/test39/cgroups/cpuset/prefix39/pool"
        | "/test39/cgroups/cpuset/prefix39/39" => Ok({}),
        "/test25/cgroups/cpuset/prefix25/25" => error!("fs_create_dir_all(25)"),
        "/test12/cgroups/cpuset/prefix12/pool" => error!("fs_create_dir_all(12)"),
        p => panic!("Unexpected call to fs_create_dir_all({})", p),
    }
}

#[cfg(test)]
fn fs_read_to_string<P: AsRef<Path>>(path: P) -> Result<String, Error> {
    match path.as_ref().to_str().unwrap() {
        "/test6/cgroups/cpuset/prefix6/cpuset.mems" => error!("fs_read_to_string(6)"),
        "/test7/cgroups/cpuset/prefix7/cpuset.mems" => Ok(String::new()),
        "/test7/cgroups/cpuset/cpuset.mems" => error!("fs_read_to_string(7)"),
        "/test8/cgroups/cpuset/prefix8/cpuset.mems" => Ok(String::new()),
        "/test8/cgroups/cpuset/cpuset.mems" => Ok("8".to_string()),
        "/test9/cgroups/cpuset/prefix9/cpuset.mems" => Ok(String::new()),
        "/test9/cgroups/cpuset/cpuset.mems" => Ok("9".to_string()),
        "/test9/cgroups/cpuset/prefix9/cpuset.cpus" => error!("fs_read_to_string(9)"),
        "/test10/cgroups/cpuset/prefix10/cpuset.mems" => Ok("10".to_string()),
        "/test10/cgroups/cpuset/prefix10/cpuset.cpus" => Ok(String::new()),
        "/test10/cgroups/cpuset/cpuset.cpus" => error!("fs_read_to_string(10)"),
        "/test11/cgroups/cpuset/prefix11/cpuset.mems" => Ok("11".to_string()),
        "/test11/cgroups/cpuset/prefix11/cpuset.cpus" => Ok(String::new()),
        "/test11/cgroups/cpuset/cpuset.cpus" => Ok("11".to_string()),
        "/test12/cgroups/cpuset/prefix12/cpuset.mems" => Ok("12".to_string()),
        "/test12/cgroups/cpuset/prefix12/cpuset.cpus" => Ok(String::new()),
        "/test12/cgroups/cpuset/cpuset.cpus" => Ok("0-12".to_string()),
        "/test13/cgroups/cpuset/prefix13/cpuset.mems" => Ok("13".to_string()),
        "/test13/cgroups/cpuset/prefix13/cpuset.cpus" => Ok("0-13".to_string()),
        "/test14/cgroups/cpuset/prefix14/cpuset.mems" => Ok("14".to_string()),
        "/test14/cgroups/cpuset/prefix14/cpuset.cpus" => Ok("0-14".to_string()),
        "/test14/cgroups/cpuset/prefix14/pool/cpuset.mems" => error!("fs_read_to_string(14)"),
        "/test15/cgroups/cpuset/prefix15/cpuset.mems" => Ok("15".to_string()),
        "/test15/cgroups/cpuset/prefix15/cpuset.cpus" => Ok("0-15".to_string()),
        "/test15/cgroups/cpuset/prefix15/pool/cpuset.mems" => Ok(String::new()),
        "/test16/cgroups/cpuset/prefix16/cpuset.mems" => Ok("16".to_string()),
        "/test16/cgroups/cpuset/prefix16/cpuset.cpus" => Ok("0-16".to_string()),
        "/test16/cgroups/cpuset/prefix16/pool/cpuset.mems" => Ok(String::new()),
        "/test16/cgroups/cpuset/prefix16/pool/cpuset.cpus" => error!("fs_read_to_string(16)"),
        "/test17/cgroups/cpuset/prefix17/cpuset.mems" => Ok("17".to_string()),
        "/test17/cgroups/cpuset/prefix17/cpuset.cpus" => Ok("0-17".to_string()),
        "/test17/cgroups/cpuset/prefix17/pool/cpuset.mems" => Ok("17".to_string()),
        "/test17/cgroups/cpuset/prefix17/pool/cpuset.cpus" => Ok(String::new()),
        "/test18/cgroups/cpuset/prefix18/cpuset.mems" => Ok("18".to_string()),
        "/test18/cgroups/cpuset/prefix18/cpuset.cpus" => Ok("0-18".to_string()),
        "/test18/cgroups/cpuset/prefix18/pool/cpuset.mems" => Ok("18".to_string()),
        "/test18/cgroups/cpuset/prefix18/pool/cpuset.cpus" => Ok(String::new()),
        "/test18/cgroups/cpuset/tasks" => error!("fs_read_to_string(18)"),
        "/test19/cgroups/cpuset/prefix19/cpuset.mems" => Ok("19".to_string()),
        "/test19/cgroups/cpuset/prefix19/cpuset.cpus" => Ok("0-19".to_string()),
        "/test19/cgroups/cpuset/prefix19/pool/cpuset.mems" => Ok("19".to_string()),
        "/test19/cgroups/cpuset/prefix19/pool/cpuset.cpus" => Ok("0-19".to_string()),
        "/test19/cgroups/cpuset/tasks" => Ok("1019\n1119\n1219\n1319\n".to_string()),
        "/test20/cgroups/cpuset/prefix20/cpuset.mems" => Ok("20".to_string()),
        "/test20/cgroups/cpuset/prefix20/cpuset.cpus" => Ok("0-20".to_string()),
        "/test20/cgroups/cpuset/prefix20/pool/cpuset.mems" => Ok("20".to_string()),
        "/test20/cgroups/cpuset/prefix20/pool/cpuset.cpus" => Ok("0-20".to_string()),
        "/test20/cgroups/cpuset/tasks" => Ok("1020\n2020\n3020\n".to_string()),
        "/test21/cgroups/cpuset/prefix21/cpuset.mems" => Ok("21".to_string()),
        "/test21/cgroups/cpuset/prefix21/cpuset.cpus" => Ok("0-21".to_string()),
        "/test21/cgroups/cpuset/prefix21/pool/cpuset.mems" => Ok("21".to_string()),
        "/test21/cgroups/cpuset/prefix21/pool/cpuset.cpus" => Ok("0-21".to_string()),
        "/test21/cgroups/cpuset/tasks" => Ok("1121\n2121\n3121\n".to_string()),
        "/test22/cgroups/cpuset/prefix22/cpuset.mems" => Ok("22".to_string()),
        "/test22/cgroups/cpuset/prefix22/cpuset.cpus" => Ok("0-22".to_string()),
        "/test22/cgroups/cpuset/prefix22/pool/cpuset.mems" => Ok("22".to_string()),
        "/test22/cgroups/cpuset/prefix22/pool/cpuset.cpus" => Ok("0-22".to_string()),
        "/test22/cgroups/cpuset/tasks" => Ok("1222\n2222\n3222\n".to_string()),
        "/test23/cgroups/cpuset/prefix23/cpuset.mems" => Ok("23".to_string()),
        "/test23/cgroups/cpuset/prefix23/cpuset.cpus" => Ok("0-23".to_string()),
        "/test23/cgroups/cpuset/prefix23/pool/cpuset.mems" => Ok("23".to_string()),
        "/test23/cgroups/cpuset/prefix23/pool/cpuset.cpus" => Ok("0-23".to_string()),
        "/test23/cgroups/cpuset/tasks" => Ok("1323\n2323\n3323\n".to_string()),
        "/test24/cgroups/cpuset/prefix24/cpuset.mems" => Ok("24".to_string()),
        "/test24/cgroups/cpuset/prefix24/cpuset.cpus" => Ok("0-24".to_string()),
        "/test24/cgroups/cpuset/prefix24/pool/cpuset.mems" => Ok("24".to_string()),
        "/test24/cgroups/cpuset/prefix24/pool/cpuset.cpus" => Ok("0-24".to_string()),
        "/test24/cgroups/cpuset/tasks" => Ok("1024\n2024\n3024\n".to_string()),
        "/test25/cgroups/cpuset/prefix25/cpuset.mems" => Ok("25".to_string()),
        "/test25/cgroups/cpuset/prefix25/cpuset.cpus" => Ok("0-25".to_string()),
        "/test25/cgroups/cpuset/prefix25/pool/cpuset.mems" => Ok("25".to_string()),
        "/test25/cgroups/cpuset/prefix25/pool/cpuset.cpus" => Ok("0-25".to_string()),
        "/test25/cgroups/cpuset/tasks" => Ok("1025\n1125\n1225\n".to_string()),
        "/test26/cgroups/cpuset/prefix26/cpuset.mems" => Ok("26".to_string()),
        "/test26/cgroups/cpuset/prefix26/cpuset.cpus" => Ok("0-26".to_string()),
        "/test26/cgroups/cpuset/prefix26/pool/cpuset.mems" => Ok("26".to_string()),
        "/test26/cgroups/cpuset/prefix26/pool/cpuset.cpus" => Ok("0-26".to_string()),
        "/test26/cgroups/cpuset/tasks" => Ok("1026\n2026\n3026\n".to_string()),
        "/test27/cgroups/cpuset/prefix27/cpuset.mems" => Ok("27".to_string()),
        "/test27/cgroups/cpuset/prefix27/cpuset.cpus" => Ok("0-27".to_string()),
        "/test27/cgroups/cpuset/prefix27/pool/cpuset.mems" => Ok("27".to_string()),
        "/test27/cgroups/cpuset/prefix27/pool/cpuset.cpus" => Ok("0-27".to_string()),
        "/test27/cgroups/cpuset/tasks" => Ok("1027\n2027\n3027\n".to_string()),
        "/test28/cgroups/cpuset/prefix28/cpuset.mems" => Ok("28".to_string()),
        "/test28/cgroups/cpuset/prefix28/cpuset.cpus" => Ok("0-28".to_string()),
        "/test28/cgroups/cpuset/prefix28/pool/cpuset.mems" => Ok("28".to_string()),
        "/test28/cgroups/cpuset/prefix28/pool/cpuset.cpus" => Ok("0-28".to_string()),
        "/test28/cgroups/cpuset/tasks" => Ok("3028\n4028\n5028\n".to_string()),
        "/test29/cgroups/cpuset/prefix29/cpuset.mems" => Ok("29".to_string()),
        "/test29/cgroups/cpuset/prefix29/cpuset.cpus" => Ok("0-29".to_string()),
        "/test29/cgroups/cpuset/prefix29/pool/cpuset.mems" => Ok("29".to_string()),
        "/test29/cgroups/cpuset/prefix29/pool/cpuset.cpus" => Ok("0-29".to_string()),
        "/test29/cgroups/cpuset/tasks" => Ok("5029\n4029\n3029\n".to_string()),
        "/test30/cgroups/cpuset/prefix30/cpuset.mems" => Ok("30".to_string()),
        "/test30/cgroups/cpuset/prefix30/cpuset.cpus" => Ok("25-30".to_string()),
        "/test30/cgroups/cpuset/prefix30/pool/cpuset.mems" => Ok("30".to_string()),
        "/test30/cgroups/cpuset/prefix30/pool/cpuset.cpus" => Ok("25-30".to_string()),
        "/test30/cgroups/cpuset/tasks" => Ok("2030\n3030\n4030\n".to_string()),
        "/test31/cgroups/cpuset/prefix31/cpuset.mems" => Ok("31".to_string()),
        "/test31/cgroups/cpuset/prefix31/cpuset.cpus" => Ok("25-31".to_string()),
        "/test31/cgroups/cpuset/prefix31/pool/cpuset.mems" => Ok("31".to_string()),
        "/test31/cgroups/cpuset/prefix31/pool/cpuset.cpus" => Ok("25-31".to_string()),
        "/test31/cgroups/cpuset/tasks" => Ok("1031\n2031\n3031\n".to_string()),
        "/test32/cgroups/cpuset/prefix32/cpuset.mems" => Ok("32".to_string()),
        "/test32/cgroups/cpuset/prefix32/cpuset.cpus" => Ok("25-32".to_string()),
        "/test32/cgroups/cpuset/prefix32/pool/cpuset.mems" => Ok("32".to_string()),
        "/test32/cgroups/cpuset/prefix32/pool/cpuset.cpus" => Ok("25-32".to_string()),
        "/test32/cgroups/cpuset/tasks" => Ok("1032\n2032\n3032\n".to_string()),
        "/test33/cgroups/cpuset/prefix33/cpuset.mems" => Ok("33".to_string()),
        "/test33/cgroups/cpuset/prefix33/cpuset.cpus" => Ok("27-33".to_string()),
        "/test33/cgroups/cpuset/prefix33/pool/cpuset.mems" => Ok("33".to_string()),
        "/test33/cgroups/cpuset/prefix33/pool/cpuset.cpus" => Ok("32-33".to_string()),
        "/test33/cgroups/cpuset/tasks" => Ok("1033\n2033\n3033".to_string()),
        "/test34/cgroups/cpuset/prefix34/cpuset.mems" => Ok("34".to_string()),
        "/test34/cgroups/cpuset/prefix34/cpuset.cpus" => Ok("30-34".to_string()),
        "/test34/cgroups/cpuset/prefix34/pool/cpuset.mems" => Ok("34".to_string()),
        "/test34/cgroups/cpuset/prefix34/pool/cpuset.cpus" => Ok("30-34".to_string()),
        "/test34/cgroups/cpuset/tasks" => Ok("1034\n2034\n3034\n".to_string()),
        "/test35/cgroups/cpuset/prefix35/cpuset.mems" => Ok("35".to_string()),
        "/test35/cgroups/cpuset/prefix35/cpuset.cpus" => Ok("30-35".to_string()),
        "/test35/cgroups/cpuset/prefix35/pool/cpuset.mems" => Ok("35".to_string()),
        "/test35/cgroups/cpuset/prefix35/pool/cpuset.cpus" => Ok("30-35".to_string()),
        "/test35/cgroups/cpuset/tasks" => Ok("1035\n2035\n3035\n".to_string()),
        "/test36/cgroups/cpuset/prefix36/cpuset.mems" => Ok("36".to_string()),
        "/test36/cgroups/cpuset/prefix36/cpuset.cpus" => Ok("30-36".to_string()),
        "/test36/cgroups/cpuset/prefix36/pool/cpuset.mems" => Ok("36".to_string()),
        "/test36/cgroups/cpuset/prefix36/pool/cpuset.cpus" => Ok("30-36".to_string()),
        "/test36/cgroups/cpuset/tasks" => Ok("1036\n2036\n3036\n".to_string()),
        "/test37/cgroups/cpuset/prefix37/cpuset.mems" => Ok("37".to_string()),
        "/test37/cgroups/cpuset/prefix37/cpuset.cpus" => Ok("30-37".to_string()),
        "/test37/cgroups/cpuset/prefix37/pool/cpuset.mems" => Ok("37".to_string()),
        "/test37/cgroups/cpuset/prefix37/pool/cpuset.cpus" => Ok("30-37".to_string()),
        "/test37/cgroups/cpuset/tasks" => Ok("1037\n2037\n3037\n".to_string()),
        "/test38/cgroups/cpuset/prefix38/cpuset.mems" => Ok("38".to_string()),
        "/test38/cgroups/cpuset/prefix38/cpuset.cpus" => Ok("30-38".to_string()),
        "/test38/cgroups/cpuset/prefix38/pool/cpuset.mems" => Ok("38".to_string()),
        "/test38/cgroups/cpuset/prefix38/pool/cpuset.cpus" => Ok("30-38".to_string()),
        "/test38/cgroups/cpuset/tasks" => Ok("1038\n2038\n3038\n".to_string()),
        "/test39/cgroups/cpuset/prefix39/cpuset.mems" => Ok("39".to_string()),
        "/test39/cgroups/cpuset/prefix39/cpuset.cpus" => Ok("30-39".to_string()),
        "/test39/cgroups/cpuset/prefix39/pool/cpuset.mems" => Ok("39".to_string()),
        "/test39/cgroups/cpuset/prefix39/pool/cpuset.cpus" => Ok("30-39".to_string()),
        "/test39/cgroups/cpuset/tasks" => Ok("1039\n2039\n3039\n".to_string()),
        p => panic!("Unexpected call to fs_read_to_string({})", p),
    }
}

#[cfg(test)]
fn fs_remove_dir<P: AsRef<Path>>(path: P) -> Result<(), Error> {
    let path = path.as_ref().to_str().unwrap();

    match path {
        "/test33/cgroups/cpuset/prefix33/33" => error!("fs_remove_dir(33)"),
        "/test34/cgroups/cpuset/prefix34/34"
        | "/test35/cgroups/cpuset/prefix35/35"
        | "/test36/cgroups/cpuset/prefix36/36"
        | "/test37/cgroups/cpuset/prefix37/37"
        | "/test38/cgroups/cpuset/prefix38/38"
        | "/test39/cgroups/cpuset/prefix39/39" => Ok({}),
        p => panic!("Unexpected call to fs::remove_dir({})", p),
    }
}

#[cfg(test)]
fn source_mounted_at<S: AsRef<Path>, P: AsRef<Path>>(source: S, path: P) -> Result<bool, Error> {
    if "cgroup" != source.as_ref().to_str().unwrap() {
        panic!("Unexpected call to source_mounted_at(): source must be `cgroup`.");
    }

    match path.as_ref().to_str().unwrap() {
        "/test2/cgroups/cpuset" => error!("source_mounted_at(2)"),
        "/test3/cgroups/cpuset" | "/test4/cgroups/cpuset" => Ok(false),
        "/test5/cgroups/cpuset"
        | "/test6/cgroups/cpuset"
        | "/test7/cgroups/cpuset"
        | "/test8/cgroups/cpuset"
        | "/test9/cgroups/cpuset"
        | "/test10/cgroups/cpuset"
        | "/test11/cgroups/cpuset"
        | "/test12/cgroups/cpuset"
        | "/test13/cgroups/cpuset"
        | "/test14/cgroups/cpuset"
        | "/test15/cgroups/cpuset"
        | "/test16/cgroups/cpuset"
        | "/test17/cgroups/cpuset"
        | "/test18/cgroups/cpuset"
        | "/test19/cgroups/cpuset"
        | "/test20/cgroups/cpuset"
        | "/test21/cgroups/cpuset"
        | "/test22/cgroups/cpuset"
        | "/test23/cgroups/cpuset"
        | "/test24/cgroups/cpuset"
        | "/test25/cgroups/cpuset"
        | "/test26/cgroups/cpuset"
        | "/test27/cgroups/cpuset"
        | "/test28/cgroups/cpuset"
        | "/test29/cgroups/cpuset"
        | "/test30/cgroups/cpuset"
        | "/test31/cgroups/cpuset"
        | "/test32/cgroups/cpuset"
        | "/test33/cgroups/cpuset"
        | "/test34/cgroups/cpuset"
        | "/test35/cgroups/cpuset"
        | "/test36/cgroups/cpuset"
        | "/test37/cgroups/cpuset"
        | "/test38/cgroups/cpuset"
        | "/test39/cgroups/cpuset" => Ok(true),
        p => panic!("Unexpected call to MountIter::source_mounted_at({})", p),
    }
}

#[cfg(test)]
fn fs_read_line<P: AsRef<Path>>(path: P) -> Result<String, Error> {
    let path = path.as_ref().to_str().unwrap();

    match path {
        "/test19/cgroups/cpuset/prefix19/19/tasks" => error!("fs_read_line(19)"),
        "/test20/cgroups/cpuset/prefix20/20/tasks" => Ok("1020\n".to_string()),
        "/test21/cgroups/cpuset/prefix21/21/tasks" => Ok("".to_string()),
        "/test22/cgroups/cpuset/prefix22/22/tasks"
        | "/test23/cgroups/cpuset/prefix23/23/tasks"
        | "/test24/cgroups/cpuset/prefix24/24/tasks"
        | "/test25/cgroups/cpuset/prefix25/25/tasks"
        | "/test26/cgroups/cpuset/prefix26/26/tasks"
        | "/test27/cgroups/cpuset/prefix27/27/tasks"
        | "/test28/cgroups/cpuset/prefix28/28/tasks"
        | "/test29/cgroups/cpuset/prefix29/29/tasks"
        | "/test30/cgroups/cpuset/prefix30/30/tasks" => {
            Err(Error::new(ErrorKind::NotFound, "fs_read_line()"))
        }
        "/test31/cgroups/cpuset/prefix31/31/tasks" => pinned!(
            Err(Error::new(ErrorKind::Other, "fs_read_line(31_pinned)"));
            Err(Error::new(ErrorKind::NotFound, "fs_read_line(31_unpinned)"))
        ),
        "/test32/cgroups/cpuset/prefix32/32/tasks" => pinned!(
            Ok("1217".to_string());
            Err(Error::new(ErrorKind::NotFound, "fs_read_line(31_unpinned)"))
        ),
        "/test33/cgroups/cpuset/prefix33/33/tasks"
        | "/test34/cgroups/cpuset/prefix34/34/tasks"
        | "/test35/cgroups/cpuset/prefix35/35/tasks"
        | "/test36/cgroups/cpuset/prefix36/36/tasks"
        | "/test37/cgroups/cpuset/prefix37/37/tasks"
        | "/test38/cgroups/cpuset/prefix38/38/tasks"
        | "/test39/cgroups/cpuset/prefix39/39/tasks" => pinned!(
            Ok(String::new());
            Err(Error::new(ErrorKind::NotFound, "fs_read_line()"))
        ),
        p => panic!("Unexpected call to fs_read_line({})", p),
    }
}

#[cfg(test)]
fn fs_mount<P1: AsRef<Path>>(target: P1) -> Result<(), nix::Error> {
    match target.as_ref().to_str().unwrap() {
        "/test3/cgroups/cpuset" => Err(nix::Error::InvalidPath),
        "/test4/cgroups/cpuset" => Ok({}),
        p => panic!("Unexpected call to fs_mount({})", p),
    }
}

#[cfg(test)]
mod test {
    use super::{set_thread_pinned, CpuSet};
    use crate::assert_error;
    use std::io::ErrorKind;

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

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `1` - fs_create_dir_all(1)",
            cpuset.pin_task(1, 32001)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_check_cpuset_cgroug_mount_status() {
        let mut cpuset = CpuSet::new("/test2/cgroups/cpuset", "prefix2").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `2` - An error \
            occurred while reading mounts: source_mounted_at(2)",
            cpuset.pin_task(2, 32002)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_cpuset_cgroup_mount_fails() {
        let mut cpuset = CpuSet::new("/test3/cgroups/cpuset", "prefix3").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `3` - Failed to \
            mount cpuset to `/test3/cgroups/cpuset`: Invalid path",
            cpuset.pin_task(3, 32003)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_cpuset_prefix_directory() {
        let mut cpuset = CpuSet::new("/test4/cgroups/cpuset", "prefix4").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `4` - fs_create_dir_all(4)",
            cpuset.pin_task(4, 32004)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_make_cpuset_cpu_exclusive() {
        let mut cpuset = CpuSet::new("/test5/cgroups/cpuset", "prefix5").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `5` - fs_write(5)",
            cpuset.pin_task(5, 32005)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_prefix_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test6/cgroups/cpuset", "prefix6").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `6` - fs_read_to_string(6)",
            cpuset.pin_task(6, 32006)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test7/cgroups/cpuset", "prefix7").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `7` - fs_read_to_string(7)",
            cpuset.pin_task(7, 32007)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_prefix_cpuset_mems() {
        let mut cpuset = CpuSet::new("/test8/cgroups/cpuset", "prefix8").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `8` - fs_write(8)",
            cpuset.pin_task(8, 32008)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_prefix_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test9/cgroups/cpuset", "prefix9").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `9` - fs_read_to_string(9)",
            cpuset.pin_task(9, 32009)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test10/cgroups/cpuset", "prefix10").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `10` - fs_read_to_string(10)",
            cpuset.pin_task(10, 32010)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_prefix_cpuset_cpus() {
        let mut cpuset = CpuSet::new("/test11/cgroups/cpuset", "prefix11").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `11` - fs_write(11)",
            cpuset.pin_task(11, 32011)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_cpu_pool_directory() {
        let mut cpuset = CpuSet::new("/test12/cgroups/cpuset", "prefix12").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `12` - fs_create_dir_all(12)",
            cpuset.pin_task(12, 32012)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_make_cpuset_pool_cpu_exclusive() {
        let mut cpuset = CpuSet::new("/test13/cgroups/cpuset", "prefix13").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `13` - fs_write(13)",
            cpuset.pin_task(13, 32013)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_mems() {
        let mut cpuset = CpuSet::new("/test14/cgroups/cpuset", "prefix14").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `14` - fs_read_to_string(14)",
            cpuset.pin_task(14, 32014)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_mems() {
        let mut cpuset = CpuSet::new("/test15/cgroups/cpuset", "prefix15").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `15` - fs_write(15)",
            cpuset.pin_task(15, 32015)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_cpus() {
        let mut cpuset = CpuSet::new("/test16/cgroups/cpuset", "prefix16").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `16` - fs_read_to_string(16)",
            cpuset.pin_task(16, 32016)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_cpus() {
        let mut cpuset = CpuSet::new("/test17/cgroups/cpuset", "prefix17").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `17` - fs_write(17)",
            cpuset.pin_task(17, 32017)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_tasks() {
        let mut cpuset = CpuSet::new("/test18/cgroups/cpuset", "prefix18").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `18` - fs_read_to_string(18)",
            cpuset.pin_task(18, 32018)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_thread_tasks() {
        let mut cpuset = CpuSet::new("/test19/cgroups/cpuset", "prefix19").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `19` - fs_read_line(19)",
            cpuset.pin_task(19, 32019)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_open_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test20/cgroups/cpuset", "prefix20").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `20` - File::open(20)",
            cpuset.pin_task(20, 32020)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_lock_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test21/cgroups/cpuset", "prefix21").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `21` - File::lock(21)",
            cpuset.pin_task(21, 32021)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_read_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test22/cgroups/cpuset", "prefix22").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `22` - File::read_string(22)",
            cpuset.pin_task(22, 32022)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_trim_cpuset_pool_cpus_file_to_isolate_thread() {
        let mut cpuset = CpuSet::new("/test23/cgroups/cpuset", "prefix23").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `23` - File::trim(23)",
            cpuset.pin_task(23, 32023)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_pool_cpus_file_with_isolated_thread()
    {
        let mut cpuset = CpuSet::new("/test24/cgroups/cpuset", "prefix24").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `24` - File::write(24)",
            cpuset.pin_task(24, 32024)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_create_a_cpuset_directory_for_isolated_thread() {
        let mut cpuset = CpuSet::new("/test25/cgroups/cpuset", "prefix25").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `25` - fs_create_dir_all(25)",
            cpuset.pin_task(25, 32025)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_mems_for_isloated_thread() {
        let mut cpuset = CpuSet::new("/test26/cgroups/cpuset", "prefix26").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `26` - fs_write(26)",
            cpuset.pin_task(26, 32026)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_cpu_exclusive_for_isloated_thread() {
        let mut cpuset = CpuSet::new("/test27/cgroups/cpuset", "prefix27").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `27` - fs_write(27)",
            cpuset.pin_task(27, 32027)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_write_cpuset_cpus_for_isolated_thread() {
        let mut cpuset = CpuSet::new("/test28/cgroups/cpuset", "prefix28").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to isolate the host cpu thread `28` - fs_write(28)",
            cpuset.pin_task(28, 32028)
        );
    }

    #[test]
    fn cpuset_pin_task_returns_error_if_unable_to_pin_task_to_isolated_thread() {
        let mut cpuset = CpuSet::new("/test29/cgroups/cpuset", "prefix29").unwrap();

        assert_error!(
            ErrorKind::Other,
            "Failed to pin the process id `32029` to the host cpu thread `29` - fs_write(29)",
            cpuset.pin_task(29, 32029)
        );
    }

    #[test]
    fn cpuset_pin_task_isolates_the_thread_and_pins_the_task_to_it() {
        let mut cpuset = CpuSet::new("/test30/cgroups/cpuset", "prefix30").unwrap();

        assert!(cpuset.pin_task(30, 3030).is_ok());
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_unable_to_read_pinned_thread_tasks() {
        let mut cpuset = CpuSet::new("/test31/cgroups/cpuset", "prefix31").unwrap();
        cpuset.pin_task(31, 3031).unwrap();

        set_thread_pinned();

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_thread_still_busy_with_at_least_one_process() {
        let mut cpuset = CpuSet::new("/test32/cgroups/cpuset", "prefix32").unwrap();
        cpuset.pin_task(32, 2032).unwrap();

        set_thread_pinned();

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_unable_to_remove_thread_cpuset_cgroup_directory() {
        let mut cpuset = CpuSet::new("/test33/cgroups/cpuset", "prefix33").unwrap();
        cpuset.pin_task(33, 1033).unwrap();

        set_thread_pinned();

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_open_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test34/cgroups/cpuset", "prefix34").unwrap();
        cpuset.pin_task(34, 1034).unwrap();

        set_thread_pinned();

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_lock_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test35/cgroups/cpuset", "prefix35").unwrap();
        cpuset.pin_task(35, 1035).unwrap();

        set_thread_pinned();

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_read_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test36/cgroups/cpuset", "prefix36").unwrap();
        cpuset.pin_task(36, 1036).unwrap();

        set_thread_pinned();

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_trim_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test37/cgroups/cpuset", "prefix37").unwrap();
        cpuset.pin_task(37, 1037).unwrap();

        set_thread_pinned();

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );
    }

    #[test]
    fn cpuset_release_threads_returns_error_if_uanble_to_write_pool_cpuset_cpus_file() {
        let mut cpuset = CpuSet::new("/test38/cgroups/cpuset", "prefix38").unwrap();
        cpuset.pin_task(38, 1038).unwrap();

        set_thread_pinned();

        assert_error!(
            ErrorKind::Other,
            "Failed to release some of the pinned threads.",
            cpuset.release_threads()
        );
    }

    #[test]
    fn cpuset_release_threads_returns_all_pinned_threads_back_to_pool() {
        let mut cpuset = CpuSet::new("/test39/cgroups/cpuset", "prefix39").unwrap();
        cpuset.pin_task(39, 1039).unwrap();

        set_thread_pinned();

        assert!(cpuset.release_threads().is_ok());
    }
}
