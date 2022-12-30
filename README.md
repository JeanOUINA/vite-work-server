> This project was forked from [nano-work-server](https://github.com/nanocurrency/nano-work-server)

# Vite Work Server

![Build](https://github.com/jeanouina/vite-work-server/workflows/Build/badge.svg)

This project is a dedicated work server for [the Vite cryptocurrency](https://vite.org/).

**vite-work-server** supports the `work_generate`, `work_cancel`, and `work_validate` commands.

To see available command line options, run `nano-work-server --help`.

If using more than one work peer, give the flag `--shuffle`. This makes it so that the next request is picked randomly instead of sequentially, which leads to more efficient work generation with multiple peers, especially when they are not in the same network.

## Installation

### OpenCL 

Ubuntu:

```
sudo apt install ocl-icd-opencl-dev
```

Fedora:

```
sudo dnf install ocl-icd-devel
```

Windows:
- AMD GPU: [OCL-SDK](https://github.com/GPUOpen-LibrariesAndSDKs/OCL-SDK/releases/)
- Nvidia GPU: [CUDA Toolkit](https://developer.nvidia.com/cuda-toolkit)

### Rust

Linux:

```
curl https://sh.rustup.rs -sSf | sh
```

Windows: follow instructions in https://www.rust-lang.org/tools/install

### GCC

Ubuntu:

```
sudo apt install gcc
```

Fedora:

```
sudo dnf install gcc
```

### Build

```bash
git clone https://github.com/jeanouina/vite-work-server.git
cd vite-work-server
cargo build --release
```

Depending on your system configuration and if the OpenCL library cannot be found in the `PATH`, it may be necessary to link against explicitly:

```bash
cargo rustc --release -- -l OpenCL -L "/path/to/opencl.lib"
```

## Using

`vite-work-server --help`

_Note_ threshold values may be outdated in these examples.

- `work_generate` example:

    ```json
    {
        "action": "work_generate",
        "hash": "718cc2121c3e641059bc1c2cfc45666c99e8ae922f7a807b7d07b62c995d79e2",
        "threshold": "ffffffc000000000000000000000000000000000000000000000000000000000"
    }
    ```
    Response:

    ```json
    {
        "threshold": "ffffffdef664a41ce3f73ab1882577719b77854188d0d5939cd6a63d7fc950bb",
        "work": "2bf29ef00786a6bc"
    }
    ```


- `work_validate` example:

    ```json
    {
        "action": "work_validate",
        "hash": "718cc2121c3e641059bc1c2cfc45666c99e8ae922f7a807b7d07b62c995d79e2",
        "threshold": "ffffffc000000000000000000000000000000000000000000000000000000000",
        "work": "2bf29ef00786a6bc"
    }
    ```
    Response:

    ```json
    {
        "valid": true,
        "threshold": "ffffffdef664a41ce3f73ab1882577719b77854188d0d5939cd6a63d7fc950bb"
    }
    ```

- `work_cancel` example:
    ```json
    {
        "action": "work_cancel",
        "hash": "718cc2121c3e641059bc1c2cfc45666c99e8ae922f7a807b7d07b62c995d79e2"
    }
    ```
    Response:

    ```json
    {}
    ```

## Benchmarking

Example request:

```json
{
    "action": "benchmark",
    "threshold": "ffffffc000000000000000000000000000000000000000000000000000000000",
    "count": "10"
}
```

_Note_ use a sufficiently high count as work generation is a random process.

Example response:

```json
{
    "average": "609",
    "count": "10",
    "duration": "6097",
    "hint": "Times in milliseconds",
    "threshold": "ffffffc000000000000000000000000000000000000000000000000000000000"
}
```

## Status

Example request:

```json
{
    "action": "status"
}
```

Example response:

```json
{
    "generating": "1",
    "queue_size": "3"
}
```

## Troubleshooting

- Linux OpenCL AMD GPU series error: `thread 'main' panicked at 'Failed to create GPU from string "00:00"` - see [solution here](https://github.com/nanocurrency/nano-work-server/issues/28)
