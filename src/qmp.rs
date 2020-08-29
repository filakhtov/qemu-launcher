use json::{object, JsonValue};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Error, ErrorKind, Read, Write},
};

pub trait QmpPipe: Read + Write {}

pub struct StdioReadWrite<'a> {
    stdin: &'a mut dyn Write,
    stdout: &'a mut dyn Read,
}

impl<'a> StdioReadWrite<'a> {
    pub fn new(stdin: &'a mut impl Write, stdout: &'a mut impl Read) -> Self {
        Self {
            stdin: stdin,
            stdout: stdout,
        }
    }
}

impl Read for StdioReadWrite<'_> {
    fn read(&mut self, message: &mut [u8]) -> Result<usize, Error> {
        self.stdout.read(message)
    }
}

impl Write for StdioReadWrite<'_> {
    fn write(&mut self, message: &[u8]) -> Result<usize, Error> {
        self.stdin.write(message)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.stdin.flush()
    }
}

impl QmpPipe for StdioReadWrite<'_> {}

struct QmpClient<'a> {
    io: Box<dyn QmpPipe + 'a>,
    negotiated: bool,
}

impl<'a> QmpClient<'a> {
    pub fn new(io: impl QmpPipe + 'a) -> QmpClient<'a> {
        QmpClient {
            io: Box::new(io),
            negotiated: false,
        }
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
                    "Missing `QMP.capabilities` field in the welcome QMP message: {}.",
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
        match BufReader::new(&mut (self.io)).read_line(&mut qmp_response) {
            Ok(_) => {}
            Err(e) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("Failed to read welcome message from QMP socket: `{}`.", e),
                ))
            }
        }

        match json::parse(&qmp_response) {
            Ok(r) => Ok(r),
            Err(e) => Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to parse QMP response `{}`, error: `{}`.",
                    qmp_response, e
                ),
            )),
        }
    }

    fn send_command(&mut self, json: JsonValue) -> Result<JsonValue, Error> {
        self.io.write_all(json.dump().as_bytes())?;
        self.io.flush()?;
        let mut response = self.read_message()?;

        if !response["error"].is_null() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "Received error QMP response: `{}`.",
                    response["error"]["desc"]
                ),
            ));
        }

        if response["return"].is_null() {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Missing `return` field in response: `{}`.", response),
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
                    "Error parsing QMP response for `query-cpus-fast`, \
                    expected an array, but got: `{}`.",
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
                Some(core) => core.get(&thread_id).cloned(),
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

fn transform_vcpu_info(json_response: &Vec<JsonValue>) -> Result<Topology, Error> {
    let mut topology = HashMap::new();

    for (id, cpu) in json_response.iter().enumerate() {
        let task_id = cpu["thread-id"].as_usize().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                format!(
                    "`return.{}.thread-id` is invalid, a \
                    positive number is expected, but got: `{}`.",
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
                            "`return.{}.props.core-id` is invalid, \
                            a positive number is expected, but got: `{}`.",
                            id, props["core-id"]
                        ),
                    )
                })?;
                let thread_id = props["thread-id"].as_usize().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Other,
                        format!(
                            "`return.{}.props.thread-id` is invalid, a \
                            positive number is expected, but got: `{}`.",
                            id, props["thread-id"]
                        ),
                    )
                })?;
                let socket_id = props["socket-id"].as_usize().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Other,
                        format!(
                            "`return.{}.props.socket-id` is invalid, a \
                            positive number is expected, but got: `{}`.",
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
                        "Invalid vCPU info, expected `props` to be an object, but got: `{}`.",
                        cpu["props"]
                    ),
                ))
            }
        }
    }

    Ok(Topology { topology: topology })
}

pub fn read_vcpu_info_from_qmp_socket(io: impl QmpPipe) -> Result<Topology, Error> {
    transform_vcpu_info(&QmpClient::new(io).query_cpus_fast()?)
}

#[cfg(test)]
mod test {
    use super::{read_vcpu_info_from_qmp_socket, QmpPipe, Topology};
    use json::{object, JsonValue};
    use std::io::{Error, ErrorKind, Read, Write};

    struct MockQmpPipe {
        reads: Vec<Option<String>>,
        writes: Vec<(String, bool)>,
        flushes: Vec<bool>,
    }

