mod gpu;

use std::convert::Infallible;
use std::process;
use std::sync::atomic::{self, AtomicBool};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use std::u64;
use std::vec::Vec;

use futures::channel::oneshot;
use futures::future::{self, Future};
use futures::TryFutureExt;

use hyper::{Body, Request, Response, Server, StatusCode};

use serde_json::{json, Value};

use rand::{Rng, SeedableRng};

use rand_xorshift::XorShiftRng;

use blake2::Blake2bVar;

use digest::{Update, VariableOutput};

use parking_lot::{Condvar, Mutex};

use chrono::{DateTime, Utc};

use gpu::Gpu;

fn work_value(root: [u8; 32], work: [u8; 8]) -> [u8; 32] {
    let mut buf = [0u8; 32];
    let mut hasher = Blake2bVar::new(buf.len()).expect("Unsupported hash length");
    hasher.update(&work);
    hasher.update(&root);
    hasher.finalize_variable(&mut buf).unwrap();
    buf
}

#[inline]
fn work_valid(root: [u8; 32], work: [u8; 8], threshold: [u8; 32]) -> (bool, [u8; 32]) {
    let result_threshold = work_value(root, work);
    (quick_greater_or_equal(result_threshold, threshold), result_threshold)
}

fn quick_greater_or_equal(x: [u8; 32], y: [u8; 32]) -> bool {
    for i in 0..32 {
        if x[i] > y[i] {
            return true;
        }
        if x[i] < y[i] {
            return false;
        }
    }
    true
}

enum WorkError {
    Canceled,
    Errored,
}

#[derive(Default)]
struct WorkState {
    root: [u8; 32],
    threshold: [u8; 32],
    callback: Option<oneshot::Sender<Result<[u8; 8], WorkError>>>,
    task_complete: Arc<AtomicBool>,
    unsuccessful_workers: usize,
    random_mode: bool,
    future_work: Vec<([u8; 32], [u8; 32], oneshot::Sender<Result<[u8; 8], WorkError>>)>,
}

impl WorkState {
    fn set_task(&mut self, cond_var: &Condvar) {
        if self.callback.is_none() {
            self.task_complete.store(true, atomic::Ordering::Relaxed);
            if self.future_work.len() > 0 {
                let max_range = if self.random_mode {
                    self.future_work.len()
                } else {
                    1
                };
                let i = rand::thread_rng().gen_range(0..max_range);
                let (root, threshold, callback) = self.future_work.remove(i);
                self.root = root;
                self.threshold = threshold;
                self.callback = Some(callback);
                self.task_complete = Arc::new(AtomicBool::new(false));
                cond_var.notify_all();
            }
        }
    }
}

#[derive(Clone)]
struct RpcService {
    work_state: Arc<(Mutex<WorkState>, Condvar)>,
}

enum RpcCommand {
    WorkGenerate([u8; 32], [u8; 32]),
    WorkCancel([u8; 32]),
    WorkValidate([u8; 32], [u8; 8], [u8; 32]),
    Benchmark([u8; 32], u64),
    Status(),
}

enum HexJsonError {
    Empty,
    InvalidHex,
    TooLong,
    TooShort,
}

impl RpcService {
    fn generate_work(
        &self,
        root: [u8; 32],
        threshold: [u8; 32],
    ) -> impl Future<Output = Result<[u8; 8], WorkError>> {
        let mut state = self.work_state.0.lock();
        let (callback_send, callback_recv) = oneshot::channel();
        state.future_work.push((root, threshold, callback_send));
        state.set_task(&self.work_state.1);
        callback_recv
            .map_err(|_| WorkError::Errored)
            .and_then(|x| future::ready(x))
    }

    fn cancel_work(&self, root: [u8; 32]) {
        let mut state = self.work_state.0.lock();
        let mut i = 0;
        while i < state.future_work.len() {
            if state.future_work[i].0 == root {
                let (_, _, callback) = state.future_work.remove(i);
                let _ = callback.send(Err(WorkError::Canceled));
                continue;
            }
            i += 1;
        }
        if state.root == root {
            if let Some(callback) = state.callback.take() {
                let _ = callback.send(Err(WorkError::Canceled));
                state.set_task(&self.work_state.1);
            }
        }
    }

