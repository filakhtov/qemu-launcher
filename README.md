# qemu-launcher
A minimalistic tool designed to run qemu-based virtual machines based on definitions stored in configuration files
with support for vCPU pinning.

## How this project was born
Qemu allows to specify all necessary configuration flags via command line arguments, for example:

```
qemu -M q35 -m 8G -cpu host -smp 4 ...
```

but this is very inconvenient. Qemu also supports its own configuration file format, however it is incomplete and
does not allow to set all the options that are supported via command line arguments.

Initial approach taken to resolve this was using a collection of shell scripts that had all the needed options
listed, but after some time, as an amount of virtual machines needed has grown, this approach become a little
cumbersome, so the decision was made to write a [tiny C++ application](https://github.com/filakhtov/qemu-wrapper)
that accepts a configuration file name and reads a list of command line options to pass to the `qemu` binary. This
served the purpose well for a while.

Lately, however a need to run high performance and low latency virtual machines arose, and in order to achieve
this, CPU pinning was identified as a requirement. Libvirt was used for a while at the beginning, but it is
extremely heavy weight, very opinionated on how things are arranged, depends on a significant amount of
dependencies and is simply overkill for the use case at hand.

Hence, this project was born - an evolution of previously mentioned `qemu-wrapper`, rewritten in Rust and extended
to support CPU pinning functionality. And as a pleasant bonus, instead of the homegrown configuration file format
it uses Yaml.

## Usage
```sh
qemu-launcher foo
```

will attempt to load the `foo.yml` file from the configuration directory (which is `/usr/local/etc/qemu-launcher`
by default) and launch the virtual machine defined that file. A path to the directory where virtual machine
definition files are located can be changed by setting the `QEMU_LAUNCHER_CONFIG_DIR` environment variable, e.g.:

```sh
QEMU_LAUNCHER_CONFIG_DIR="/etc/my-vms" qemu-launcher bar
```

which will attempt to load the `/etc/my-vms/bar.yml` configuration file. Additionally, when using vCPU pinning in
the configuration file, `qemu-launcher` will attempt to create the directory, if it does not exist, and mount the
cpuset cgroup tree under the `/sys/fs/cgroup/cpuset` path. This path can be controlled by setting another
environment variable - `QEMU_LAUNCHER_CPUSET_MOUNT_PATH`. By default, the qemu launcher will create the `qemu`
prefix subdirectory under the mount path. This can be controlled by the `QEMU_LAUNCHER_CPUSET_PREFIX` environment
variable. It will then create a `pool` subdirectory inside of the prefix, which will use only non-pinned cores and
an additional directories will be created for each pinned core as needed. All running tasks are migrated to the
`pool` cpuset and only the qemu virtual machine vCPU threads are pinned to the core-specific sets.

## Configuration file format
All virtual machine configuration files should be stored in a single directory and must use the `.yml` file
extension. There are two top level keys supported:

- `launcher` - to control the launcher settings itself;
- `qemu` - to specify command line options to be passed to qemu binary;

### Launcher configuration
All keys but `binary` in the `launcher` section are optional. The following keys are supported:

- `binary` - string, mandatory. Used to specify the name, or full path if the binary is not in the default `$PATH`
variable, of the qemu emulator binary.
- `clear_env` - boolean, optional, defaults to `false`. If set to true, the environment variables of the
`qemu-launcher` process will not be forwarded to the qemu child process.
- `env` - hash, optional. Allows to provide additional environment variables for the child qemu process. Example:

```yaml
env:
  # Set the audio driver for qemu to use PulseAudio
  QEMU_AUDIO_DRV: pa
```

- `debug` - boolean, optional, default to `false`. Controls where additional debugging information should be
printed by the `qemu-launcher`, such as vCPU pinning mapping.
- `user` - integer, optional. Set an effective user ID that will be used to launch the qemu child process. This can
be useful when the `qemu-launcher` is executed with elevated privileges, i.e. when using vCPU pinning feature.
- `group` - integer, optional. Same as `user`, but setting the effective group ID for the child process.
- `priority` - integer, optional. Does not work if the `scheduler` is not specified. Set a priority to be set using
`chrt` for each of the vCPU threads (requires elevated privileges).
- `scheduler` - string, optional. Must be one of `batch`, `deadline`, `fifo`, `idle`, `other` or `rr`. Does not
work if the `priority` option is not specified. Set a policy using the `chrt` for each of the vCPU threads
(requires elevated privileges).
- `vcpu_pinning` - hash, optional. Configures how to pin threads responsible for each vCPU core to a logical
processor of the hypervisor machine. First dimension matches the `socket` of the virtual machine processor, second
matches the `core` and third matches the `thread`, for example:

```yaml
vcpu_pinning:
  # vCPU socket 0
  0:
    # vCPU core 0
    0:
      # vCPU thread 0 : host logical CPU 2
      0: 2
      1: 6
    1:
      0: 3
      1: 7
```

the thread 0 of the core 0 on the socket 0 will be pinned to the logical host processor 2, thread 1 core 0 socket 0
to 6, thread 0 core 1 socket 0 to 3 and thread 1 core 1 socket 0 to 7.
- `rlimit_memlock` - boolean, optional, defaults to `false`. When set to `true` the `qemu-launcher` will change an
amount of memory that can be locked by the `qemu` process to `unlimited`, using the `setrlimit(2)` system call.
Both, soft and hard limits are unset. This is necessary for systems that have a low limit set by default for the
amount of memory that a single process can lock.

#### An important note on vCPU pinning
In order to achieve the best possible virtual machine performance, it is necessary to match the number of threads
per core of the virtual machine to the threads per core of the hypervisor, and for hyper-threaded processors, vCPU
threads of the same virtual core should be allocated to the logical cores of the same host core. For example,
looking at the `/proc/cpuinfo` of a random host machine:

```sh
$ cat /proc/cpuinfo
processor	: 0
...
core id		: 0
...

processor	: 1
...
core id		: 1
...

processor	: 2
...
core id		: 0
...

processor	: 3
...
core id		: 1
...
```

note how core id is the same for logical processor 0 and 2, meaning that they are _threads_ of the same _core_. So
if we to allocate a single multi-threaded vCPU core, it is best to map it like:

```yaml
vcpu_pinning:
  0:
    0:
      0: 0
      1: 2
```

or

```yaml
vcpu_pinning:
  0:
    0:
      0: 1
      1: 3
```

and the latter is actually preferred, because the logical processor 0 in Linux usually has some tasks associated
with it, such as handling IRQs and should be avoided to achieve the maximum performance.

### Qemu command line options
This section is largely driven by the supported qemu options and can be easily translated back and forth. The best
way to explain this would be using the following example:

```sh
qemu -name "my virtual machine" -machine pc,accel=kvm -nographic -monitor unix:/run/my-vm.sock -cpu host \
     -smp 2,sockets=1,cores=1,threads=2 -m 2G -device ide-hd,bus=ide.0,drive=drive0 \
     -drive file=/var/storage/my-vm.qcow,if=none,id=drive0,format=qcow,readonly
```

would translate into the following YAML definition:
```yaml
qemu:
- name: my virtual machine
- machine: [ pc, accel: kvm ]
- nographic
- monitor: [ unix:/run/my-vm.sock ]
- cpu: host
- smp: [ 2, sockets: 1, cores: 1, threads: 1 ]
- m: 2G
- device: [ ide-hd, bus: ide.0, drive: drive0 ]
- drive: [ file: /var/storage/my-vm.qcow, if: none, id: drive0, format: qcow, readonly ]
```

these options can equally be written in the following way (all argument values are just string):

```yaml
qemu:
- name: my virtual machine
- machine: pc,accel=kvm
- nographic
- monitor: unix:/run/my-vm.sock
- cpu: host
- smp: 2,sockets=1,cores=1,threads=1
- m: 2G
- device: ide-hd,bus=ide.0,drive=drive0
- drive: file=/var/storage/my-vm.qcow,if=none,id=drive0,format=qcow,readonly
```

or even in the mixed way:

```yaml
qemu:
- name: [ my virtual machine ] # Array with one element
- machine:
  # Array of strings written in the more verbose way
  - pc,
  - accel=kvm
# Simple flag parameter
- nographic
- monitor:
  - unix:/run/my-vm.sock
# This is simila to the name key, but using a different YAML syntax
- cpu:
  - host
- smp:
  # Simple integer
  - 2
  # Explicit hash
  - { sockets: 1 }
  # Implicit hash
  - cores: 1
  - { threads: 1 }
- m: 2G
- device: ide-hd,bus=ide.0,drive=drive0
- drive: file=/var/storage/my-vm.qcow,if=none,id=drive0,format=qcow,readonly
```

First one though is easier to read and understand when compared to others.

## Possible aproaches of handling elevated privileges
To achieve the best performance possible it is necessary to use vCPU pinning together with custom scheduler and
higher thread priorities. In order to be able to perform these operations `qemu-launcher` has to be executed with
elevated privileges.

Simplest way to do so is to execute the `qemu-launcher` binary directly as the `root` user, or using tools like
`sudo` or `pkexec`. Yet another option is to set the SETUID bit on the binary using `chmod u+s qemu-launcher`
command.

For most security-conscious users, it is possible to use the pre-mounted `cpuset` tree and allow all users who need
to execute the `qemu-launcher` an access to modify cpuset entries and allow changing process priorities if
necessary.

## How it all works
For those who are interested in the high level overview of how this tool works, there is a short summary. The
`qemu-launcher` binary first loads the YAML definition file for the specified virtual machine, compiles the list of
command line options for qemu, appends the `-qmp stdio` option and launches the child qemu process, setting the
effective user and group IDs for it and manipulating the environment variable as desired.

If the vCPU pinning is specified, `qemu-launcher` will negotiate capabilities using the QMP protocol (JSON control
protocol provided by the qemu) using stdio of the child process and execute the `query-cpus-fast` command to obtain
the list of the qemu process threads responsible for each vCPU socket/core/thread triplet. It will then proceeds to
check if the cpuset cgroup tree is mounted under the specified mount point, mounting it if it is not. Next the a
prefix (`qemu` by default) directory where all cpusets will be hosted is created and all cores available on the
hypervisor are allocated to this prefix. The `pool` directory is created, representing a pool of available
(non-pinned) host CPU threads and all currently running tasks are moved into this new `pool` cpuset. Then a
separate cpu sets (subdirectories) for each logical core to which threads will be pinned are created and respective
logical core id is removed from the `pool` cpu set and written into the cpu set dedicated for this core, and
finally moves the vCPU thread over to the newly created cpu set to shield it from other tasks.

If the `priority` and the `scheduler` options of the `launcher` configuration section are provided, then the
application will execute the `chrt` command (see `man chrt(1)`) for each vCPU thread ID, obtained via the QMP
protocol as described above, passing it the preferred scheduler and the priority parameters.

The application then sits calmly, waiting for the child qemu process to finish and unwinds the changes done to the
cpu sets.

## Contributing
Just open an issue or a pull request, describe what problem you are facing or feature you would like to see and we
will work together to see what can be done.
