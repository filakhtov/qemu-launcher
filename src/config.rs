use std::{
    collections::HashMap,
    convert::TryFrom,
    io::{Error, ErrorKind},
};
use yaml_rust::{
    yaml::{Array, Hash},
    Yaml, YamlLoader,
};

enum Argument {
    Flag(String),
    Parameter(String, String),
}

pub struct Config {
    user: Option<u16>,
    group: Option<u16>,
    cpu_pinning: Vec<(usize, usize, usize, usize)>,
    qemu_binary: String,
    clear_env: bool,
    env: HashMap<String, String>,
    priority: Option<u8>,
    scheduler: Option<String>,
    command_line: Vec<Argument>,
}

impl Config {
    pub fn new(yaml: &str) -> Result<Self, Error> {
        let conf = match YamlLoader::load_from_str(yaml) {
            Ok(mut data) => match data.pop() {
                Some(conf) => conf,
                None => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "Supplied configuration is empty.",
                    ))
                }
            },
            Err(e) => return Err(Error::new(ErrorKind::InvalidData, format!("{}", e))),
        };

        Ok(Config {
            user: parse_user(&conf)?,
            group: parse_group(&conf)?,
            cpu_pinning: parse_cpu_pinning(&conf)?,
            qemu_binary: parse_qemu_binary(&conf)?,
            clear_env: parse_clear_env(&conf)?,
            env: parse_env(&conf)?,
            priority: parse_priority(&conf)?,
            scheduler: parse_scheduler(&conf)?,
            command_line: parse_command_line(&conf)?,
        })
    }

    pub fn get_user(&self) -> Option<u16> {
        self.user
    }

    pub fn get_group(&self) -> Option<u16> {
        self.group
    }

    pub fn get_cpu_pinning(&self) -> &Vec<(usize, usize, usize, usize)> {
        &self.cpu_pinning
    }

    pub fn get_command_line_options(&self) -> Vec<String> {
        let mut result = vec![];

        for option in &self.command_line {
            match option {
                Argument::Flag(flag) => result.push(format!("-{}", flag)),
                Argument::Parameter(name, value) => {
                    result.push(format!("-{}", name));
                    result.push(value.clone());
                }
            }
        }

        result
    }

    pub fn get_qemu_binary_path(&self) -> &String {
        &self.qemu_binary
    }

    pub fn has_cpu_pinning(&self) -> bool {
        self.cpu_pinning.len() > 0
    }

    pub fn should_clear_env(&self) -> bool {
        self.clear_env
    }

    pub fn get_env_vars(&self) -> &HashMap<String, String> {
        &self.env
    }

    pub fn has_env_vars(&self) -> bool {
        self.env.len() > 0
    }

    pub fn has_scheduling(&self) -> bool {
        if let None = self.scheduler {
            return false;
        }

        if let None = self.priority {
            return false;
        }

        true
    }

    pub fn get_priority(&self) -> Option<u8> {
        self.priority
    }

    pub fn get_scheduler(&self) -> &Option<String> {
        &self.scheduler
    }
}

fn parse_bool_value(yaml: &Yaml, key: &str) -> Result<bool, Error> {
    match yaml[key] {
        Yaml::Boolean(b) => Ok(b),
        Yaml::BadValue => Ok(false),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "Invalid value for `launcher.{}` value: a boolean is expected.",
                key
            ),
        )),
    }
}

fn parse_clear_env(config: &Yaml) -> Result<bool, Error> {
    parse_bool_value(&config["launcher"], "clear_env")
}

fn parse_qemu_binary(config: &Yaml) -> Result<String, Error> {
    match config["launcher"]["binary"].as_str() {
        Some(bin) => Ok(bin.to_string()),
        None => Err(Error::new(
            ErrorKind::InvalidData,
            "qemu binary path is not specified, missing or \
            the `launcher.binary` key has an invalid type.",
        )),
    }
}

fn parse_env(config: &Yaml) -> Result<HashMap<String, String>, Error> {
    match &config["launcher"]["env"] {
        Yaml::Hash(h) => parse_env_hash(h),
        Yaml::BadValue => Ok(HashMap::new()),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            "Invalid value for the `launcher.env` key: a hash expected.",
        )),
    }
}

