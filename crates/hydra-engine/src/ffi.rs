//! C ABI and JNI bridge.
//!
//! Ownership: engines live in a process-wide registry keyed by opaque `u64` handles, never raw
//! pointers. Every entry point clones the engine's `Arc` for as long as it uses it, so
//! `hydra_engine_destroy` only releases the owner reference: an event wait or request still running
//! keeps the engine alive until it returns, and any call with a destroyed handle fails cleanly.
//!
//! Threading: `hydra_engine_call_async` never blocks. It registers the request (so a later cancel
//! always reaches it), spawns it on the shared Tokio runtime and returns. The completion runs exactly
//! once, on a Tokio worker thread, or synchronously on the caller thread for requests rejected before
//! they start. Completions must not call blocking engine functions (`next_event`, `shutdown`,
//! `destroy`); `cancel` is safe anywhere.
//!
//! Panics: a panic inside a spawned request is caught by Tokio and its completion reports
//! `internal`. A panic in the synchronous part of an `extern "C"` function aborts the process
//! (Rust's behavior for panics crossing a non-unwinding ABI); those paths do no fallible work.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Engine, EngineError, runtime};

#[derive(Deserialize)]
struct Call {
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
}

fn registry() -> &'static Mutex<HashMap<u64, Arc<Engine>>> {
    static ENGINES: OnceLock<Mutex<HashMap<u64, Arc<Engine>>>> = OnceLock::new();
    ENGINES.get_or_init(Default::default)
}

fn engine(handle: u64) -> Option<Arc<Engine>> {
    registry().lock().ok()?.get(&handle).cloned()
}

fn string(value: &Value) -> *mut c_char {
    CString::new(value.to_string()).map_or(std::ptr::null_mut(), CString::into_raw)
}

fn error_response(id: &str, error: EngineError) -> Value {
    json!({"id":id,"error":error})
}

fn invalid_call() -> Value {
    error_response("", EngineError::new("invalid_request", "Invalid native JSON request", "pass {id, method, params}"))
}

type Done = Box<dyn FnOnce(Value) + Send>;

/// Delivers exactly one response. If the request task ends without one (a panic), Drop reports it.
struct Completion {
    id: String,
    done: Option<Done>,
}

impl Completion {
    fn finish(mut self, response: Value) {
        if let Some(done) = self.done.take() {
            done(response);
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done(error_response(
                &self.id,
                EngineError::new("internal", "The native request ended without a result", "reload the native module"),
            ));
        }
    }
}

/// Shared by the C ABI and JNI: validate, register synchronously, run asynchronously.
fn submit(handle: u64, request: &[u8], done: Done) {
    let call = match serde_json::from_slice::<Call>(request) {
        Ok(call) if !call.id.is_empty() && call.id.len() <= 128 => call,
        _ => return done(invalid_call()),
    };
    let Some(engine) = engine(handle) else { return done(error_response(&call.id, EngineError::shutdown())) };
    let token = match engine.begin(&call.id) {
        Ok(token) => token,
        Err(error) => return done(error_response(&call.id, error)),
    };
    let completion = Completion { id: call.id.clone(), done: Some(done) };
    runtime().spawn(async move {
        let id = call.id;
        let response = match engine.run(id.clone(), token, call.method, call.params).await {
            Ok(result) => json!({"id":id,"result":result}),
            Err(error) => error_response(&id, error),
        };
        drop(engine);
        completion.finish(response);
    });
}

fn create() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let handle = NEXT.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut engines) = registry().lock() {
        engines.insert(handle, Engine::new());
        handle
    } else {
        0
    }
}

fn next_event(handle: u64) -> Option<Value> {
    let engine = engine(handle)?;
    runtime().block_on(engine.next_event())
}

fn shutdown(handle: u64) {
    if let Some(engine) = engine(handle) {
        runtime().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), engine.shutdown()).await.ok();
        });
    }
}

fn destroy(handle: u64) {
    shutdown(handle);
    let owner = registry().lock().ok().and_then(|mut engines| engines.remove(&handle));
    drop(owner);
}

/// Completion callback: `response` is a JSON `{id, result}` or `{id, error}` string that is only
/// valid for the duration of the call; copy it before returning.
pub type HydraEngineCompletion = extern "C" fn(context: *mut c_void, response: *const c_char);

struct Context(*mut c_void);
// SAFETY: the context is an opaque token owned by the caller, handed back once on any thread;
// the C contract requires it to be usable from any thread.
unsafe impl Send for Context {}

/// Creates an engine and returns its handle (never 0 on success). Release it with hydra_engine_destroy.
#[unsafe(no_mangle)]
pub extern "C" fn hydra_engine_create() -> u64 {
    create()
}

