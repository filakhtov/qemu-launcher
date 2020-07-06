extern crate yaml_rust;

use std::{
    collections::HashMap,
    fs,
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
    user: Option<u32>,
    group: Option<u32>,
    cpu_pinning: Vec<(usize, usize, usize, usize)>,
    command_line: Vec<Argument>,
    qemu_binary: String,
    clear_env: bool,
    debug: bool,
    env: HashMap<String, String>,
    priority: Option<u8>,
    scheduler: Option<String>,
}

impl Config {
    pub fn new(path: &str) -> Result<Self, Error> {
        let yaml = fs::read_to_string(path)?;

        match YamlLoader::load_from_str(&yaml) {
            Ok(conf) => Ok(Config {
                user: parse_user(&conf),
                group: parse_group(&conf),
                cpu_pinning: parse_cpu_pinning(&conf)?,
                command_line: parse_command_line(&conf)?,
                qemu_binary: parse_qemu_binary(&conf)?,
                clear_env: parse_clear_env(&conf)?,
                debug: parse_debug(&conf)?,
                env: parse_env(&conf)?,
                priority: parse_priority(&conf)?,
                scheduler: parse_scheduler(&conf)?,
            }),
            Err(e) => Err(Error::new(ErrorKind::Other, format!("{}", e))),
        }
    }

    pub fn is_debug_enabled(&self) -> bool {
        self.debug
    }

    pub fn get_user(&self) -> Option<u32> {
        self.user.clone()
    }

    pub fn get_group(&self) -> Option<u32> {
        self.group.clone()
    }

    pub fn get_cpu_pinning(&self) -> Vec<(usize, usize, usize, usize)> {
        self.cpu_pinning.clone()
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

    pub fn get_qemu_binary_path(&self) -> String {
        self.qemu_binary.clone()
    }

    pub fn has_cpu_pinning(&self) -> bool {
        self.cpu_pinning.len() > 0
    }

    pub fn should_clear_env(&self) -> bool {
        self.clear_env
    }

    pub fn get_env_vars(&self) -> HashMap<String, String> {
        self.env.clone()
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
        self.priority.clone()
    }

    pub fn get_scheduler(&self) -> Option<String> {
        self.scheduler.clone()
    }
}

fn parse_bool_value(yaml: &Yaml, key: &str) -> Result<bool, Error> {
    match yaml[key] {
        Yaml::Boolean(b) => Ok(b),
        Yaml::BadValue => Ok(false),
        _ => Err(Error::new(
            ErrorKind::Other,
            format!(
                "Invalid value for `launcher.{}` value: boolean expected",
                key
            ),
        )),
    }
}

fn parse_debug(config: &Vec<Yaml>) -> Result<bool, Error> {
    parse_bool_value(&config[0]["launcher"], "debug")
}

fn parse_clear_env(config: &Vec<Yaml>) -> Result<bool, Error> {
    parse_bool_value(&config[0]["launcher"], "clear_env")
}

fn parse_qemu_binary(config: &Vec<Yaml>) -> Result<String, Error> {
    let config = &config[0];
    match config["launcher"]["binary"].as_str() {
        Some(bin) => Ok(bin.to_string()),
        None => Err(Error::new(
            ErrorKind::Other,
            "qemu binary path is not specified, missing or invalid `launcher.binary` key.",
        )),
    }
}

fn parse_env(config: &Vec<Yaml>) -> Result<HashMap<String, String>, Error> {
    let config = &config[0];

    match &config["launcher"]["env"] {
        Yaml::Hash(h) => parse_env_hash(h),
        Yaml::BadValue => Ok(HashMap::new()),
        _ => Err(Error::new(
            ErrorKind::Other,
            "Invalid value for the `launcher.env` key, hash expected.",
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
                    ErrorKind::Other,
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
                    ErrorKind::Other,
                    format!("Invalid value for the `{}` environment variable.", name,),
                ))
            }
        };

        env_vars.insert(name, value);
    }

    Ok(env_vars)
}

fn parse_user(config: &Vec<Yaml>) -> Option<u32> {
    let config = &config[0];
    match config["launcher"]["user"].as_i64() {
        Some(s) => Some(s as u32),
        None => None,
    }
}

fn parse_group(config: &Vec<Yaml>) -> Option<u32> {
    let config = &config[0];
    match config["launcher"]["group"].as_i64() {
        Some(s) => Some(s as u32),
        None => None,
    }
}

fn parse_priority(config: &Vec<Yaml>) -> Result<Option<u8>, Error> {
    let config = &config[0];
    match config["launcher"]["priority"] {
        Yaml::Integer(i) => Ok(Some(i as u8)),
        Yaml::BadValue => Ok(None),
        _ => Err(Error::new(
            ErrorKind::Other,
            format!("Failed to parse `launcher.priority`: integer expected."),
        )),
    }
}