    impl MockQmpPipe {
        fn new(
            mut reads: Vec<Option<String>>,
            mut writes: Vec<(String, bool)>,
            mut flushes: Vec<bool>,
        ) -> Self {
            reads.reverse();
            writes.reverse();
            flushes.reverse();

            MockQmpPipe {
                reads: reads,
                writes: writes,
                flushes: flushes,
            }
        }
    }

    impl Read for MockQmpPipe {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
            let invocation = match self.reads.pop() {
                Some(v) => v,
                None => panic!("Unexpected call to MockQmpPipe::read() method."),
            };

            if let Some(message) = invocation {
                return message.as_bytes().read(buf);
            }

            Err(Error::new(ErrorKind::Other, "MockQmpPipe::read()"))
        }
    }

    impl Write for MockQmpPipe {
        fn write(&mut self, message: &[u8]) -> Result<usize, Error> {
            let invocation = match self.writes.pop() {
                Some(v) => v,
                None => panic!(
                    "Unexpected call to MockQmpPipe::write() method: {:?}",
                    message
                ),
            };

            if !invocation.1 {
                return Err(Error::new(ErrorKind::Other, "MockQmpPipe::write()"));
            }

            if invocation.0.as_bytes() != message {
                panic!(
                    "Unexpected call to MockQmpPipe::write(): expected `{:?}`, got `{:?}`",
                    invocation.0.as_bytes(),
                    message
                );
            }

            return Ok(message.len());
        }

        fn flush(&mut self) -> Result<(), Error> {
            let invocation = match self.flushes.pop() {
                Some(v) => v,
                None => panic!("Unexpected call to MockQmpPipe::flush() method."),
            };

            if invocation {
                return Ok({});
            }

            Err(Error::new(ErrorKind::Other, "MockQmpPipe::flush()"))
        }
    }

    impl Drop for MockQmpPipe {
        fn drop(&mut self) {
            if self.reads.len() > 0 {
                panic!(
                    "{} more MockQmpPipe::read() method call was/were expected.",
                    self.reads.len()
                )
            }

            if self.writes.len() > 0 {
                panic!(
                    "{} more MockQmpPipe::write() method call was/were expected.",
                    self.writes.len()
                )
            }

            if self.flushes.len() > 0 {
                panic!(
                    "{} more MockQmpPipe::flush() method call was/were expected.",
                    self.flushes.len()
                )
            }
        }
    }

    impl QmpPipe for MockQmpPipe {}

    fn assert_error(result: Result<Topology, Error>, kind: ErrorKind, message: &str) {
        if let Err(error) = result {
            assert_eq!(kind, error.kind());
            assert_eq!(message, format!("{}", error));

            return;
        }

        panic!("Expected an error, got the Ok() result");
    }

    fn create_successful_mock_qmp_pipe(payload: JsonValue) -> MockQmpPipe {
        MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "return": {} }).dump() + "\n"),
                Some(payload.dump() + "\n"),
            ],
            vec![
                ((object! { "execute": "qmp_capabilities" }).dump(), true),
                ((object! { "execute": "query-cpus-fast" }).dump(), true),
            ],
            vec![true, true],
        )
    }

    #[test]
    fn read_vcpu_info_returns_json_information() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": 25627,
                    "props": { "core-id": 0, "thread-id": 0, "socket-id": 0 },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
                {
                    "thread-id": 25628,
                    "props": { "core-id": 0, "thread-id": 1, "socket-id": 0 },
                    "qom-path": "/machine/unattached/device[2]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 1
                }
            ]
        });

        let topology = read_vcpu_info_from_qmp_socket(io).unwrap();

        let mut task_ids = topology.get_task_ids();
        task_ids.sort();

        assert_eq!(vec![25627, 25628], task_ids);

        assert_eq!(Some(25627), topology.get_thread_id(0, 0, 0));
        assert_eq!(Some(25628), topology.get_thread_id(0, 0, 1));

        assert_eq!(None, topology.get_thread_id(1, 0, 0));
        assert_eq!(None, topology.get_thread_id(0, 1, 0));
        assert_eq!(None, topology.get_thread_id(0, 0, 2));
    }

    #[test]
    fn read_vcpu_info_returns_error_if_negotiation_read_fails() {
        let io = MockQmpPipe::new(vec![None], vec![], vec![]);
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Failed to read welcome message from QMP socket: `MockQmpPipe::read()`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_negotiation_json_parsing_fails() {
        let io = MockQmpPipe::new(
            vec![Some(String::from("this is not a json\n"))],
            vec![],
            vec![],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Failed to parse QMP response `this is not a json\n`, \
            error: `Unexpected character: h at (1:2)`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_negotiation_json_is_invalid() {
        let io = MockQmpPipe::new(vec![Some(String::from("{}\n"))], vec![], vec![]);
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Missing `QMP.capabilities` field in the welcome QMP message: {}.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_negotiation_write_fails() {
        let io = MockQmpPipe::new(
            vec![Some(
                (object! {
                    "QMP": {
                        "version": {
                            "qemu": { "micro": 0, "minor": 6, "major": 1 },
                            "package": ""
                        },
                        "capabilities": []
                    }
                })
                .dump()
                    + "\n",
            )],
            vec![(
                (object! {
                    "execute": "qmp_capabilities"
                })
                .dump(),
                false,
            )],
            vec![],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(result, ErrorKind::Other, "MockQmpPipe::write()");
    }

    #[test]
    fn read_vcpu_info_returns_error_if_negotiation_flush_fails() {
        let io = MockQmpPipe::new(
            vec![Some(
                (object! {
                    "QMP": {
                        "version": {
                            "qemu": { "micro": 0, "minor": 6, "major": 1 },
                            "package": ""
                        },
                        "capabilities": []
                    }
                })
                .dump()
                    + "\n",
            )],
            vec![((object! { "execute": "qmp_capabilities" }).dump(), true)],
            vec![false],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(result, ErrorKind::Other, "MockQmpPipe::flush()");
    }

    #[test]
    fn qeuery_cpu_fast_returns_error_if_negotiation_response_read_fails() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                None,
            ],
            vec![((object! { "execute": "qmp_capabilities" }).dump(), true)],
            vec![true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Failed to read welcome message from QMP socket: `MockQmpPipe::read()`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_json_response_contains_error_message() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some(
                    (object! { "error": {
                        "class": "GenericError",
                        "desc": "negotiation failed",
                    } })
                    .dump()
                        + "\n",
                ),
            ],
            vec![((object! { "execute": "qmp_capabilities" }).dump(), true)],
            vec![true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Received error QMP response: `negotiation failed`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_json_response_contains_no_return_field() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "test": "value" }).dump() + "\n"),
            ],
            vec![((object! { "execute": "qmp_capabilities" }).dump(), true)],
            vec![true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Missing `return` field in response: `{\"test\":\"value\"}`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_sending_command_write_fails() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "return": {} }).dump() + "\n"),
            ],
            vec![
                ((object! { "execute": "qmp_capabilities" }).dump(), true),
                ((object! { "execute": "query-cpus-fast" }).dump(), false),
            ],
            vec![true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(result, ErrorKind::Other, "MockQmpPipe::write()");
    }

    #[test]
    fn read_vcpu_info_returns_error_if_sending_command_flush_fails() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "return": {} }).dump() + "\n"),
            ],
            vec![
                ((object! { "execute": "qmp_capabilities" }).dump(), true),
                ((object! { "execute": "query-cpus-fast" }).dump(), true),
            ],
            vec![true, false],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(result, ErrorKind::Other, "MockQmpPipe::flush()");
    }

    #[test]
    fn read_vcpu_info_returns_error_if_reading_fails_after_sending_command() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "return": {} }).dump() + "\n"),
                None,
            ],
            vec![
                ((object! { "execute": "qmp_capabilities" }).dump(), true),
                ((object! { "execute": "query-cpus-fast" }).dump(), true),
            ],
            vec![true, true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Failed to read welcome message from QMP socket: `MockQmpPipe::read()`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_parsing_json_response_fails() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "return": {} }).dump() + "\n"),
                Some(String::from("not a JSON!\n")),
            ],
            vec![
                ((object! { "execute": "qmp_capabilities" }).dump(), true),
                ((object! { "execute": "query-cpus-fast" }).dump(), true),
            ],
            vec![true, true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Failed to parse QMP response `not a JSON!\n`, \
            error: `Unexpected character: o at (1:2)`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_qemu_returns_error_response() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "return": {} }).dump() + "\n"),
                Some(
                    (object! { "error": {
                        "class": "GenericError",
                        "desc": "query failed",
                    } })
                    .dump()
                        + "\n",
                ),
            ],
            vec![
                ((object! { "execute": "qmp_capabilities" }).dump(), true),
                ((object! { "execute": "query-cpus-fast" }).dump(), true),
            ],
            vec![true, true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Received error QMP response: `query failed`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_return_field_is_missing_from_qemu_response() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "return": {} }).dump() + "\n"),
                Some((object! { "wrong_response": {} }).dump() + "\n"),
            ],
            vec![
                ((object! { "execute": "qmp_capabilities" }).dump(), true),
                ((object! { "execute": "query-cpus-fast" }).dump(), true),
            ],
            vec![true, true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Missing `return` field in response: `{\"wrong_response\":{}}`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_return_field_is_not_an_array() {
        let io = MockQmpPipe::new(
            vec![
                Some(
                    (object! {
                        "QMP": {
                            "version": {
                                "qemu": { "micro": 0, "minor": 6, "major": 1 },
                                "package": ""
                            },
                            "capabilities": []
                        }
                    })
                    .dump()
                        + "\n",
                ),
                Some((object! { "return": {} }).dump() + "\n"),
                Some((object! { "return": { "cpus": [] } }).dump() + "\n"),
            ],
            vec![
                ((object! { "execute": "qmp_capabilities" }).dump(), true),
                ((object! { "execute": "query-cpus-fast" }).dump(), true),
            ],
            vec![true, true],
        );
        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Error parsing QMP response for `query-cpus-fast`, \
            expected an array, but got: `{\"cpus\":[]}`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_process_id_is_not_an_integer() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": "string",
                    "props": { "core-id": 0, "thread-id": 0, "socket-id": 0 },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "`return.0.thread-id` is invalid, a positive \
            number is expected, but got: `string`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_process_id_is_negative() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": -33,
                    "props": { "core-id": 0, "thread-id": 0, "socket-id": 0 },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "`return.0.thread-id` is invalid, a positive number is expected, but got: `-33`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_vcpu_properties_are_invalid() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": 2418,
                    "props": [],
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "Invalid vCPU info, expected `props` to be an object, but got: `[]`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_vcpu_core_id_is_not_an_integer() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": 5974,
                    "props": { "core-id": "wrong", "thread-id": 0, "socket-id": 0 },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "`return.0.props.core-id` is invalid, a \
            positive number is expected, but got: `wrong`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_vcpu_core_id_is_negative() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": 6894,
                    "props": { "core-id": -1, "thread-id": 0, "socket-id": 0 },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "`return.0.props.core-id` is invalid, a \
            positive number is expected, but got: `-1`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_vcpu_thread_id_is_not_an_integer() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": 24789,
                    "props": { "core-id": 0, "thread-id": "tid0", "socket-id": 0 },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "`return.0.props.thread-id` is invalid, a \
            positive number is expected, but got: `tid0`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_vcpu_thread_id_is_negative() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": 9514,
                    "props": { "core-id": 0, "thread-id": -1, "socket-id": 0 },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "`return.0.props.thread-id` is invalid, a \
            positive number is expected, but got: `-1`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_vcpu_socket_id_is_not_an_integer() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": 8711,
                    "props": { "core-id": 0, "thread-id": 0, "socket-id": "sockid0" },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "`return.0.props.socket-id` is invalid, a \
            positive number is expected, but got: `sockid0`.",
        );
    }

    #[test]
    fn read_vcpu_info_returns_error_if_vcpu_socket_id_is_negative() {
        let io = create_successful_mock_qmp_pipe(object! { "return": [
                {
                    "thread-id": 6152,
                    "props": { "core-id": 0, "thread-id": 0, "socket-id": -2 },
                    "qom-path": "/machine/unattached/device[0]",
                    "arch":"x86",
                    "target":"x86_64",
                    "cpu-index": 0
                },
            ]
        });

        let result = read_vcpu_info_from_qmp_socket(io);

        assert_error(
            result,
            ErrorKind::Other,
            "`return.0.props.socket-id` is invalid, a \
            positive number is expected, but got: `-2`.",
        );
    }
}