/// Submits one request without blocking. `completion(context, response)` runs exactly once.
#[unsafe(no_mangle)]
pub extern "C" fn hydra_engine_call_async(
    engine: u64,
    request: *const c_char,
    completion: HydraEngineCompletion,
    context: *mut c_void,
) {
    let context = Context(context);
    let done: Done = Box::new(move |response| {
        let context = context;
        let response = CString::new(response.to_string()).unwrap_or_default();
        completion(context.0, response.as_ptr());
    });
    if request.is_null() {
        return done(invalid_call());
    }
    // SAFETY: the caller passes a valid NUL-terminated string that outlives this call.
    let request = unsafe { CStr::from_ptr(request) }.to_bytes();
    submit(engine, request, done);
}

/// Cancels a request. Returns true if it was in flight; an id not yet submitted is remembered
/// (bounded) and that request fails with `cancelled` when it arrives.
#[unsafe(no_mangle)]
pub extern "C" fn hydra_engine_cancel(engine: u64, id: *const c_char) -> bool {
    if id.is_null() {
        return false;
    }
    // SAFETY: the caller passes a valid NUL-terminated string that outlives this call.
    let Ok(id) = unsafe { CStr::from_ptr(id) }.to_str() else { return false };
    self::engine(engine).is_some_and(|engine| engine.cancel(id))
}

/// Blocks until the next event, in order. Returns null once the engine is shut down or destroyed.
/// Call it from exactly one dedicated thread per engine; free the result with hydra_engine_string_free.
#[unsafe(no_mangle)]
pub extern "C" fn hydra_engine_next_event(engine: u64) -> *mut c_char {
    next_event(engine).map_or(std::ptr::null_mut(), |event| string(&event))
}

/// Cancels every request (each completes with `cancelled`, `shutdown` or `classic_outcome_unknown`),
/// ends event waits, closes byte channels and disconnects sessions. Blocks for at most ~5 s.
#[unsafe(no_mangle)]
pub extern "C" fn hydra_engine_shutdown(engine: u64) {
    shutdown(engine);
}

/// Shuts down and releases the owner reference. Later calls with this handle fail with `shutdown`.
#[unsafe(no_mangle)]
pub extern "C" fn hydra_engine_destroy(engine: u64) {
    destroy(engine);
}