fn parse_env_hash(env: &Hash) -> Result<HashMap<String, String>, Error> {
    let mut env_vars = HashMap::new();

    for (name, value) in env {
        let name = name
            .as_str()
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidData,
                    "Environment variable name must be a string.",
                )
            })?
            .to_string();

        let value = match value {
            Yaml::Boolean(b) => b.to_string(),
            Yaml::Integer(i) => i.to_string(),
            Yaml::Real(r) => r.to_string(),
            Yaml::String(s) => s.to_string(),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("Invalid value for the `{}` environment variable.", name,),
                ))
            }
        };

        env_vars.insert(name, value);
    }

    Ok(env_vars)
}

fn parse_u16_value(config: &Yaml, key: &str) -> Result<Option<u16>, Error> {
    match config[key] {
        Yaml::Integer(i) => match u16::try_from(i) {
            Ok(i) => Ok(Some(i)),
            Err(_) => Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Invalid value for `launcher.{}` option: given value is \
                    out of bounds, expected an unsigned 16-bit integer.",
                    key
                ),
            )),
        },
        Yaml::BadValue => Ok(None),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "Invalid value for `launcher.{}` option: unsigned 16-bit integer expected.",
                key
            ),
        )),
    }
}

fn parse_user(config: &Yaml) -> Result<Option<u16>, Error> {
    parse_u16_value(&config["launcher"], "user")
}

fn parse_group(config: &Yaml) -> Result<Option<u16>, Error> {
    parse_u16_value(&config["launcher"], "group")
}

fn parse_priority(config: &Yaml) -> Result<Option<u8>, Error> {
    match config["launcher"]["priority"] {
        Yaml::Integer(i) => match u8::try_from(i) {
            Ok(i) => Ok(Some(i)),
            Err(_) => Err(Error::new(
                ErrorKind::InvalidData,
                format!("Wrong value for `launcher.priority`: value out of bounds."),
            )),
        },
        Yaml::BadValue => Ok(None),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            format!("Failed to parse `launcher.priority`: an integer expected."),
        )),
    }
}

fn parse_scheduler(config: &Yaml) -> Result<Option<String>, Error> {
    match &config["launcher"]["scheduler"] {
        Yaml::String(s) => match s.as_str() {
            "batch" | "deadline" | "fifo" | "idle" | "other" | "rr" => Ok(Some(s.to_string())),
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Failed to parse `launcher.scheduler`: Expected one of \
                    `batch`, `deadline`, `fifo`, `idle`, `other` or `rr`."
                ),
            )),
        },
        Yaml::BadValue => Ok(None),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            format!("Failed to parse `launcher.scheduler`: string expected."),
        )),
    }
}

fn as_u64(id: &Yaml) -> Option<usize> {
    match id.as_i64() {
        Some(i) => match usize::try_from(i) {
            Ok(i) => Some(i),
            Err(_) => None,
        },
        None => None,
    }
}

fn parse_cpu_pinning_sockets(sockets: &Hash) -> Result<Vec<(usize, usize, usize, usize)>, Error> {
    let mut cpu_pinning = vec![];

    for (socket, cores) in sockets {
        let socket_id = match as_u64(socket) {
            Some(id) => Ok(id),
            None => Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Failed to parse `launcher.vcpu_pinning`: \
                        the socket ID must be an integer greater or equal to zero."
                ),
            )),
        }?;

        match cores {
            Yaml::Hash(cores) => {
                cpu_pinning.append(&mut parse_cpu_pinning_cores(socket_id, cores)?)
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "Failed to parse `launcher.vcpu_pinning.{}`: a hash expected.",
                        socket_id
                    ),
                ))
            }
        }
    }

    Ok(cpu_pinning)
}

fn parse_cpu_pinning_cores(
    socket_id: usize,
    cores: &Hash,
) -> Result<Vec<(usize, usize, usize, usize)>, Error> {
    let mut cpu_pinning = vec![];

    for (core, threads) in cores {
        let core_id = match as_u64(core) {
            Some(id) => Ok(id),
            None => Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Failed to parse `launcher.vcpu_pinning.{}`: \
                        the core ID must be an integer greater or equal to zero.",
                    socket_id
                ),
            )),
        }?;

        match threads {
            Yaml::Hash(threads) => {
                cpu_pinning.append(&mut parse_cpu_pinning_threads(socket_id, core_id, threads)?)
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "Failed to parse `launcher.vcpu_pinning.{}.{}`: a hash expected.",
                        socket_id, core_id
                    ),
                ))
            }
        }
    }

    Ok(cpu_pinning)
}