    fn parse_hex_json(
        value: &Value,
        out: &mut [u8],
        allow_short: bool,
    ) -> Result<(), HexJsonError> {
        let bytes = value
            .as_str()
            .and_then(|s| hex::decode(s).ok())
            .ok_or(HexJsonError::InvalidHex)?;
        if bytes.len() == 0 {
            return Err(HexJsonError::Empty);
        } else if !allow_short && bytes.len() < out.len() {
            return Err(HexJsonError::TooShort);
        } else if bytes.len() > out.len() {
            return Err(HexJsonError::TooLong);
        }
        for (byte, out) in bytes.iter().rev().zip(out.iter_mut().rev()) {
            *out = *byte;
        }
        Ok(())
    }

    fn parse_hash_json(json: &Value) -> Result<[u8; 32], Value> {
        let root = json.get("hash").ok_or(json!({
            "error": "Failed to deserialize JSON",
            "hint": "Hash field missing",
        }))?;
        let mut out = [0u8; 32];
        Self::parse_hex_json(&root, &mut out, false).map_err(|err| match err {
            HexJsonError::Empty => json!({
                "error": "Bad block hash",
                "hint": "Hash is empty. Expecting a hex string",
            }),
            HexJsonError::InvalidHex => json!({
                "error": "Bad block hash",
                "hint": "Expecting a hex string",
            }),
            HexJsonError::TooShort => json!({
                "error": "Bad block hash",
                "hint": "Hash is too short (should be 32 bytes)",
            }),
            HexJsonError::TooLong => json!({
                "error": "Bad block hash",
                "hint": "Hash is too long (should be 32 bytes)",
            }),
        })?;
        Ok(out)
    }

    fn parse_work_json(json: &Value) -> Result<[u8; 8], Value> {
        let root = json.get("work").ok_or(json!({
            "error": "Failed to deserialize JSON",
            "hint": "Work field missing",
        }))?;
        let mut out = [0u8; 8];
        Self::parse_hex_json(&root, &mut out, true).map_err(|err| match err {
            HexJsonError::Empty => json!({
                "error": "Failed to deserialize JSON",
                "hint": "Work is empty. Expecting a hex string",
            }),
            HexJsonError::InvalidHex => json!({
                "error": "Failed to deserialize JSON",
                "hint": "Expecting a hex string for work",
            }),
            HexJsonError::TooShort => panic!("Unexpected error HexJsonError::TooShort"),
            HexJsonError::TooLong => json!({
                "error": "Failed to deserialize JSON",
                "hint": "Work is too long (should be 8 bytes)",
            }),
        })?;
        out.reverse();
        Ok(out)
    }

    fn parse_threshold_json(json: &Value) -> Result<[u8; 32], Value> {
        let threshold = json.get("threshold").ok_or(json!({
            "error": "Failed to deserialize JSON",
            "hint": "Threshold field missing",
        }))?;
        let mut out = [0u8; 32];
        Self::parse_hex_json(&threshold, &mut out, false).map_err(|err| match err {
            HexJsonError::Empty => json!({
                "error": "Bad threshold",
                "hint": "Threshold is empty. Expecting a hex string",
            }),
            HexJsonError::InvalidHex => json!({
                "error": "Bad threshold",
                "hint": "Expecting a hex string",
            }),
            HexJsonError::TooShort => json!({
                "error": "Bad threshold",
                "hint": "Threshold is too short (should be 32 bytes)",
            }),
            HexJsonError::TooLong => json!({
                "error": "Bad threshold",
                "hint": "Threshold is too long (should be 32 bytes)",
            }),
        })?;
        Ok(out)
    }

    fn parse_count_json(json: &Value) -> Result<u64, Value> {
        match json.get("count") {
            None => Err(json!({
                "error": "Failed to deserialize JSON",
                "hint": "count field missing"
            })),

            Some(json) => {
                let count = json
                    .as_u64()
                    .filter(|&x| x > 0)
                    .or(json
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .filter(|&x| x > 0))
                    .ok_or(json!({
                        "error": "Failed to deserialize JSON",
                        "hint": "Expecting a positive number for count"
                    }))?;
                Ok(count)
            }
        }
    }

    fn parse_json(&self, json: Value) -> Result<RpcCommand, Value> {
        match json.get("action") {
            None => {
                return Err(json!({
                    "error": "Failed to deserialize JSON",
                    "hint": "Work field missing",
                }))
            }
            Some(action) if action == "work_generate" => Ok(RpcCommand::WorkGenerate(
                Self::parse_hash_json(&json)?,
                Self::parse_threshold_json(&json)?
            )),
            Some(action) if action == "work_cancel" => {
                Ok(RpcCommand::WorkCancel(Self::parse_hash_json(&json)?))
            }
            Some(action) if action == "work_validate" => Ok(RpcCommand::WorkValidate(
                Self::parse_hash_json(&json)?,
                Self::parse_work_json(&json)?,
                Self::parse_threshold_json(&json)?
            )),
            Some(action) if action == "benchmark" => Ok(RpcCommand::Benchmark(
                Self::parse_threshold_json(&json)?,
                Self::parse_count_json(&json)?,
            )),
            Some(action) if action == "status" => Ok(RpcCommand::Status()),
            Some(_) => {
                return Err(json!({
                    "error": "Unknown command",
                    "hint": "Supported commands: work_generate, work_cancel, work_validate, benchmark, status"
                }))
            }
        }
    }

