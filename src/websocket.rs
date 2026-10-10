use crate::config::Config;
use esp_idf_svc::{hal::delay::TickType, sys::*};
use espal_core::realtime::Assembler;
use std::{
    ffi::CString,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

enum Incoming {
    Connected,
    Fragment {
        opcode: u8,
        fin: bool,
        offset: usize,
        total: usize,
        bytes: Vec<u8>,
    },
    Failed(i32),
}
struct Context {
    sender: SyncSender<Incoming>,
    failed: AtomicBool,
    overflowed: AtomicBool,
}

fn connection_error(status: i32) -> &'static str {
    match status {
        401 => "OpenAI authentication failed (401); re-provision the API key",
        403 => "OpenAI access denied (403); check project/model access",
        400 => "OpenAI handshake rejected (400); check configured Realtime model",
        429 => "OpenAI rate or quota limit (429); no automatic retry",
        _ => "TLS/WebSocket connection failed or disconnected",
    }
}

// ESP-IDF owns callback execution. `Context` remains pinned until stop/destroy
// have joined the WebSocket task. Never parse JSON or block in this callback.
#[allow(non_upper_case_globals)]
unsafe extern "C" fn callback(
    arg: *mut core::ffi::c_void,
    _base: esp_event_base_t,
    id: i32,
    data: *mut core::ffi::c_void,
) {
    let ctx = &*(arg as *const Context);
    let event = match id as esp_websocket_event_id_t {
        esp_websocket_event_id_t_WEBSOCKET_EVENT_CONNECTED => Incoming::Connected,
        esp_websocket_event_id_t_WEBSOCKET_EVENT_DATA => {
            let d = &*(data as *const esp_websocket_event_data_t);
            if d.op_code >= 8 {
                return;
            }
            if d.data_len < 0
                || d.data_len > 4096
                || d.payload_offset < 0
                || d.payload_len < 0
                || (d.data_len > 0 && d.data_ptr.is_null())
            {
                ctx.failed.store(true, Ordering::Release);
                return;
            }
            let bytes = if d.data_len == 0 {
                Vec::new()
            } else {
                core::slice::from_raw_parts(d.data_ptr as *const u8, d.data_len as usize).to_vec()
            };
            Incoming::Fragment {
                opcode: d.op_code,
                fin: d.fin,
                offset: d.payload_offset as usize,
                total: d.payload_len as usize,
                bytes,
            }
        }
        esp_websocket_event_id_t_WEBSOCKET_EVENT_ERROR
        | esp_websocket_event_id_t_WEBSOCKET_EVENT_DISCONNECTED
        | esp_websocket_event_id_t_WEBSOCKET_EVENT_CLOSED => {
            let status = if data.is_null() {
                0
            } else {
                (*(data as *const esp_websocket_event_data_t))
                    .error_handle
                    .esp_ws_handshake_status_code
            };
            Incoming::Failed(status)
        }
        _ => return,
    };
    if ctx.sender.try_send(event).is_err() {
        ctx.overflowed.store(true, Ordering::Release);
        ctx.failed.store(true, Ordering::Release);
    }
}

pub struct Socket {
    handle: esp_websocket_client_handle_t,
    context: Box<Context>,
    receiver: Receiver<Incoming>,
    assembler: Assembler,
    // Retain referenced configuration strings until the C client is destroyed.
    _uri: CString,
    _headers: CString,
}
impl Socket {
    pub fn connect(c: &Config) -> Result<Self, &'static str> {
        let uri = CString::new(format!(
            "wss://api.openai.com/v1/realtime?model={}",
            c.model
        ))
        .map_err(|_| "Invalid URL")?;
        let headers = CString::new(format!("Authorization: Bearer {}\r\n", c.api_key))
            .map_err(|_| "Invalid authentication header")?;
        let (sender, receiver) = mpsc::sync_channel(256);
        let context = Box::new(Context {
            sender,
            failed: AtomicBool::new(false),
            overflowed: AtomicBool::new(false),
        });
        let config = esp_websocket_client_config_t {
            uri: uri.as_ptr(),
            headers: headers.as_ptr(),
            disable_auto_reconnect: true,
            buffer_size: 4096,
            task_stack: 8192,
            task_prio: 5,
            transport: esp_websocket_transport_t_WEBSOCKET_TRANSPORT_OVER_SSL,
            crt_bundle_attach: Some(esp_crt_bundle_attach),
            skip_cert_common_name_check: false,
            network_timeout_ms: 15000,
            ping_interval_sec: 15,
            pingpong_timeout_sec: 10,
            ..Default::default()
        };
        let handle = unsafe { esp_websocket_client_init(&config) };
        if handle.is_null() {
            return Err("WebSocket initialization failed");
        }
        let socket = Self {
            handle,
            context,
            receiver,
            assembler: Assembler::default(),
            _uri: uri,
            _headers: headers,
        };
        let result = unsafe {
            esp_websocket_register_events(
                handle,
                esp_websocket_event_id_t_WEBSOCKET_EVENT_ANY,
                Some(callback),
                &*socket.context as *const Context as *mut _,
            )
        };
        if result != ESP_OK {
            return Err("WebSocket callback registration failed");
        }
        if unsafe { esp_websocket_client_start(handle) } != ESP_OK {
            return Err("WebSocket start failed");
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if socket.context.failed.load(Ordering::Acquire) {
                return Err("WebSocket connection failed");
            }
            match socket.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Incoming::Connected) => break,
                Ok(Incoming::Failed(status)) => return Err(connection_error(status)),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err("WebSocket connection timed out");
            }
        }
        Ok(socket)
    }
    pub fn send(&mut self, event: &serde_json::Value) -> Result<(), &'static str> {
        let json = event.to_string();
        if json.len() > espal_core::realtime::MAX_MESSAGE {
            return Err("Outbound event too large");
        }
        let n = unsafe {
            esp_websocket_client_send_text(
                self.handle,
                json.as_ptr() as *const _,
                json.len() as i32,
                TickType::new_millis(5000).ticks(),
            )
        };
        if n != json.len() as i32 {
            return Err("WebSocket send failed");
        }
        Ok(())
    }
    pub fn receive(&mut self, deadline: Instant) -> Result<serde_json::Value, &'static str> {
        loop {
            if self.context.failed.load(Ordering::Acquire) {
                return Err(if self.context.overflowed.load(Ordering::Acquire) {
                    "WebSocket fragment queue overflow (256 fragments)"
                } else {
                    "Invalid WebSocket callback fragment"
                });
            }
            if Instant::now() >= deadline {
                return Err("Realtime session deadline exceeded");
            }
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Incoming::Failed(status)) => return Err(connection_error(status)),
                Ok(Incoming::Fragment {
                    opcode,
                    fin,
                    offset,
                    total,
                    bytes,
                }) => {
                    if let Some(message) =
                        self.assembler.push(opcode, fin, offset, total, &bytes)?
                    {
                        return serde_json::from_slice(&message).map_err(|_| "Invalid server JSON");
                    }
                }
                _ => {}
            }
        }
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        // No callback may outlive context. Stop joins the task; destroy releases
        // the event loop and C client. Do not drop this object in its callback.
        unsafe {
            esp_websocket_client_stop(self.handle);
            esp_websocket_client_destroy(self.handle);
        }
    }
}