#[unsafe(no_mangle)]
pub extern "C" fn hydra_engine_string_free(value: *mut c_char) {
    if !value.is_null() {
        // SAFETY: only strings returned by hydra_engine_next_event are passed here, exactly once.
        drop(unsafe { CString::from_raw(value) });
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use jni::JNIEnv;
    use jni::objects::{JClass, JObject, JString, JValue};
    use jni::sys::{jboolean, jlong, jstring};

    fn jni_string(env: &mut JNIEnv, value: Option<Value>) -> jstring {
        value
            .and_then(|value| env.new_string(value.to_string()).ok())
            .map_or(std::ptr::null_mut(), |output| output.into_raw())
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_expo_modules_hydraengine_NativeHydraEngine_create(
        _env: JNIEnv,
        _class: JClass,
    ) -> jlong {
        create() as jlong
    }

    /// `completion` implements `EngineCompletion.complete(String)`; it is invoked exactly once.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_expo_modules_hydraengine_NativeHydraEngine_callAsync(
        mut env: JNIEnv,
        _class: JClass,
        engine: jlong,
        request: JString,
        completion: JObject,
    ) {
        let (Ok(vm), Ok(completion)) = (env.get_java_vm(), env.new_global_ref(completion)) else { return };
        let done: Done = Box::new(move |response| {
            let Ok(mut env) = vm.attach_current_thread_permanently() else { return };
            if let Ok(response) = env.new_string(response.to_string()) {
                let _ = env.call_method(&completion, "complete", "(Ljava/lang/String;)V", &[JValue::Object(&response)]);
            }
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
        });
        let request: Option<String> = env.get_string(&request).ok().map(Into::into);
        match request {
            Some(request) => submit(engine as u64, request.as_bytes(), done),
            None => done(invalid_call()),
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_expo_modules_hydraengine_NativeHydraEngine_cancel(
        mut env: JNIEnv,
        _class: JClass,
        engine: jlong,
        id: JString,
    ) -> jboolean {
        let Ok(id) = env.get_string(&id).map(String::from) else { return 0 };
        super::engine(engine as u64).is_some_and(|engine| engine.cancel(&id)) as jboolean
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_expo_modules_hydraengine_NativeHydraEngine_nextEvent(
        mut env: JNIEnv,
        _class: JClass,
        engine: jlong,
    ) -> jstring {
        let event = next_event(engine as u64);
        jni_string(&mut env, event)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_expo_modules_hydraengine_NativeHydraEngine_shutdown(
        _env: JNIEnv,
        _class: JClass,
        engine: jlong,
    ) {
        shutdown(engine as u64);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_expo_modules_hydraengine_NativeHydraEngine_destroy(
        _env: JNIEnv,
        _class: JClass,
        engine: jlong,
    ) {
        destroy(engine as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    fn call(handle: u64, request: Value) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel();
        submit(
            handle,
            request.to_string().as_bytes(),
            Box::new(move |response| {
                tx.send(response).ok();
            }),
        );
        rx
    }

    /// Loopback listener that accepts and never speaks SSH: a connect to it hangs until the 10 s timeout.
    fn stalled_endpoint() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn connect_request(id: &str, port: u16) -> Value {
        json!({"id":id,"method":"connect","params":{"host":"127.0.0.1","port":port,"username":"u","session":"compat","privateKey":"unused","expectedFingerprint":"SHA256:x"}})
    }

    #[test]
    fn hung_call_does_not_block_other_calls_and_cancel_reaches_it() {
        let (_listener, port) = stalled_endpoint();
        let handle = create();
        let started = Instant::now();
        let hung = call(handle, connect_request("hung", port));
        // Submitting returned immediately; an independent request completes while the first hangs.
        let other = call(handle, json!({"id":"other","method":"list","params":{"sessionId":"ssh-9"}}));
        let other = other.recv_timeout(Duration::from_secs(2)).expect("independent request not blocked");
        assert_eq!(other["error"]["kind"], "session_not_found");
        assert!(hung.try_recv().is_err(), "the stalled connect is still in flight");
        // A second engine (separate JS runtime) is just as independent.
        let second = create();
        let reply = call(second, json!({"id":"x","method":"nope"})).recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(reply["error"]["kind"], "unknown_method");
        assert!(engine(handle).unwrap().cancel("hung"));
        let hung = hung.recv_timeout(Duration::from_secs(2)).expect("cancel reaches the in-flight request");
        assert_eq!(hung["error"]["kind"], "cancelled");
        assert!(started.elapsed() < Duration::from_secs(5));
        destroy(handle);
        destroy(second);
    }

    #[test]
    fn cancel_before_submit_cancels_the_queued_request() {
        let handle = create();
        let id = CString::new("queued").unwrap();
        assert!(!hydra_engine_cancel(handle, id.as_ptr()), "recorded, not yet in flight");
        let reply = call(handle, json!({"id":"queued","method":"list","params":{"sessionId":"s"}}))
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(reply["error"]["kind"], "cancelled");
        destroy(handle);
    }

    #[test]
    fn shutdown_during_next_event_and_in_flight_call_is_safe() {
        let (_listener, port) = stalled_endpoint();
        for _ in 0..20 {
            let handle = create();
            let pump = std::thread::spawn(move || {
                let mut events = 0;
                loop {
                    let event = hydra_engine_next_event(handle);
                    if event.is_null() {
                        break events;
                    }
                    hydra_engine_string_free(event);
                    events += 1;
                }
            });
            let in_flight = call(handle, connect_request("c", port));
            std::thread::sleep(Duration::from_millis(20));
            // Cancel racing teardown, then destroy while the pump is blocked inside next_event.
            let canceller = std::thread::spawn(move || {
                let id = CString::new("c").unwrap();
                hydra_engine_cancel(handle, id.as_ptr());
            });
            hydra_engine_destroy(handle);
            canceller.join().unwrap();
            pump.join().expect("event thread returns after destroy");
            let reply = in_flight.recv_timeout(Duration::from_secs(2)).expect("in-flight call completes");
            assert!(matches!(reply["error"]["kind"].as_str(), Some("cancelled" | "shutdown")), "{reply}");
            // Every entry point is a clean failure after destroy.
            assert!(hydra_engine_next_event(handle).is_null());
            let id = CString::new("late").unwrap();
            assert!(!hydra_engine_cancel(handle, id.as_ptr()));
            let late = call(handle, json!({"id":"late","method":"list"})).recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(late["error"]["kind"], "shutdown");
            hydra_engine_shutdown(handle);
            hydra_engine_destroy(handle);
        }
    }

    #[test]
    fn c_abi_completion_runs_exactly_once() {
        extern "C" fn record(context: *mut c_void, response: *const c_char) {
            let sender = unsafe { Box::from_raw(context as *mut mpsc::Sender<String>) };
            let response = unsafe { CStr::from_ptr(response) }.to_string_lossy().into_owned();
            sender.send(response).ok();
        }
        let handle = hydra_engine_create();
        for request in [r#"{"id":"a","method":"nope"}"#, "not json"] {
            let (tx, rx) = mpsc::channel::<String>();
            let request = CString::new(request).unwrap();
            hydra_engine_call_async(handle, request.as_ptr(), record, Box::into_raw(Box::new(tx)) as *mut c_void);
            let response: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
            assert!(response["error"]["kind"].is_string());
        }
        hydra_engine_destroy(handle);
    }
}