fn parse_scheduler(config: &Vec<Yaml>) -> Result<Option<String>, Error> {
    let config = &config[0];
    match &config["launcher"]["scheduler"] {
        Yaml::String(s) => match s.as_str() {
            "batch" | "deadline" | "fifo" | "idle" | "other" | "rr" => Ok(Some(s.to_string())),
            _ => Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to parse `launcher.scheduler`: Expected one of \
                    `batch`, `deadline`, `fifo`, `idle`, `other` or `rr`."
                ),
            )),
        },
        Yaml::BadValue => Ok(None),
        _ => Err(Error::new(
            ErrorKind::Other,
            format!("Failed to parse `launcher.scheduler`: string expected."),
        )),
    }
}

fn parse_cpu_pinning_sockets(sockets: &Hash) -> Result<Vec<(usize, usize, usize, usize)>, Error> {
    let mut cpu_pinning = vec![];

    for (socket, cores) in sockets {
        let socket_id = socket.as_i64().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to parse `vcpu_pinning`: the socket ID must be a positive integer."
                ),
            )
        })? as usize;

        match cores {
            Yaml::Hash(cores) => {
                cpu_pinning.append(&mut parse_cpu_pinning_cores(socket_id, cores)?)
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "Failed to parse `vcpu_pinning.{}`: hash expected.",
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
        let core_id = core.as_i64().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to parse `vcpu_pinning.{}`: the core ID must be a positive integer.",
                    socket_id
                ),
            )
        })? as usize;

        match threads {
            Yaml::Hash(threads) => {
                cpu_pinning.append(&mut parse_cpu_pinning_threads(socket_id, core_id, threads)?)
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "Failed to parse `vcpu_pinning.{}.{}`: hash expected",
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
        let thread_id = thread.as_i64().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                format!(
                "Failed to parse `vcpu_pinning.{}.{}`: the thread ID must be a positive integer.",
                socket_id, core_id
            ),
            )
        })? as usize;

        let host_id = host.as_i64().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                format!(
                "Failed to parse `vcpu_pinning.{}.{}.{}`: host core ID must be a positive integer.",
                socket_id, core_id, thread_id
            ),
            )
        })? as usize;

        cpu_pinning.push((socket_id, core_id, thread_id, host_id));
    }

    Ok(cpu_pinning)
}

fn parse_cpu_pinning(config: &Vec<Yaml>) -> Result<Vec<(usize, usize, usize, usize)>, Error> {
    let config = &config[0];

    match &config["launcher"]["vcpu_pinning"] {
        Yaml::Hash(sockets) => parse_cpu_pinning_sockets(sockets),
        Yaml::BadValue => Ok(vec![]),
        _ => {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Failed to parse `vcpu_pinning` configuration."),
            ))
        }
    }
}

fn parse_command_line(config: &Vec<Yaml>) -> Result<Vec<Argument>, Error> {
    match &config[0]["qemu"] {
        Yaml::Array(options) => parse_command_line_options(options),
        _ => Err(Error::new(
            ErrorKind::Other,
            format!("Failed to parse qemu command line options."),
        )),
    }
}

fn parse_command_line_options(options: &Array) -> Result<Vec<Argument>, Error> {
    let mut parsed_options = vec![];

    for option in options {
        match option {
            Yaml::String(option) => parsed_options.push(Argument::Flag(option.to_owned())),
            Yaml::Hash(option) => parsed_options.push(parse_parameter(option)?),
            _ => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "Failed to parse qemu command line options. \
                             Every option must be a string or a hash."
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

fn parse_parameter(option: &Hash) -> Result<Argument, Error> {
    if option.len() != 1 {
        return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Found a command line argument with {} pairs in the hash, but exactly one is expected",
                    option.len()
                ),
            ));
    }

    let entry = option.front().unwrap();

    let name = entry
        .0
        .as_str()
        .ok_or_else(|| Error::new(ErrorKind::Other, format!("Argument name must be a string.")))?
        .to_string();

    let value = match entry.1 {
        Yaml::Integer(i) => i.to_string(),
        Yaml::String(s) => s.to_string(),
        Yaml::Real(r) => r.to_string(),
        Yaml::Array(v) => parse_parameter_value(&name, v)?,
        _ => {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Invalid value for `{}` argument: expected a string, integer, \
                        real or a hash with a single pair",
                    name
                ),
            ))
        }
    };

    Ok(Argument::Parameter(name, value))
}

fn parse_parameter_value(name: &str, values: &Array) -> Result<String, Error> {
    if values.len() == 0 {
        return Err(Error::new(
            ErrorKind::Other,
            format!(
                "Empty value for `{}` argument, consider using a string instead of an array",
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
                    ErrorKind::Other,
                    format!(
                    "Invalid value for `{}` option: must be a hash with a single pair or a string",
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
            ErrorKind::Other,
            format!(
                "Failed to parse a value for `{}` argument: a hash with multiple pairs found",
                name
            ),
        ));
    }

    let entry = part.front().unwrap();

    let part_name = entry.0.as_str().ok_or_else(|| {
        Error::new(
            ErrorKind::Other,
            format!(
                "Failed to parse a value for `{}` argument: key must be a string",
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
                ErrorKind::Other,
                format!(
                    "Failed to parse a value for `{}/{}` argument: value must be a string",
                    name, part_name
                ),
            ))
        }
    };

    Ok(format!("{}={}", part_name, part_value))
}