fn parse_cpu_pinning_threads(
    socket_id: usize,
    core_id: usize,
    threads: &Hash,
) -> Result<Vec<(usize, usize, usize, usize)>, Error> {
    let mut cpu_pinning = vec![];

    for (thread, host) in threads {
        let thread_id = match as_u64(thread) {
            Some(id) => Ok(id),
            None => Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Failed to parse `launcher.vcpu_pinning.{}.{}`: \
                        the thread ID must be a positive integer.",
                    socket_id, core_id
                ),
            )),
        }?;

        let host_id = match as_u64(host) {
            Some(id) => Ok(id),
            None => Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Failed to parse `launcher.vcpu_pinning.{}.{}.{}`: \
                        the host core ID must be an integer greater or equal to zero.",
                    socket_id, core_id, thread_id
                ),
            )),
        }?;

        cpu_pinning.push((socket_id, core_id, thread_id, host_id));
    }

    Ok(cpu_pinning)
}

fn parse_cpu_pinning(config: &Yaml) -> Result<Vec<(usize, usize, usize, usize)>, Error> {
    match &config["launcher"]["vcpu_pinning"] {
        Yaml::Hash(sockets) => parse_cpu_pinning_sockets(sockets),
        Yaml::BadValue => Ok(vec![]),
        _ => {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Failed to parse `launcher.vcpu_pinning` \
                        configuration: a hash is expected."
                ),
            ))
        }
    }
}

fn parse_command_line(config: &Yaml) -> Result<Vec<Argument>, Error> {
    match &config["qemu"] {
        Yaml::Array(options) => parse_command_line_options(options),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "Failed to parse qemu command line options: \
                    missing or invalid value, array expected."
            ),
        )),
    }
}

fn parse_command_line_options(options: &Array) -> Result<Vec<Argument>, Error> {
    let mut parsed_options = vec![];

    for (position, option) in options.iter().enumerate() {
        let position = position + 1;

        match option {
            Yaml::String(option) => parsed_options.push(Argument::Flag(option.to_owned())),
            Yaml::Hash(option) => parsed_options.push(parse_parameter(option, position)?),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "Failed to parse qemu command line option {}. \
                        Every option must be either a string or a hash.",
                        position
                    ),
                ))
            }
        }
    }

    parsed_options.push(Argument::Parameter(
        String::from("qmp"),
        String::from("stdio"),
    ));

    Ok(parsed_options)
}

fn parse_parameter(option: &Hash, position: usize) -> Result<Argument, Error> {
    if option.len() != 1 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "Found a command line argument {} with {} pairs \
                in the hash, but exactly one is expected.",
                position,
                option.len()
            ),
        ));
    }

    let entry = option.front().unwrap();

    let name = entry
        .0
        .as_str()
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Argument {} name must be a string.", position),
            )
        })?
        .to_string();

    let value = match entry.1 {
        Yaml::Integer(i) => i.to_string(),
        Yaml::String(s) => s.to_string(),
        Yaml::Real(r) => r.to_string(),
        Yaml::Array(v) => parse_parameter_value(&name, v)?,
        _ => {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Invalid value for `{}` qemu argument {}: expected a \
                        string, number or a hash with a single pair.",
                    name, position
                ),
            ))
        }
    };

    Ok(Argument::Parameter(name, value))
}

fn parse_parameter_value(name: &str, values: &Array) -> Result<String, Error> {
    if values.len() == 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "Empty value for `{}` argument, consider \
                    using a string instead of an array.",
                name
            ),
        ));
    }

    let mut parts = vec![];

    for value in values {
        match value {
            Yaml::String(s) => parts.push(s.to_string()),
            Yaml::Integer(i) => parts.push(i.to_string()),
            Yaml::Real(r) => parts.push(r.to_string()),
            Yaml::Hash(h) => parts.push(parse_parameter_value_part(&name, h)?),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "Invalid value for `{}` option: must be a \
                            hash with a single pair or a string",
                        name
                    ),
                ))
            }
        }
    }

    Ok(parts.join(","))
}