    async fn process_req(self, body: &[u8]) -> hyper::Result<(StatusCode, Value)> {
        let json = match serde_json::from_slice(body) {
            Ok(json) => json,
            Err(_) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": "Failed to deserialize JSON",
                    }),
                ));
            }
        };
        let command = match self.parse_json(json) {
            Ok(r) => r,
            Err(err) => return Ok((StatusCode::BAD_REQUEST, err)),
        };
        let start = Instant::now();
        match command {
            RpcCommand::WorkGenerate(root, threshold) => {
                let now: DateTime<Utc> = Utc::now();
                let _ = println!(
                    "{} Received work for {}",
                    now.format("%T"),
                    hex::encode_upper(&root)
                );
                match self.generate_work(root, threshold).await {
                    Ok(mut work) => {
                        let result_threshold = work_value(root, work);
                        let now: DateTime<Utc> = Utc::now();
                        let _ = println!(
                            "{} Generated for {} in {}ms for threshold {}",
                            now.format("%T"),
                            hex::encode_upper(&root),
                            start.elapsed().as_millis(),
                            hex::encode(&result_threshold)
                        );
                        // Reverse before encoding
                        work.reverse();
                        Ok((
                            StatusCode::OK,
                            json!({
                                "work": hex::encode(&work),
                                "threshold": hex::encode(result_threshold)
                            }),
                        ))
                    }
                    Err(WorkError::Canceled) => Ok((
                        StatusCode::OK,
                        json!({
                            "error": "Cancelled",
                        }),
                    )),
                    Err(WorkError::Errored) => Ok((
                        StatusCode::OK,
                        json!({
                            "error": "Work generation failed (see logs for details)",
                        }),
                    )),
                }
            }
            RpcCommand::WorkCancel(root) => {
                let _ = println!("Cancel {}", hex::encode_upper(&root));
                self.cancel_work(root);
                Ok((StatusCode::OK, json!({})))
            }
            RpcCommand::WorkValidate(root, work, threshold) => {
                let _ = println!("Validate {}", hex::encode_upper(&root));
                let (valid, result_threshold) = work_valid(root, work, threshold);
                let result = json!({
                    "valid": valid,
                    "threshold": hex::encode(result_threshold)
                });
                Ok((StatusCode::OK, result))
            }
            RpcCommand::Benchmark(threshold, count) => {
                let _ = println!(
                    "Benchmarking {} samples at threshold {}",
                    count, hex::encode(threshold),
                );
                let mut roots: Vec<[u8; 32]> = Vec::new();
                roots.reserve(count as usize);
                for _ in 0..count {
                    roots.push(rand::random())
                }
                let start = Instant::now();
                for root in roots {
                    if self.generate_work(root, threshold).await.is_err() {
                        return Ok((StatusCode::INTERNAL_SERVER_ERROR, {
                            json!({
                                "error": "Benchmark failed",
                                "hint": "Work generation failure",
                            })
                        }));
                    }
                }
                let duration = start.elapsed().as_millis();
                let average = duration as u64 / count;
                println!(
                    "Benchmark finished in {}ms , average {}ms / sample",
                    duration, average
                );
                Ok((StatusCode::OK, {
                    json!({
                        "threshold": hex::encode(threshold),
                        "count": format!("{}", count),
                        "duration": format!("{}", duration),
                        "average": format!("{}", average),
                        "hint": "Times in milliseconds",
                    })
                }))
            }
            RpcCommand::Status() => {
                let state = self.work_state.0.lock();
                let queue_size = state.future_work.len();
                let resp = json!({
                    "queue_size": format!("{}", queue_size),
                    "generating": if state.task_complete.load(atomic::Ordering::Relaxed) {"0"} else {"1"},
                });
                println!("Status {}", resp);
                Ok((StatusCode::OK, resp))
            }
        }
    }

    async fn handle_request(self, mut req: Request<Body>) -> hyper::Result<Response<Body>> {
        let (status, body) = if *req.method() == hyper::Method::POST {
            let self_copy = self.clone();
            let body = hyper::body::to_bytes(req.body_mut()).await?;
            self_copy.process_req(body.as_ref()).await?
        } else {
            (
                StatusCode::METHOD_NOT_ALLOWED,
                json!({
                    "error": "Can only POST requests",
                }),
            )
        };
        let body_str = body.to_string();
        let body_len = body_str.len();
        let body = Body::from(body_str);
        Ok(Response::builder()
            .header(hyper::header::CONTENT_LENGTH, body_len)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .status(status)
            .body(body)
            .expect("Failed to build response"))
    }
}

