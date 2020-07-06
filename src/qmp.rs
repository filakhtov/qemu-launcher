extern crate json;

use json::{object, JsonValue};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Error, ErrorKind, Write},
    process::{Child, ChildStdin, ChildStdout},
};

struct QmpClient<'a> {
    child: &'a mut Child,
    negotiated: bool,
}

impl<'a> QmpClient<'a> {
    pub fn new(child: &'a mut Child) -> Result<QmpClient, Error> {
        Ok(QmpClient {
            child: child,
            negotiated: false,
        })
    }

    fn get_stdout(&mut self) -> Result<&mut ChildStdout, Error> {
        self.child
            .stdout
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Other, "Failed to capture child standard output."))
    }

    fn get_stdin(&mut self) -> Result<&mut ChildStdin, Error> {
        self.child
            .stdin
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Other, "Failed to capture child standard input."))
    }

    fn negotiate_capabilities(&mut self) -> Result<(), Error> {
        if self.negotiated {
            return Ok({});
        }

        let response = self.read_message()?;

        if response["QMP"]["capabilities"].is_null() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Missing `QMP.capabilities` field in the welcome QMP message: {}",
                    response
                ),
            ));
        }

        self.send_command(object! {"execute": "qmp_capabilities"})?;

        self.negotiated = true;

        return Ok({});
    }

    fn read_message(&mut self) -> Result<JsonValue, Error> {
        let mut qmp_response = String::new();
        match BufReader::new(self.get_stdout()?).read_line(&mut qmp_response) {
            Ok(_) => {}
            Err(e) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("Failed to read welcome message from stdout: `{}`.", e),
                ))
            }
        }

        match json::parse(&qmp_response) {
            Ok(r) => Ok(r),
            Err(e) => Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to parse QMP response `{}`, error: `{}`",
                    qmp_response, e
                ),
            )),
        }
    }

    fn send_command(&mut self, json: JsonValue) -> Result<JsonValue, Error> {
        let stdin = self.get_stdin()?;
        stdin.write_all(json.dump().as_bytes())?;
        stdin.flush()?;
        let mut response = self.read_message()?;

        if !response["error"].is_null() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Received error QMP response: `{}`",
                    response["error"]["desc"]
                ),
            ));
        }

        if response["return"].is_null() {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Missing `return` field in response: `{}`", response),
            ));
        }

        Ok(response["return"].take())
    }

    pub fn query_cpus_fast(&mut self) -> Result<std::vec::Vec<JsonValue>, Error> {
        self.negotiate_capabilities()?;

        let response = self.send_command(object! {"execute": "query-cpus-fast"})?;

        match response {
            JsonValue::Array(cpus) => Ok(cpus),
            _ => Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Error parsing QMP response for `query-cpus-fast`, expected an array, but got: {}",
                    response
                ),
            )),
        }
    }
}

pub struct Topology {
    topology: HashMap<usize, HashMap<usize, HashMap<usize, usize>>>,
}

impl Topology {
    pub fn get_thread_id(
        &self,
        socket_id: usize,
        core_id: usize,
        thread_id: usize,
    ) -> Option<usize> {
        match self.topology.get(&socket_id) {
            Some(socket) => match socket.get(&core_id) {
                Some(core) => match core.get(&thread_id) {
                    Some(thread) => Some(*thread),
                    None => None,
                },
                None => None,
            },
            None => None,
        }
    }

    pub fn get_task_ids(&self) -> Vec<usize> {
        let mut task_ids = vec![];

        for (_, cores) in &self.topology {
            for (_, threads) in cores {
                for (_, thread) in threads {
                    task_ids.push(*thread);
                }
            }
        }

        task_ids
    }
}

pub fn read_vcpu_info(child: &mut Child) -> Result<Topology, Error> {
    let mut client = QmpClient::new(child)?;
    let cpus = client.query_cpus_fast()?;
    let mut topology = HashMap::new();

    for (id, cpu) in cpus.iter().enumerate() {
        let task_id = cpu["thread-id"].as_usize().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                format!(
                    "`return.{}.thread-id` is invalid, a number is expected, but got: `{}`",
                    id, cpu["thread-id"]
                ),
            )
        })?;
        match &cpu["props"] {
            JsonValue::Object(props) => {
                let core_id = props["core-id"].as_usize().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Other,
                        format!(
                            "`return.{}.props.core-id` is invalid, a number is expected, but got: `{}`",
                            id, props["core-id"]
                        ),
                    )
                })?;
                let thread_id = props["thread-id"].as_usize().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Other,
                        format!(
                            "`return.{}.props.thread-id` is invalid, a number is expected, but got: `{}`",
                            id, props["thread-id"]
                        ),
                    )
                })?;
                let socket_id = props["socket-id"].as_usize().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Other,
                        format!(
                            "`return.{}.props.socket-id` is invalid, a number is expected, but got: `{}`",
                            id, props["socket-id"]
                        ),
                    )
                })?;

                topology
                    .entry(socket_id)
                    .or_insert(HashMap::new())
                    .entry(core_id)
                    .or_insert(HashMap::new())
                    .insert(thread_id, task_id);
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "Invalid vCPU info, expected `props` to be an object, but got: `{}`",
                        cpu["props"]
                    ),
                ))
            }
        }
    }

    Ok(Topology { topology: topology })
}