fn parse_parameter_value_part(name: &str, part: &Hash) -> Result<String, Error> {
    if part.len() != 1 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "Failed to parse a value for `{}` argument: \
                    a hash with multiple pairs found.",
                name
            ),
        ));
    }

    let entry = part.front().unwrap();

    let part_name = entry.0.as_str().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            format!(
                "Failed to parse a value for `{}` argument: \
                    a property name must be a string.",
                name
            ),
        )
    })?;

    let part_value = match entry.1 {
        Yaml::String(s) => s.to_string(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Real(r) => r.to_string(),
        _ => {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Failed to parse a value for `{}/{}` argument: \
                    value must be either a string or a number.",
                    name, part_name
                ),
            ))
        }
    };

    Ok(format!("{}={}", part_name, part_value))
}

#[cfg(test)]
mod test {
    use super::Config;
    use std::{
        collections::HashMap,
        io::{Error, ErrorKind},
    };

    #[test]
    fn config_with_all_options_parsed_properly() {
        let config = Config::new(
            "
            launcher:
              user: 100
              group: 200
              vcpu_pinning:
                0:
                  0:
                    0: 2
                    1: 6
                  1:
                    0: 3
                    1: 7
              binary: /usr/bin/qemu-kvm
              clear_env: true
              env:
                STRING: \"bar\"
                INTEGER: 1
                REAL: 1.0
                BOOLEAN: true
              priority: 1
              scheduler: fifo

            qemu:
            - realtime
            - cpu: host
            - smp: [ cpus: 1, cores: 2, threads: 2 ]
            - m: [ 4096, slots: 2 ]
            - numa: 1
            - seed: 1.234
            - device: [ vfio-pci, 3, 9.87 ]
            - device: [ vfio-pci, multifunction: on, addr: 0.1 ]
            - { device: [ usb-mouse ] }
            - device:
              - vfio-pci
              - multifunction: on
              - { addr: 0.2 }
        ",
        )
        .unwrap();

        assert_eq!(Some(100), config.get_user());
        assert_eq!(Some(200), config.get_group());

        let expected_cpu_pinnig: &Vec<(usize, usize, usize, usize)> =
            &vec![(0, 0, 0, 2), (0, 0, 1, 6), (0, 1, 0, 3), (0, 1, 1, 7)];
        assert_eq!(expected_cpu_pinnig, config.get_cpu_pinning());

        assert_eq!("/usr/bin/qemu-kvm", config.get_qemu_binary_path());
        assert_eq!(true, config.should_clear_env());
        assert_eq!(Some(1), config.get_priority());
        assert_eq!(&Some(String::from("fifo")), config.get_scheduler());
        assert_eq!("bar", config.get_env_vars()["STRING"]);
        assert_eq!("1", config.get_env_vars()["INTEGER"]);
        assert_eq!("1.0", config.get_env_vars()["REAL"]);
        assert_eq!("true", config.get_env_vars()["BOOLEAN"]);
        assert_eq!(
            vec![
                "-realtime",
                "-cpu",
                "host",
                "-smp",
                "cpus=1,cores=2,threads=2",
                "-m",
                "4096,slots=2",
                "-numa",
                "1",
                "-seed",
                "1.234",
                "-device",
                "vfio-pci,3,9.87",
                "-device",
                "vfio-pci,multifunction=on,addr=0.1",
                "-device",
                "usb-mouse",
                "-device",
                "vfio-pci,multifunction=on,addr=0.2",
                "-qmp",
                "stdio",
            ],
            config.get_command_line_options()
        );
    }

    #[test]
    fn config_with_absent_optional_fields_passed_properly() {
        let config = Config::new(
            "
            launcher:
              binary: /usr/bin/qemu-kvm

            qemu:
            - sda: /dev/sdb
        ",
        )
        .unwrap();

        assert_eq!("/usr/bin/qemu-kvm", config.get_qemu_binary_path());
        assert_eq!(None, config.get_user());
        assert_eq!(None, config.get_group());
        assert_eq!(
            &Vec::<(usize, usize, usize, usize)>::new(),
            config.get_cpu_pinning()
        );
        assert_eq!(false, config.should_clear_env());
        assert_eq!(None, config.get_priority());
        assert_eq!(&None, config.get_scheduler());
        assert_eq!(&HashMap::<String, String>::new(), config.get_env_vars());
        assert_eq!(
            vec!["-sda", "/dev/sdb", "-qmp", "stdio"],
            config.get_command_line_options()
        );
    }