#[tokio::main]
async fn main() {
    let args = clap::App::new("Nano work server")
        .version("1.0")
        .author("Lee Bousfield <ljbousfield@gmail.com>")
        .about("Provides a work server for Nano without a full node.")
        .arg(
            clap::Arg::with_name("listen_address")
                .short("l")
                .long("listen-address")
                .value_name("ADDR")
                .default_value("[::1]:7076")
                .help("Specifies the address to listen on."),
        )
        .arg(
            clap::Arg::with_name("cpu_threads")
                .short("c")
                .long("cpu-threads")
                .value_name("THREADS")
                .default_value("0")
                .help("Specifies how many CPU threads to use."),
        )
        .arg(
            clap::Arg::with_name("gpu")
                .short("g")
                .long("gpu")
                .value_name("PLATFORM:DEVICE:THREADS")
                .multiple(true)
                .help("Specifies which GPU(s) to use. THREADS is optional and defaults to 1048576."),
        )
        .arg(
            clap::Arg::with_name("gpu_local_work_size")
                .long("gpu-local-work-size")
                .value_name("N")
                .help("The GPU local work size. Increasing it may increase performance. For advanced users only."),
        )
        .arg(
            clap::Arg::with_name("shuffle")
                .long("shuffle")
                .help("Pick a random request from the queue instead of the oldest. Increases efficiency when using multiple work servers")
        )
        .get_matches();
    let random_mode = args.is_present("shuffle");
    let listen_addr = args
        .value_of("listen_address")
        .unwrap()
        .parse()
        .expect("Failed to parse listen address");
    let cpu_threads: usize = args
        .value_of("cpu_threads")
        .unwrap()
        .parse()
        .expect("Failed to parse CPU threads");
    let gpu_local_work_size = args.value_of("gpu_local_work_size").map(|s| {
        s.parse()
            .expect("Failed to parse GPU local work size option")
    });
    let gpus: Vec<Gpu> = args
        .values_of("gpu")
        .map(|x| x.collect())
        .unwrap_or_else(Vec::new)
        .into_iter()
        .map(|s| {
            let mut parts = s.split(':');
            let platform = parts
                .next()
                .expect("GPU string cannot be blank")
                .parse()
                .expect(&format!("Failed to parse GPU platform in string {:?}", s));
            let device = parts
                .next()
                .expect(&format!("GPU string {:?} must have at least one colon", s))
                .parse()
                .expect(&format!("Failed to parse GPU device in string {:?}", s));
            let threads = parts
                .next()
                .unwrap_or("1048576")
                .parse()
                .expect(&format!("Failed to parse GPU threads in string {:?}", s));
            if parts.next().is_some() {
                panic!("Too many colons in GPU string {:?}", s);
            }
            Gpu::new(platform, device, threads, gpu_local_work_size)
                .expect(&format!("Failed to create GPU from string {:?}", s))
        })
        .collect();

    let n_workers = gpus.len() + cpu_threads;
    if n_workers == 0 {
        eprintln!("No workers specified. Please use the --gpu or --cpu-threads flags.\nUse --help for more options.");
        process::exit(1);
    }
    let work_state = Arc::new((Mutex::new(WorkState::default()), Condvar::new()));
    {
        let mut state = work_state.0.lock();
        state.task_complete.store(true, atomic::Ordering::Relaxed);
        state.random_mode = random_mode;
    }
    let mut worker_handles = Vec::new();
    for _ in 0..cpu_threads {
        let work_state = work_state.clone();
        let mut rng =
            XorShiftRng::from_rng(rand::thread_rng()).expect("Failed to create XorShiftRng");
        let mut root = [0u8; 32];
        let mut threshold = [0u8; 32];
        let mut task_complete = Arc::new(AtomicBool::new(true));
        let handle = thread::spawn(move || loop {
            if task_complete.load(atomic::Ordering::Relaxed) {
                let mut state = work_state.0.lock();
                while state.callback.is_none() {
                    work_state.1.wait(&mut state);
                }
                root = state.root;
                threshold = state.threshold;
                task_complete = state.task_complete.clone();
            }
            let mut out: [u8; 8] = rng.gen();
            for _ in 0..(1 << 18) {
                if work_valid(root, out, threshold).0 {
                    let mut state = work_state.0.lock();
                    if root == state.root {
                        if let Some(callback) = state.callback.take() {
                            let _ = callback.send(Ok(out));
                            state.set_task(&work_state.1);
                        }
                    }
                    break;
                }
                for byte in out.iter_mut() {
                    *byte = byte.wrapping_add(1);
                    if *byte != 0 {
                        // We did not overflow
                        break;
                    }
                }
            }
        });
        worker_handles.push(handle.thread().clone());
    }
    for (gpu_i, mut gpu) in gpus.into_iter().enumerate() {
        let mut failed = false;
        let mut rng =
            XorShiftRng::from_rng(rand::thread_rng()).expect("Failed to create XorShiftRng");
        let mut root = [0u8; 32];
        let mut threshold = [0u8; 32];
        let work_state = work_state.clone();
        let mut task_complete = Arc::new(AtomicBool::new(true));
        let mut consecutive_gpu_errors = 0;
        let mut consecutive_gpu_invalid_work_errors = 0;
        let handle = thread::spawn(move || loop {
            if failed || task_complete.load(atomic::Ordering::Relaxed) {
                let mut state = work_state.0.lock();
                if root != state.root {
                    failed = false;
                }
                if failed {
                    state.unsuccessful_workers += 1;
                    if state.unsuccessful_workers == n_workers {
                        if let Some(callback) = state.callback.take() {
                            let _ = callback.send(Err(WorkError::Errored));
                            state.set_task(&work_state.1);
                        }
                    }
                    work_state.1.wait(&mut state);
                }
                while state.callback.is_none() {
                    work_state.1.wait(&mut state);
                }
                root = state.root;
                threshold = state.threshold;
                task_complete = state.task_complete.clone();
                if failed {
                    state.unsuccessful_workers -= 1;
                }
                if let Err(err) = gpu.set_task(&root, &threshold) {
                    eprintln!(
                        "Failed to set GPU {}'s task, abandoning it for this work: {:?}",
                        gpu_i, err,
                    );
                    failed = true;
                    continue;
                }
                failed = false;
                consecutive_gpu_errors = 0;
            }
            let attempt = rng.gen();
            let mut out = [0u8; 8];
            match gpu.run(&mut out, attempt) {
                Ok(true) => {
                    if work_valid(root, out, threshold).0 {
                        let mut state = work_state.0.lock();
                        if root == state.root {
                            if let Some(callback) = state.callback.take() {
                                let _ = callback.send(Ok(out));
                                state.set_task(&work_state.1);
                            }
                        }
                        consecutive_gpu_errors = 0;
                        consecutive_gpu_invalid_work_errors = 0;
                    } else {
                        eprintln!(
                            "GPU {} returned invalid work {} for root {}",
                            gpu_i,
                            hex::encode(&out),
                            hex::encode_upper(&root),
                        );
                        if consecutive_gpu_invalid_work_errors >= 3 {
                            eprintln!("GPU {} returned invalid work 3 consecutive times, abandoning it for this work", gpu_i);
                            failed = true;
                        } else {
                            consecutive_gpu_errors += 1;
                            consecutive_gpu_invalid_work_errors += 1;
                        }
                    }
                }
                Ok(false) => {
                    consecutive_gpu_errors = 0;
                }
                Err(err) => {
                    eprintln!("Error computing work on GPU {}: {:?}", gpu_i, err);
                    if let Err(err) = gpu.reset_bufs() {
                        eprintln!(
                            "Failed to reset GPU {}'s buffers, abandoning it for this work: {:?}",
                            gpu_i, err,
                        );
                        failed = true;
                    }
                    consecutive_gpu_errors += 1;
                }
            }
            if consecutive_gpu_errors >= 3 {
                eprintln!(
                    "3 consecutive GPU {} errors, abandoning it for this work",
                    gpu_i,
                );
                failed = true;
            }
        });
        worker_handles.push(handle.thread().clone());
    }

    let service = RpcService {
        work_state: work_state.clone(),
    };
    let make_service = hyper::service::make_service_fn(|_| {
        let service = service.clone();
        async move {
            Ok::<_, Infallible>(hyper::service::service_fn(move |req| {
                service.clone().handle_request(req)
            }))
        }
    });
    let server = Server::bind(&listen_addr).serve(make_service);
    println!("Ready to receive requests on {}", listen_addr);
    server.await.expect("Failed to serve requests");
}
