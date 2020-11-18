use std::io::{Error, ErrorKind};

pub struct Environment {
    config_directory: String,
    cpuset_mount_path: String,
    cpuset_prefix: String,
}

impl Environment {
    pub fn new(vars: impl Iterator<Item = (String, String)>) -> Result<Self, Error> {
        let mut config_directory = String::from("/usr/local/etc/qemu-launcher");
        let mut cpuset_mount_path = String::from("/sys/fs/cgroup/cpuset");
        let mut cpuset_prefix = String::from("qemu");

        for (name, value) in vars {
            match name.as_str() {
                "QEMU_LAUNCHER_CONFIG_DIR" => config_directory = value,
                "QEMU_LAUNCHER_CPUSET_MOUNT_PATH" => cpuset_mount_path = value,
                "QEMU_LAUNCHER_CPUSET_PREFIX" => cpuset_prefix = value,
                _ => {}
            }
        }

        validate_cpuset_prefix(&cpuset_prefix)?;

        Ok(Environment {
            config_directory: config_directory,
            cpuset_mount_path: cpuset_mount_path,
            cpuset_prefix: cpuset_prefix,
        })
    }

    pub fn get_config_directory(&self) -> &String {
        &self.config_directory
    }

    pub fn get_cpuset_mount_path(&self) -> &String {
        &self.cpuset_mount_path
    }

    pub fn get_cpuset_prefix(&self) -> &String {
        &self.cpuset_prefix
    }
}

fn validate_cpuset_prefix(prefix: &String) -> Result<(), Error> {
    if prefix.contains("\0") || prefix.contains("/") {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "`QEMU_LAUNCHER_CPUSET_PREFIX` environment variable has invalid characters",
        ));
    }

    Ok({})
}

#[cfg(test)]
mod test {
    use super::Environment;
    use std::io::ErrorKind;

    #[test]
    fn environment_uses_default_values_if_not_provided() {
        let env = Environment::new(vec![].into_iter()).unwrap();

        assert_eq!("/usr/local/etc/qemu-launcher", env.get_config_directory());
        assert_eq!("/sys/fs/cgroup/cpuset", env.get_cpuset_mount_path());
        assert_eq!("qemu", env.get_cpuset_prefix());
    }

    #[test]
    fn environment_uses_config_dir_if_provided() {
        let vars = vec![(
            "QEMU_LAUNCHER_CONFIG_DIR".to_owned(),
            "/my/config/dir".to_owned(),
        )]
        .into_iter();

        let env = Environment::new(vars).unwrap();

        assert_eq!("/my/config/dir", env.get_config_directory());
    }

    #[test]
    fn environment_uses_cpuset_mount_path_if_provided() {
        let vars = vec![(
            "QEMU_LAUNCHER_CPUSET_MOUNT_PATH".to_owned(),
            "/cpuset".to_owned(),
        )]
        .into_iter();

        let env = Environment::new(vars).unwrap();

        assert_eq!("/cpuset", env.get_cpuset_mount_path());
    }

    #[test]
    fn environment_uses_cpuset_prefix_if_provided() {
        let vars = vec![(
            "QEMU_LAUNCHER_CPUSET_PREFIX".to_owned(),
            "foobar".to_owned(),
        )]
        .into_iter();

        let env = Environment::new(vars).unwrap();

        assert_eq!("foobar", env.get_cpuset_prefix());
    }

    #[test]
    fn environment_returns_error_if_prefix_is_invalid() {
        let vars = vec![(
            "QEMU_LAUNCHER_CPUSET_PREFIX".to_owned(),
            "foo/bar".to_owned(),
        )]
        .into_iter();

        let result = Environment::new(vars);

        match result {
            Ok(_) => panic!(
                "Environment::new() returned no error for invalid `QEMU_LAUNCHER_CPUSET_PREFIX` variable"
            ),
            Err(e) => {
                assert!(format!("{}", e).contains("QEMU_LAUNCHER_CPUSET_PREFIX"));
                assert_eq!(ErrorKind::InvalidInput, e.kind());
            }
        }
    }
}