    fn assert_error(result: Result<Config, Error>, kind: ErrorKind, message: &str) {
        match result {
            Ok(_) => panic!("Parser did not produce an error for invalid data."),
            Err(e) => {
                assert_eq!(kind, e.kind());
                assert_eq!(message, format!("{}", e));
            }
        }
    }

    #[test]
    fn empty_configuration_returns_error() {
        assert_error(
            Config::new(""),
            ErrorKind::InvalidData,
            "Supplied configuration is empty.",
        );
    }

    #[test]
    fn launcher_user_invalid_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  user: user
                  binary: /usr/bin/qemu-kvm

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `launcher.user` option: unsigned 16-bit integer expected.",
        );
    }

    #[test]
    fn launcher_user_negative_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  user: -1
                  binary: /usr/bin/qemu-kvm

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `launcher.user` option: given value is \
                out of bounds, expected an unsigned 16-bit integer.",
        );
    }

    #[test]
    fn launcher_user_too_high_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  user: 4294967295
                  binary: /usr/bin/qemu-kvm

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `launcher.user` option: given value is \
                out of bounds, expected an unsigned 16-bit integer.",
        );
    }

    #[test]
    fn launcher_group_invalid_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  group: group
                  binary: /usr/bin/qemu-kvm

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `launcher.group` option: unsigned 16-bit integer expected.",
        );
    }

    #[test]
    fn launcher_group_negative_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  group: -1
                  binary: /usr/bin/qemu-kvm

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `launcher.group` option: given value is \
                out of bounds, expected an unsigned 16-bit integer.",
        );
    }

    #[test]
    fn launcher_group_too_high_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  group: 4294967295
                  binary: /usr/bin/qemu-kvm

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `launcher.group` option: given value is \
                out of bounds, expected an unsigned 16-bit integer.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_not_a_hash_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning: []

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning` configuration: a hash is expected.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_socket_id_not_an_integer_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    a:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning`: the socket \
                ID must be an integer greater or equal to zero.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_negative_socket_id_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    -1:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning`: the socket \
                ID must be an integer greater or equal to zero.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_socket_value_is_not_a_hash_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    0: []

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning.0`: a hash expected.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_core_id_not_an_integer_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    0:
                      a:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning.0`: the core \
                ID must be an integer greater or equal to zero.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_negative_core_id_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    0:
                      -1:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning.0`: the core \
                ID must be an integer greater or equal to zero.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_core_value_is_not_a_hash_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    0:
                      0: []

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning.0.0`: a hash expected.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_thread_id_not_an_integer_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    0:
                      0:
                        a:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning.0.0`: \
                the thread ID must be a positive integer.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_negative_thread_id_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    0:
                      0:
                        -1:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning.0.0`: \
                the thread ID must be a positive integer.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_host_core_id_not_an_integer_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    0:
                      0:
                        0: true

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning.0.0.0`: the host \
                core ID must be an integer greater or equal to zero.",
        );
    }

    #[test]
    fn launcher_vcpu_pinning_negative_host_core_id_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  vcpu_pinning:
                    0:
                      0:
                        0: -1

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.vcpu_pinning.0.0.0`: the host \
                core ID must be an integer greater or equal to zero.",
        );
    }

    #[test]
    fn launcher_section_with_missing_qemu_binary_path_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "qemu binary path is not specified, missing or the \
                `launcher.binary` key has an invalid type.",
        );
    }

    #[test]
    fn launcher_section_with_non_string_qemu_binary_path_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: 3

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "qemu binary path is not specified, missing or the \
                `launcher.binary` key has an invalid type.",
        );
    }

    #[test]
    fn launcher_section_with_empty_qemu_binary_path_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "qemu binary path is not specified, missing or the \
                `launcher.binary` key has an invalid type.",
        );
    }

    #[test]
    fn launcher_section_with_invalid_clear_env_option_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  clear_env: 'yes'

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `launcher.clear_env` value: a boolean is expected.",
        );
    }

    #[test]
    fn launcher_section_with_empty_clear_env_option_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  clear_env:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `launcher.clear_env` value: a boolean is expected.",
        );
    }

    #[test]
    fn launcher_hash_with_invalid_env_value_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  env: []

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for the `launcher.env` key: a hash expected.",
        );
    }

    #[test]
    fn launcher_hash_with_empty_env_key_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  env:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for the `launcher.env` key: a hash expected.",
        );
    }

    #[test]
    fn launcher_hash_with_env_hash_containing_non_string_keys_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  env:
                    3: nope

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Environment variable name must be a string.",
        );
    }

    #[test]
    fn launcher_hash_with_env_hash_containing_invalid_values_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  env:
                    NOPE: []

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for the `NOPE` environment variable.",
        );
    }

    #[test]
    fn launcher_hash_with_empty_priority_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  priority:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.priority`: an integer expected.",
        );
    }

    #[test]
    fn launcher_hash_with_non_integer_priority_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  priority: a

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.priority`: an integer expected.",
        );
    }

    #[test]
    fn launcher_hash_with_too_high_priority_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  priority: 1024

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Wrong value for `launcher.priority`: value out of bounds.",
        );
    }

    #[test]
    fn launcher_hash_with_negative_priority_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  priority: -2

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Wrong value for `launcher.priority`: value out of bounds.",
        );
    }

    #[test]
    fn launcher_hash_with_invalid_scheduler_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  scheduler: []

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.scheduler`: string expected.",
        );
    }

    #[test]
    fn launcher_hash_with_empty_scheduler_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  scheduler:

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.scheduler`: string expected.",
        );
    }

    #[test]
    fn launcher_hash_with_unsupported_scheduler_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
                  scheduler: foo

                qemu:
                - sda: /dev/sdb
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse `launcher.scheduler`: Expected one of \
                `batch`, `deadline`, `fifo`, `idle`, `other` or `rr`.",
        );
    }

    #[test]
    fn missing_qemu_section_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse qemu command line options: \
                missing or invalid value, array expected.",
        );
    }

    #[test]
    fn non_array_qemu_section_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu: {}
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse qemu command line options: \
                missing or invalid value, array expected.",
        );
    }

    #[test]
    fn qemu_section_with_non_hash_or_string_argument_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - true
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse qemu command line option 1. Every \
                option must be either a string or a hash.",
        );
    }

    #[test]
    fn qemu_section_with_empty_argument_hash_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - {}
            ",
            ),
            ErrorKind::InvalidData,
            "Found a command line argument 1 with 0 pairs \
                in the hash, but exactly one is expected.",
        );
    }

    #[test]
    fn qemu_section_with_argument_hash_containing_multiple_items_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - { a: 1, b: 2 }
            ",
            ),
            ErrorKind::InvalidData,
            "Found a command line argument 1 with 2 pairs \
                in the hash, but exactly one is expected.",
        );
    }

    #[test]
    fn qemu_section_with_argument_hash_containing_non_string_keys_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - { 3: nope }
            ",
            ),
            ErrorKind::InvalidData,
            "Argument 1 name must be a string.",
        );
    }

    #[test]
    fn qemu_section_with_argument_hash_containing_invalid_values_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - still_no:
            ",
            ),
            ErrorKind::InvalidData,
            "Invalid value for `still_no` qemu argument 1: expected \
                a string, number or a hash with a single pair.",
        );
    }

    #[test]
    fn qemu_section_with_argument_hash_containing_empty_array_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - foo: []
            ",
            ),
            ErrorKind::InvalidData,
            "Empty value for `foo` argument, consider \
                using a string instead of an array.",
        );
    }

    #[test]
    fn qemu_section_with_argument_hash_containing_properties_hash_with_no_items_returns_error() {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - foo: [ bar: ]
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse a value for `foo/bar` argument: \
                value must be either a string or a number.",
        );
    }

    #[test]
    fn qemu_section_with_argument_hash_containing_properties_hash_with_multiple_items_returns_error(
    ) {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - foo: [ { bar: baz, this: wontwork } ]
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse a value for `foo` argument: \
                a hash with multiple pairs found.",
        );
    }

    #[test]
    fn qemu_section_with_argument_hash_containing_properties_hash_with_non_string_keys_returns_error(
    ) {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - foo: [ { 3: bad } ]
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse a value for `foo` argument: \
                a property name must be a string.",
        );
    }

    #[test]
    fn qemu_section_with_argument_hash_containing_properties_hash_with_invalid_values_returns_error(
    ) {
        assert_error(
            Config::new(
                "
                launcher:
                  binary: /usr/bin/qemu-kvm

                qemu:
                - foo: [ bad: [] ]
            ",
            ),
            ErrorKind::InvalidData,
            "Failed to parse a value for `foo/bad` argument: \
                value must be either a string or a number.",
        );
    }
}
